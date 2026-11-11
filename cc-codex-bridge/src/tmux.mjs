// Thin wrapper over a dedicated tmux server (`tmux -L <socket>`), so the
// user's own tmux sessions are never touched.
import { execFile } from "node:child_process";
import { writeFileSync } from "node:fs";

export class Tmux {
  constructor(socket) {
    this.socket = socket;
  }

  run(args, { allowFail = false } = {}) {
    return new Promise((resolve, reject) => {
      execFile("tmux", ["-L", this.socket, ...args], { maxBuffer: 16 * 1024 * 1024 }, (err, stdout, stderr) => {
        if (err && !allowFail) reject(new Error(`tmux ${args[0]} failed: ${stderr.trim() || err.message}`));
        else resolve({ ok: !err, stdout, stderr });
      });
    });
  }

  async newSession({ name, cwd, width, height, command }) {
    await this.run(["new-session", "-d", "-s", name, "-x", String(width), "-y", String(height), "-c", cwd, command]);
    // Keep the fixed size even if a human attaches to look.
    await this.run(["set-option", "-t", name, "window-size", "manual"], { allowFail: true });
  }

  async hasSession(name) {
    return (await this.run(["has-session", "-t", `=${name}`], { allowFail: true })).ok;
  }

  async listSessions() {
    const { ok, stdout } = await this.run(["list-sessions", "-F", "#{session_name}"], { allowFail: true });
    return ok ? stdout.split("\n").filter(Boolean) : [];
  }

  killSession(name) {
    return this.run(["kill-session", "-t", `=${name}`], { allowFail: true });
  }

  /** Type literal text; never interpreted as key names. */
  sendLiteral(name, text) {
    return this.run(["send-keys", "-t", `=${name}:`, "-l", "--", text]);
  }

  /** Send a named key such as Enter, Escape, Down. */
  sendKey(name, key) {
    return this.run(["send-keys", "-t", `=${name}:`, key]);
  }

  /** Bracketed paste, so embedded newlines do not submit early. */
  async paste(name, text, scratchFile) {
    writeFileSync(scratchFile, text);
    const buffer = `ccb-${name}`;
    await this.run(["load-buffer", "-b", buffer, scratchFile]);
    await this.run(["paste-buffer", "-p", "-d", "-b", buffer, "-t", `=${name}:`]);
  }

  async capture(name) {
    const { ok, stdout } = await this.run(["capture-pane", "-p", "-t", `=${name}:`], { allowFail: true });
    return ok ? stdout : null;
  }
}

/** Quote one argument for the shell tmux runs the session command with. */
export function shellQuote(arg) {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(arg) ? arg : `'${String(arg).replace(/'/g, `'\\''`)}'`;
}
