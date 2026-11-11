// Persistence in one small SQLite file: the thread -> Claude session map,
// tmux names, tail offsets, the in-progress turn, and finished turns.
import { DatabaseSync } from "node:sqlite";

export class Store {
  constructor(file) {
    this.db = new DatabaseSync(file);
    this.db.exec(`
      PRAGMA journal_mode = WAL;
      CREATE TABLE IF NOT EXISTS threads (
        id TEXT PRIMARY KEY,
        claude_session_id TEXT NOT NULL,
        cwd TEXT NOT NULL,
        tmux_name TEXT NOT NULL,
        events_path TEXT NOT NULL,
        events_offset INTEGER NOT NULL DEFAULT 0,
        transcript_path TEXT,
        transcript_offset INTEGER NOT NULL DEFAULT 0,
        model TEXT,
        name TEXT,
        preview TEXT NOT NULL DEFAULT '',
        created_at INTEGER NOT NULL,
        updated_at INTEGER NOT NULL,
        archived INTEGER NOT NULL DEFAULT 0,
        started INTEGER NOT NULL DEFAULT 0,
        translator_state TEXT
      );
      CREATE TABLE IF NOT EXISTS turns (
        seq INTEGER PRIMARY KEY AUTOINCREMENT,
        thread_id TEXT NOT NULL,
        id TEXT NOT NULL,
        data TEXT NOT NULL
      );
      CREATE INDEX IF NOT EXISTS turns_by_thread ON turns (thread_id, seq);
    `);
  }

  close() {
    this.db.close();
  }

  insertThread(row) {
    this.db
      .prepare(
        `INSERT INTO threads (id, claude_session_id, cwd, tmux_name, events_path, model, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)`,
      )
      .run(row.id, row.claude_session_id, row.cwd, row.tmux_name, row.events_path, row.model ?? null, row.created_at, row.updated_at);
    return this.getThread(row.id);
  }

  getThread(id) {
    return this.db.prepare("SELECT * FROM threads WHERE id = ?").get(id) ?? null;
  }

  listThreads({ archived = false } = {}) {
    return this.db
      .prepare("SELECT * FROM threads WHERE archived = ? ORDER BY updated_at DESC, created_at DESC")
      .all(archived ? 1 : 0);
  }

  updateThread(id, fields) {
    const keys = Object.keys(fields);
    if (keys.length === 0) return;
    const sql = `UPDATE threads SET ${keys.map((key) => `${key} = ?`).join(", ")} WHERE id = ?`;
    this.db.prepare(sql).run(...keys.map((key) => fields[key]), id);
  }

  addTurn(threadId, turn) {
    this.db.prepare("INSERT INTO turns (thread_id, id, data) VALUES (?, ?, ?)").run(threadId, turn.id, JSON.stringify(turn));
  }

  turns(threadId) {
    return this.db
      .prepare("SELECT data FROM turns WHERE thread_id = ? ORDER BY seq")
      .all(threadId)
      .map((row) => JSON.parse(row.data));
  }

  /** Run `fn` atomically. */
  transaction(fn) {
    this.db.exec("BEGIN");
    try {
      const result = fn();
      this.db.exec("COMMIT");
      return result;
    } catch (err) {
      this.db.exec("ROLLBACK");
      throw err;
    }
  }
}
