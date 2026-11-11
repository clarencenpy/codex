// The Codex app-server facade: the only part a Codex client sees. Handles the
// client requests the bridge implements and answers everything else with a
// JSON-RPC method-not-found error.
import { randomUUID } from "node:crypto";
import { execFile } from "node:child_process";
import { existsSync, statSync } from "node:fs";
import path from "node:path";
import { RpcError, ErrorCode } from "./jsonrpc.mjs";
import { Store } from "./store.mjs";
import { Tmux } from "./tmux.mjs";
import { ThreadSession, EngineError } from "./session.mjs";
import { PATTERNS_CLAUDE_VERSION } from "./pane-patterns.mjs";

export const BRIDGE_VERSION = "0.1.0";
const MODEL_PROVIDER = "anthropic";
const TMUX_PREFIX = "ccb-";

export class Bridge {
  constructor(config, { log = () => {} } = {}) {
    this.config = config;
    this.log = log;
    this.store = new Store(path.join(config.stateDir, "bridge.sqlite"));
    this.tmux = new Tmux(config.tmuxSocket);
    this.sessions = new Map(); // threadId -> ThreadSession
    this.subscribers = new Map(); // threadId -> Set<Connection>
    this.approvalRequests = new Map(); // threadId -> { connection, id }
    this.warnings = [];
  }

  /** Reattach to tmux sessions that outlived a previous bridge process. */
  async start() {
    for (const name of await this.tmux.listSessions()) {
      if (!name.startsWith(TMUX_PREFIX)) continue;
      const row = this.store.getThread(name.slice(TMUX_PREFIX.length));
      if (!row || row.archived) {
        this.log(`tmux session ${name} has no live thread; leaving it alone`);
        continue;
      }
      this.session(row).attach();
    }
    await this.checkEngineVersion();
  }

  async checkEngineVersion() {
    const version = await new Promise((resolve) => {
      const [command, ...args] = this.config.engineCommand;
      execFile(command, [...args, "--version"], { timeout: 10_000 }, (err, stdout) => {
        resolve(err ? null : (/\d+\.\d+\.\d+/.exec(stdout)?.[0] ?? null));
      });
    });
    if (version === null) {
      this.warnings.push({
        summary: `Could not run \`${this.config.engineCommand.join(" ")} --version\`.`,
        details: "Check engineCommand in the bridge config.",
      });
    } else if (version !== this.config.testedClaudeVersion) {
      this.warnings.push({
        summary: `Claude Code ${version} is installed; the bridge was tested with ${this.config.testedClaudeVersion}.`,
        details: `Hook payloads, transcript lines and on-screen prompts (patterns recorded for ${PATTERNS_CLAUDE_VERSION}) may differ.`,
      });
    }
    for (const warning of this.warnings) this.log(`warning: ${warning.summary} ${warning.details}`);
  }

  stop() {
    for (const session of this.sessions.values()) session.stopPolling();
    this.store.close();
  }

  session(row) {
    let session = this.sessions.get(row.id);
    if (!session) {
      session = new ThreadSession(
        { config: this.config, store: this.store, tmux: this.tmux, log: this.log, sink: (id, event) => this.onEvent(id, event) },
        row,
      );
      this.sessions.set(row.id, session);
    }
    return session;
  }

  // ---- events from sessions ---------------------------------------------

  onEvent(threadId, event) {
    if (event.type === "notification") {
      this.broadcast(threadId, event.method, event.params);
      if (event.method === "item/completed" && event.params.item.type === "userMessage") {
        const row = this.store.getThread(threadId);
        if (row && !row.preview) this.store.updateThread(threadId, { preview: previewOf(event.params.item.content) });
      }
      return;
    }
    if (event.type === "approval") {
      this.askApproval(threadId);
      return;
    }
    if (event.type === "turnEnded") {
      // An approval still open when its turn ends is over too.
      const open = this.approvalRequests.get(threadId);
      if (open) {
        this.approvalRequests.delete(threadId);
        this.broadcast(threadId, "serverRequest/resolved", { threadId, requestId: open.id });
      }
    }
  }

  broadcast(threadId, method, params) {
    for (const connection of this.subscribers.get(threadId) ?? []) connection.notify(method, params);
  }

  subscribe(threadId, connection) {
    if (!this.subscribers.has(threadId)) this.subscribers.set(threadId, new Set());
    this.subscribers.get(threadId).add(connection);
  }

  /** Send the open approval to a subscribed client and act on its answer. */
  askApproval(threadId) {
    const session = this.sessions.get(threadId);
    const pending = session?.pendingApproval;
    if (!pending || this.approvalRequests.has(threadId)) return;
    const connection = [...(this.subscribers.get(threadId) ?? [])].find((candidate) => !candidate.closed);
    if (!connection) return; // asked again when a client resumes the thread
    const { id, promise } = connection.request(pending.method, pending.params);
    this.approvalRequests.set(threadId, { connection, id });
    promise.then(
      (result) => {
        if (this.approvalRequests.get(threadId)?.id !== id) return;
        this.approvalRequests.delete(threadId);
        return session.answerApproval(result?.decision, id);
      },
      (err) => {
        // The client went away or refused the request: leave the prompt open.
        if (this.approvalRequests.get(threadId)?.id === id) this.approvalRequests.delete(threadId);
        this.log(`[${threadId}] approval request not answered: ${err.message}`);
      },
    ).catch((err) => this.log(`[${threadId}] applying approval failed: ${err.stack ?? err}`));
  }

  connectionClosed(connection) {
    for (const set of this.subscribers.values()) set.delete(connection);
  }

  // ---- protocol objects -------------------------------------------------

  defaultModel() {
    return (this.config.models.find((model) => model.isDefault) ?? this.config.models[0])?.id ?? null;
  }

  threadStatus(row) {
    const session = this.sessions.get(row.id);
    if (!session) return { type: "notLoaded" };
    if (!session.activeTurn && session.queue.length === 0) return { type: "idle" };
    return { type: "active", activeFlags: session.pendingApproval ? ["waitingOnApproval"] : [] };
  }

  threadObject(row, { includeTurns = false } = {}) {
    const session = this.sessions.get(row.id);
    const turns = [];
    if (includeTurns) {
      turns.push(...this.store.turns(row.id));
      if (session?.activeTurn) turns.push(session.translator.publicTurn());
    }
    return {
      id: row.id,
      sessionId: row.claude_session_id,
      forkedFromId: null,
      parentThreadId: null,
      preview: row.preview ?? "",
      ephemeral: false,
      section: null,
      sectionEnteredAt: null,
      projectId: null,
      historyMode: "legacy",
      modelProvider: MODEL_PROVIDER,
      model: row.model ?? this.defaultModel(),
      reasoningEffort: null,
      createdAt: row.created_at,
      updatedAt: row.updated_at,
      recencyAt: null,
      status: this.threadStatus(row),
      path: row.transcript_path ?? null,
      cwd: row.cwd,
      cliVersion: BRIDGE_VERSION,
      originator: "cc-codex-bridge",
      source: "appServer",
      threadSource: null,
      agentNickname: null,
      agentRole: null,
      gitInfo: null,
      name: row.name ?? null,
      turns,
    };
  }

  /** The settings block thread/start and thread/resume both return. */
  threadSettings(row) {
    return {
      // Claude Code's own default when the client names no model.
      model: row.model ?? this.defaultModel() ?? "default",
      modelProvider: MODEL_PROVIDER,
      serviceTier: null,
      disabledPluginIds: [],
      cwd: row.cwd,
      instructionSources: [],
      // Claude Code's permission system, not Codex's, decides what needs
      // approval, and the bridge adds no sandbox of its own.
      approvalPolicy: "on-request",
      approvalsReviewer: "user",
      sandbox: { type: "dangerFullAccess" },
      reasoningEffort: null,
    };
  }

  requireThread(threadId) {
    const row = typeof threadId === "string" ? this.store.getThread(threadId) : null;
    if (!row) throw new RpcError(ErrorCode.InvalidRequest, `thread not found: ${threadId}`);
    return row;
  }

  resolveModel(requested) {
    if (requested === undefined || requested === null) return null;
    // Only models the user listed in the config are passed on to Claude Code.
    if (!this.config.models.some((model) => model.id === requested)) {
      throw new RpcError(ErrorCode.InvalidParams, `model ${JSON.stringify(requested)} is not in the bridge's configured model list`);
    }
    return requested;
  }

  // ---- request handlers -------------------------------------------------

  handlers(connection) {
    const state = { initialized: false };
    const methods = {
      "thread/start": (params) => this.threadStart(connection, params ?? {}),
      "thread/resume": (params) => this.threadResume(connection, params ?? {}),
      "thread/list": (params) => this.threadList(params ?? {}),
      "thread/read": (params) => this.threadRead(params ?? {}),
      "thread/archive": (params) => this.threadArchive(params ?? {}),
      "thread/loaded/list": () => ({ data: [...this.sessions.keys()], nextCursor: null }),
      "thread/unsubscribe": (params) => this.threadUnsubscribe(connection, params ?? {}),
      "thread/name/set": (params) => this.threadSetName(params ?? {}),
      "turn/start": (params) => this.turnStart(connection, params ?? {}),
      "turn/interrupt": (params) => this.turnInterrupt(params ?? {}),
      "model/list": () => this.modelList(),
      // Fixed answers: the bridge has no Codex account and no Codex config.
      "account/read": () => ({ account: null, userId: null, requiresOpenaiAuth: false }),
      "config/read": () => ({ config: {}, origins: {}, layers: null }),
    };
    return {
      onRequest: async (method, params) => {
        if (method === "initialize") {
          if (state.initialized) throw new RpcError(ErrorCode.InvalidRequest, "Already initialized");
          state.initialized = true;
          return {
            userAgent: `cc-codex-bridge/${BRIDGE_VERSION}`,
            codexHome: this.config.stateDir,
            platformFamily: "unix",
            platformOs: process.platform === "darwin" ? "macos" : process.platform,
          };
        }
        if (!state.initialized) throw new RpcError(ErrorCode.InvalidRequest, "Not initialized");
        const handler = methods[method];
        if (!handler) throw new RpcError(ErrorCode.MethodNotFound, `method not found: ${method}`);
        try {
          return await handler(params);
        } catch (err) {
          if (err instanceof EngineError) throw new RpcError(ErrorCode.Internal, err.message);
          throw err;
        }
      },
      onNotification: (method) => {
        if (method === "initialized") {
          for (const warning of this.warnings) {
            connection.notify("configWarning", { summary: warning.summary, details: warning.details });
          }
        }
      },
    };
  }

  async threadStart(connection, params) {
    const cwd = path.resolve(params.cwd ?? process.cwd());
    if (!existsSync(cwd) || !statSync(cwd).isDirectory()) {
      throw new RpcError(ErrorCode.InvalidParams, `cwd is not a directory: ${cwd}`);
    }
    const model = this.resolveModel(params.model);
    const id = randomUUID();
    const now = Math.floor(Date.now() / 1000);
    const row = this.store.insertThread({
      id,
      claude_session_id: randomUUID(),
      cwd,
      tmux_name: `${TMUX_PREFIX}${id}`,
      events_path: path.join(this.config.stateDir, "sessions", id, "events.jsonl"),
      model,
      created_at: now,
      updated_at: now,
    });
    const session = this.session(row);
    try {
      await session.ensureRunning();
    } catch (err) {
      this.sessions.delete(id);
      this.store.updateThread(id, { archived: 1 });
      throw err;
    }
    this.subscribe(id, connection);
    const thread = this.threadObject(this.store.getThread(id));
    setImmediate(() => connection.notify("thread/started", { thread }));
    return { thread, ...this.threadSettings(row) };
  }

  async threadResume(connection, params) {
    let row = this.requireThread(params.threadId);
    if (row.archived) throw new RpcError(ErrorCode.InvalidRequest, `thread is archived: ${row.id}`);
    const session = this.session(row);
    await session.ensureRunning(); // reattaches, or relaunches with --resume
    this.subscribe(row.id, connection);
    row = this.store.getThread(row.id);
    // An approval that was open when the previous client left is asked again.
    setImmediate(() => this.askApproval(row.id));
    return {
      thread: this.threadObject(row, { includeTurns: params.excludeTurns !== true }),
      ...this.threadSettings(row),
      collaborationMode: null,
      turnsBackwardsCursor: null,
      itemsBackwardsCursor: null,
    };
  }

  threadList(params) {
    let rows = this.store.listThreads({ archived: params.archived === true });
    const cwds = params.cwd === undefined || params.cwd === null ? null : [].concat(params.cwd);
    if (cwds) rows = rows.filter((row) => cwds.includes(row.cwd));
    if (params.searchTerm) {
      const term = String(params.searchTerm).toLowerCase();
      rows = rows.filter((row) => `${row.name ?? ""}\n${row.preview ?? ""}`.toLowerCase().includes(term));
    }
    if (Array.isArray(params.excludedThreadIds)) rows = rows.filter((row) => !params.excludedThreadIds.includes(row.id));
    if (params.sortDirection === "asc") rows.reverse();
    const offset = Math.max(0, Number.parseInt(params.cursor ?? "0", 10) || 0);
    const limit = Math.min(Math.max(1, params.limit ?? 50), 200);
    const page = rows.slice(offset, offset + limit);
    return {
      data: page.map((row) => this.threadObject(row)),
      nextCursor: offset + limit < rows.length ? String(offset + limit) : null,
      backwardsCursor: null,
    };
  }

  threadRead(params) {
    const row = this.requireThread(params.threadId);
    return { thread: this.threadObject(row, { includeTurns: params.includeTurns === true }) };
  }

  async threadArchive(params) {
    const row = this.requireThread(params.threadId);
    const session = this.sessions.get(row.id);
    if (session?.activeTurn) session.translator.finishTurn("interrupted");
    // Archiving is the one thing that ends a thread's Claude Code process.
    if (session) await session.kill();
    else await this.tmux.killSession(row.tmux_name);
    if (session) session.persist();
    this.sessions.delete(row.id);
    this.store.updateThread(row.id, { archived: 1 });
    this.broadcast(row.id, "thread/archived", { threadId: row.id });
    this.subscribers.delete(row.id);
    return {};
  }

  threadUnsubscribe(connection, params) {
    this.requireThread(params.threadId);
    if (!this.sessions.has(params.threadId)) return { status: "notLoaded" };
    const set = this.subscribers.get(params.threadId);
    if (!set?.has(connection)) return { status: "notSubscribed" };
    set.delete(connection);
    return { status: "unsubscribed" };
  }

  threadSetName(params) {
    const row = this.requireThread(params.threadId);
    if (typeof params.name !== "string") throw new RpcError(ErrorCode.InvalidParams, "name must be a string");
    this.store.updateThread(row.id, { name: params.name });
    this.broadcast(row.id, "thread/name/updated", { threadId: row.id, threadName: params.name });
    return {};
  }

  async turnStart(connection, params) {
    const row = this.requireThread(params.threadId);
    if (row.archived) throw new RpcError(ErrorCode.InvalidRequest, `thread is archived: ${row.id}`);
    if (!Array.isArray(params.input) || params.input.length === 0) {
      throw new RpcError(ErrorCode.InvalidParams, "input must be a non-empty array");
    }
    const session = this.session(row);
    await session.ensureRunning();
    this.subscribe(row.id, connection);
    return {
      turn: session.submit({
        turnId: randomUUID(),
        input: params.input,
        clientUserMessageId: params.clientUserMessageId ?? null,
      }),
    };
  }

  async turnInterrupt(params) {
    const row = this.requireThread(params.threadId);
    const session = this.sessions.get(row.id);
    if (session) await session.interrupt(params.turnId);
    return {};
  }

  modelList() {
    const fallbackDefault = this.config.models.some((model) => model.isDefault) ? null : this.config.models[0]?.id;
    return {
      data: this.config.models.map((model) => ({
        id: model.id,
        model: model.id,
        upgrade: null,
        upgradeInfo: null,
        availabilityNux: null,
        displayName: model.displayName ?? model.id,
        description: model.description ?? "",
        modelSpecialty: null,
        hidden: false,
        supportedReasoningEfforts: [],
        defaultReasoningEffort: "medium",
        inputModalities: ["text"],
        supportsPersonality: false,
        multiAgentVersion: null,
        additionalSpeedTiers: [],
        serviceTiers: [],
        defaultServiceTier: null,
        availableAccessPrograms: null,
        isDefault: model.isDefault === true || model.id === fallbackDefault,
      })),
      nextCursor: null,
    };
  }
}

function previewOf(input) {
  const text = (input ?? []).find((item) => item?.type === "text")?.text ?? "";
  return String(text).replace(/\s+/g, " ").trim().slice(0, 200);
}
