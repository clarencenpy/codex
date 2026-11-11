// One Claude Code process per Codex thread, in its own tmux session. This is
// the only writer to that process: prompts, approval keys and interrupts all
// go through here, and hook events and transcript lines come back through
// here into the translator.
import { existsSync, writeFileSync, statSync, mkdirSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { Translator } from "./translator.mjs";
import { readLinesFrom } from "./tailer.mjs";
import { shellQuote } from "./tmux.mjs";
import {
  findPermissionPrompt,
  chooseOption,
  findTrustPrompt,
  findBlockingScreen,
  showsInterrupted,
} from "./pane-patterns.mjs";

const HOOK_SCRIPT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..", "hooks", "ccb-hook.mjs");

// Hook events the bridge listens to. Names verified against Claude Code
// 2.1.322; an event name a different version does not know is ignored by it.
export const HOOK_EVENTS = [
  "SessionStart",
  "UserPromptSubmit",
  "PreToolUse",
  "PostToolUse",
  "PostToolUseFailure",
  "PermissionRequest",
  "Notification",
  "Stop",
  "StopFailure",
  "SessionEnd",
];

/** How long the stop hook waits for the transcript to show the final message. */
const STOP_GRACE_MS = 1500;
/** How long a sent prompt may go unacknowledged by the UserPromptSubmit hook. */
const SUBMIT_TIMEOUT_MS = 15_000;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

export class EngineError extends Error {}

export class ThreadSession {
  /**
   * @param {object} deps { config, store, tmux, log, sink }
   *   sink(threadId, event) receives translator events.
   * @param {object} row the thread's row from the store
   */
  constructor({ config, store, tmux, log, sink }, row) {
    this.config = config;
    this.store = store;
    this.tmux = tmux;
    this.log = (message) => log(`[${row.id}] ${message}`);
    this.sink = sink;
    this.id = row.id;
    this.cwd = row.cwd;
    this.tmuxName = row.tmux_name;
    this.claudeSessionId = row.claude_session_id;
    this.eventsPath = row.events_path;
    this.eventsOffset = row.events_offset;
    this.transcriptPath = row.transcript_path;
    this.transcriptOffset = row.transcript_offset;
    this.model = row.model;
    this.started = row.started === 1;
    this.ready = false; // SessionStart seen for the running process
    this.queue = []; // turns waiting for the session to be idle
    this.awaitingSubmit = null; // deadline for the UserPromptSubmit hook
    this.lastActivity = Date.now();
    this.lastPaneCheck = 0;
    this.stopSeenAt = null;
    this.timer = null;
    this.ticking = false;
    this.launching = null;
    this.translator = new Translator({
      threadId: this.id,
      cwd: this.cwd,
      emit: (event) => this.onTranslatorEvent(event),
      fileExists: existsSync,
      state: row.translator_state ? JSON.parse(row.translator_state) : undefined,
    });
  }

  get sessionDir() {
    return path.dirname(this.eventsPath);
  }

  get activeTurn() {
    return this.translator.turn;
  }

  get pendingApproval() {
    return this.translator.turn?.approval ?? null;
  }

  // ---- lifecycle --------------------------------------------------------

  /** Attach to an already running tmux session (after a bridge restart). */
  attach() {
    this.ready = true;
    this.startPolling();
  }

  /** Make sure the engine is running; start or resume it if not. */
  ensureRunning() {
    this.launching ??= this.launch().finally(() => (this.launching = null));
    return this.launching;
  }

  async launch() {
    if (await this.tmux.hasSession(this.tmuxName)) {
      this.ready = true;
      this.startPolling();
      return;
    }
    mkdirSync(this.sessionDir, { recursive: true });
    const settingsPath = path.join(this.sessionDir, "settings.json");
    const hookCommand = `${shellQuote(process.execPath)} ${shellQuote(HOOK_SCRIPT)} ${shellQuote(this.eventsPath)}`;
    const hooks = Object.fromEntries(
      HOOK_EVENTS.map((event) => [event, [{ hooks: [{ type: "command", command: hookCommand }] }]]),
    );
    writeFileSync(settingsPath, JSON.stringify({ hooks }, null, 2));
    if (!existsSync(this.eventsPath)) writeFileSync(this.eventsPath, "");

    // Flags verified against Claude Code 2.1.322: --settings <file>,
    // --session-id <uuid> for a new conversation, --resume <id> to continue.
    const args = [...this.config.engineCommand, "--settings", settingsPath];
    args.push(this.started ? "--resume" : "--session-id", this.claudeSessionId);
    if (this.model) args.push("--model", this.model);
    this.ready = false;
    await this.tmux.newSession({
      name: this.tmuxName,
      cwd: this.cwd,
      width: this.config.paneWidth,
      height: this.config.paneHeight,
      command: args.map(shellQuote).join(" "),
    });
    this.startPolling();
    try {
      await this.waitUntilReady();
    } catch (err) {
      await this.tmux.killSession(this.tmuxName);
      throw err;
    }
    this.started = true;
    this.store.updateThread(this.id, { started: 1 });
  }

  async waitUntilReady() {
    const deadline = Date.now() + this.config.startupTimeoutMs;
    let trustAnswered = false;
    while (Date.now() < deadline) {
      if (this.ready) return;
      const pane = await this.tmux.capture(this.tmuxName);
      if (pane === null) throw new EngineError("the engine exited before its session started; check engineCommand");
      const trust = findTrustPrompt(pane);
      if (trust && !trustAnswered) {
        if (!this.config.autoTrustWorkspace) {
          throw new EngineError(
            `Claude Code asks whether to trust ${this.cwd}. Run \`claude\` there once and accept, or set "autoTrustWorkspace": true in the bridge config.`,
          );
        }
        // Move the cursor onto the option by its text, then confirm.
        const selected = trust.choices.findIndex((choice) => choice.selected);
        const wanted = trust.choices.findIndex((choice) => choice.text === "Yes, I trust this folder");
        const key = wanted > selected ? "Down" : "Up";
        for (let i = 0; i < Math.abs(wanted - selected); i++) await this.tmux.sendKey(this.tmuxName, key);
        await sleep(200);
        const after = findTrustPrompt((await this.tmux.capture(this.tmuxName)) ?? "");
        if (after && after.choices.find((choice) => choice.selected)?.text !== "Yes, I trust this folder") {
          throw new EngineError("could not select the trust option on screen; refusing to guess a key");
        }
        if (after) await this.tmux.sendKey(this.tmuxName, "Enter");
        trustAnswered = true;
      } else if (!trust) {
        const blocking = findBlockingScreen(pane);
        if (blocking) throw new EngineError(blocking);
      }
      await sleep(300);
    }
    throw new EngineError("timed out waiting for Claude Code to start its session");
  }

  startPolling() {
    if (this.timer) return;
    this.timer = setInterval(() => this.tick().catch((err) => this.log(`tick failed: ${err.stack ?? err}`)), this.config.pollIntervalMs);
    this.timer.unref?.();
  }

  stopPolling() {
    clearInterval(this.timer);
    this.timer = null;
  }

  async kill() {
    this.stopPolling();
    await this.tmux.killSession(this.tmuxName);
    this.ready = false;
  }

  // ---- event pump -------------------------------------------------------

  onTranslatorEvent(event) {
    if (event.type === "log") {
      this.log(event.message);
      return;
    }
    if (event.type === "session") {
      if (event.transcriptPath !== this.transcriptPath) {
        // A transcript file not seen before: skip what it already holds
        // (history copied in by a resume) so nothing is replayed.
        const known = this.transcriptPath !== null;
        this.transcriptPath = event.transcriptPath;
        this.transcriptOffset = known && existsSync(event.transcriptPath) ? statSync(event.transcriptPath).size : 0;
      }
      this.claudeSessionId = event.sessionId;
      return;
    }
    if (event.type === "turnEnded") {
      this.store.addTurn(this.id, event.turn);
      this.awaitingSubmit = null;
      this.stopSeenAt = null;
    }
    this.sink(this.id, event);
  }

  persist() {
    this.store.updateThread(this.id, {
      events_offset: this.eventsOffset,
      transcript_path: this.transcriptPath,
      transcript_offset: this.transcriptOffset,
      claude_session_id: this.claudeSessionId,
      translator_state: JSON.stringify(this.translator.snapshot()),
      updated_at: Math.floor(Date.now() / 1000),
    });
  }

  /** Read both sources, merge by timestamp, feed the translator, persist. */
  async tick() {
    if (this.ticking) return;
    this.ticking = true;
    try {
      const hooks = readLinesFrom(this.eventsPath, this.eventsOffset).lines.map((line) => {
        const parsed = parseJson(line.text);
        return { source: "hook", end: line.end, ts: Number(parsed?.ts) || 0, payload: parsed?.payload };
      });
      let lastTs = 0;
      const transcript = this.transcriptPath
        ? readLinesFrom(this.transcriptPath, this.transcriptOffset).lines.map((line) => {
            const parsed = parseJson(line.text);
            const ts = Date.parse(parsed?.timestamp ?? "");
            if (Number.isFinite(ts)) lastTs = ts;
            return { source: "transcript", end: line.end, ts: lastTs, payload: parsed };
          })
        : [];
      // Stable sort: on a tie, hooks first.
      const merged = [...hooks, ...transcript].sort((a, b) => a.ts - b.ts);
      const before = `${this.eventsOffset}:${this.transcriptOffset}`;
      this.store.transaction(() => {
        for (const entry of merged) {
          if (entry.payload === undefined || entry.payload === null) {
            this.log(`skipping unparseable ${entry.source} line`);
          } else if (entry.source === "hook") {
            if (!this.acceptHook(entry.payload)) break; // retried next tick
          } else {
            this.translator.onTranscriptLine(entry.payload);
          }
          if (entry.source === "hook") this.eventsOffset = entry.end;
          else this.transcriptOffset = entry.end;
        }
        if (before !== `${this.eventsOffset}:${this.transcriptOffset}`) {
          this.lastActivity = Date.now();
          this.persist();
        }
      });
      await this.watchdog();
      await this.dispatch();
    } finally {
      this.ticking = false;
    }
  }

  /** Returns false to leave the hook unconsumed until the next tick. */
  acceptHook(payload) {
    const name = payload.hook_event_name;
    if (name === "SessionStart") this.ready = true;
    if (name === "UserPromptSubmit") this.awaitingSubmit = null;
    if (name === "Stop" && this.activeTurn && !this.translator.hasAgentMessage(payload.last_assistant_message)) {
      // The transcript may trail the stop hook; give it a moment to show the
      // final message before taking the text from the hook payload instead.
      this.stopSeenAt ??= Date.now();
      if (Date.now() - this.stopSeenAt < STOP_GRACE_MS) return false;
    }
    this.translator.onHook(payload);
    return true;
  }

  /** Pane fallback: what no hook reports. */
  async watchdog() {
    const turn = this.activeTurn;
    const now = Date.now();
    if (!turn) return;
    if (this.awaitingSubmit !== null && now > this.awaitingSubmit) {
      this.awaitingSubmit = null;
      const pane = (await this.tmux.capture(this.tmuxName)) ?? "";
      this.translator.finishTurn("failed", findBlockingScreen(pane) ?? "Claude Code did not accept the prompt");
      this.persist();
      return;
    }
    if (now - this.lastActivity < this.config.paneCheckAfterMs || now - this.lastPaneCheck < this.config.paneCheckAfterMs) return;
    this.lastPaneCheck = now;
    const pane = await this.tmux.capture(this.tmuxName);
    if (!this.activeTurn) return;
    if (pane === null) {
      this.ready = false;
      this.translator.finishTurn("failed", "Claude Code exited while the turn was running");
      this.persist();
      return;
    }
    const blocking = findBlockingScreen(pane);
    if (blocking) {
      this.translator.finishTurn("failed", blocking);
      this.persist();
      return;
    }
    if (!this.pendingApproval && findPermissionPrompt(pane)) {
      this.log("permission prompt found on screen with no hook report");
      this.translator.permissionRequested(null, null);
      this.persist();
    }
  }

  // ---- prompts ----------------------------------------------------------

  /**
   * Queue a turn. Returns the Turn the client is told about; it starts when
   * the session is idle.
   */
  submit({ turnId, input, clientUserMessageId }) {
    this.queue.push({ turnId, input, clientUserMessageId, cancelled: false });
    return {
      id: turnId,
      rootTurnId: null,
      items: [],
      itemsView: "full",
      status: "inProgress",
      error: null,
      startedAt: null,
      completedAt: null,
      durationMs: null,
    };
  }

  /** Send the next queued prompt once the session is idle. */
  async dispatch() {
    if (!this.ready || this.activeTurn || this.queue.length === 0 || this.dispatching) return;
    this.dispatching = true;
    try {
      const next = this.queue.shift();
      this.translator.startTurn(next);
      if (next.cancelled) {
        this.translator.finishTurn("interrupted");
        this.persist();
        return;
      }
      this.persist();
      const text = promptText(next.input);
      try {
        // Always bracketed paste, so newlines never submit early.
        await this.tmux.paste(this.tmuxName, text, path.join(this.sessionDir, "prompt.txt"));
        await sleep(150);
        await this.tmux.sendKey(this.tmuxName, "Enter");
        this.awaitingSubmit = Date.now() + SUBMIT_TIMEOUT_MS;
        this.lastActivity = Date.now();
      } catch (err) {
        this.translator.finishTurn("failed", `could not send the prompt to Claude Code: ${err.message}`);
        this.persist();
      }
    } finally {
      this.dispatching = false;
    }
  }

  // ---- approvals --------------------------------------------------------

  /**
   * Apply the client's answer to the open permission prompt by pressing the
   * key of the option whose text matches. Never guesses: if the prompt or a
   * matching option is not on screen, the turn fails.
   */
  async answerApproval(decision, requestId) {
    if (!this.pendingApproval) return;
    const choice = normaliseDecision(decision);
    let prompt = null;
    for (let attempt = 0; attempt < 20 && !prompt; attempt++) {
      prompt = findPermissionPrompt((await this.tmux.capture(this.tmuxName)) ?? "");
      if (!prompt) await sleep(150);
    }
    if (!this.pendingApproval) return; // the turn moved on while waiting
    if (!prompt) {
      this.translator.approvalAnswered("cancel", requestId);
      this.translator.finishTurn("failed", "the permission prompt was not found on screen; no key was sent");
      this.persist();
      return;
    }
    if (choice === "cancel") {
      await this.tmux.sendKey(this.tmuxName, "Escape");
    } else {
      const option = chooseOption(prompt.options, choice);
      if (!option) {
        this.translator.approvalAnswered("cancel", requestId);
        this.translator.finishTurn(
          "failed",
          `no option matching "${choice}" in the permission prompt (${prompt.options.map((o) => o.text).join(" / ")}); no key was sent`,
        );
        this.persist();
        return;
      }
      await this.tmux.sendLiteral(this.tmuxName, option.key);
    }
    this.lastActivity = Date.now();
    this.translator.approvalAnswered(choice, requestId);
    this.persist();
  }

  // ---- interrupt --------------------------------------------------------

  /** Returns once the turn has ended; throws if that cannot be confirmed. */
  async interrupt(turnId) {
    const queued = this.queue.find((entry) => entry.turnId === turnId);
    if (queued) {
      queued.cancelled = true;
      return;
    }
    const turn = this.activeTurn;
    if (!turn || turn.id !== turnId) return; // already over
    // Escape is Claude Code's interrupt key (verified on 2.1.322). It writes
    // "[Request interrupted by user]" to the transcript and fires no stop hook.
    await this.tmux.sendKey(this.tmuxName, "Escape");
    const deadline = Date.now() + this.config.interruptTimeoutMs;
    while (Date.now() < deadline) {
      await sleep(100);
      if (this.activeTurn?.id !== turnId) return;
    }
    const pane = (await this.tmux.capture(this.tmuxName)) ?? "";
    if (this.activeTurn?.id !== turnId) return;
    if (showsInterrupted(pane)) {
      this.translator.finishTurn("interrupted");
      this.persist();
      return;
    }
    throw new EngineError("Claude Code did not confirm the interrupt in time; the turn is still running");
  }
}

function parseJson(text) {
  try {
    return JSON.parse(text);
  } catch {
    return undefined;
  }
}

function normaliseDecision(decision) {
  if (decision === "accept" || decision === "acceptForSession" || decision === "decline" || decision === "cancel") {
    return decision;
  }
  // Codex-specific amendments (exec policy, network policy) have no Claude
  // Code equivalent; treat them as a one-off accept.
  if (decision && typeof decision === "object") return "accept";
  return "decline";
}

/** Flatten Codex UserInput items into the text typed into Claude Code. */
export function promptText(input) {
  const parts = [];
  for (const item of input ?? []) {
    if (item?.type === "text") parts.push(String(item.text ?? ""));
    else if (item?.type === "localImage" || item?.type === "localAudio") parts.push(String(item.path ?? ""));
    else if (item?.type === "mention" || item?.type === "skill") parts.push(`@${item.path ?? item.name ?? ""}`);
    else if (item?.type === "image" && typeof item.url === "string" && !item.url.startsWith("data:")) parts.push(item.url);
    else parts.push(`[unsupported ${item?.type ?? "input"} attachment omitted]`);
  }
  let text = parts.join("\n").replace(/\r\n?/g, "\n").trim();
  // A leading "/", "!" or "#" would switch Claude Code's input into command,
  // shell or memory mode. A prompt from the Codex client is always plain
  // text, so keep it that way.
  if (/^[/!#]/.test(text)) text = ` ${text}`;
  return text;
}
