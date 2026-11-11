// Translate Claude Code events (hook payloads, transcript lines) into Codex
// thread/turn/item messages for one thread.
//
// The translator is a pure state machine: it does no I/O, takes its clock
// from the caller, and keeps all of its state in one JSON-serialisable object
// so a restarted bridge can pick a turn up where it left off.
//
// Invariants it enforces (see test/translator.test.mjs):
//   - an item is started before it completes, and completes at most once;
//   - no delta follows a completed item;
//   - every turn ends with exactly one terminal turn/completed notification;
//   - nothing is emitted for a turn after its terminal notification.
import {
  startToolItem,
  completeToolItem,
  abandonToolItem,
  userMessageItem,
  agentMessageItem,
  toolKind,
  describeTool,
} from "./items.mjs";

const INTERRUPT_MARKER = "[Request interrupted by user";

export function emptyState() {
  return { turn: null, seq: 0 };
}

export class Translator {
  /**
   * @param {object} opts
   * @param {string} opts.threadId
   * @param {string} opts.cwd
   * @param {(event: object) => void} opts.emit  receives
   *   { type: "notification", method, params }
   *   { type: "approval", method, params }       ask the client, hold open
   *   { type: "turnEnded", turn }                 terminal, after turn/completed
   *   { type: "session", sessionId, transcriptPath }
   *   { type: "log", message }
   * @param {() => number} [opts.now] milliseconds
   * @param {(path: string) => boolean} [opts.fileExists]
   * @param {() => string} [opts.newId]
   * @param {object} [opts.state] a previous snapshot()
   */
  constructor({ threadId, cwd, emit, now = Date.now, fileExists = () => true, newId, state }) {
    this.threadId = threadId;
    this.cwd = cwd;
    this.emit = emit;
    this.now = now;
    this.fileExists = fileExists;
    this.state = state ? structuredClone(state) : emptyState();
    this.newId = newId ?? (() => `${this.threadId}-${++this.state.seq}`);
  }

  snapshot() {
    return structuredClone(this.state);
  }

  get turn() {
    return this.state.turn;
  }

  // ---- outgoing helpers -------------------------------------------------

  notify(method, params) {
    this.emit({ type: "notification", method, params });
  }

  log(message) {
    this.emit({ type: "log", message });
  }

  publicTurn(turn = this.turn) {
    return {
      id: turn.id,
      rootTurnId: null,
      items: turn.items,
      itemsView: "full",
      status: turn.status,
      error: turn.error,
      startedAt: turn.startedAt,
      completedAt: turn.completedAt,
      durationMs: turn.durationMs,
    };
  }

  setStatus(flags) {
    const status = flags === null ? { type: "idle" } : { type: "active", activeFlags: flags };
    this.notify("thread/status/changed", { threadId: this.threadId, status });
  }

  itemStarted(item) {
    const turn = this.turn;
    turn.items.push(item);
    turn.open[item.id] = true;
    this.notify("item/started", { item, threadId: this.threadId, turnId: turn.id, startedAtMs: this.now() });
  }

  itemCompleted(item) {
    const turn = this.turn;
    if (!turn.open[item.id]) {
      this.log(`ignoring completion of item ${item.id}: not open`);
      return;
    }
    delete turn.open[item.id];
    const index = turn.items.findIndex((existing) => existing.id === item.id);
    turn.items[index] = item;
    this.notify("item/completed", { item, threadId: this.threadId, turnId: turn.id, completedAtMs: this.now() });
  }

  findItem(id) {
    return this.turn?.items.find((item) => item.id === id);
  }

  // ---- turn lifecycle ---------------------------------------------------

  /**
   * Begin a turn. `input` is the Codex UserInput array for a turn the client
   * asked for; omit it for a turn Claude Code started by itself (for example
   * when a background task reports back).
   */
  startTurn({ turnId, input, clientUserMessageId = null } = {}) {
    if (this.turn) throw new Error(`thread ${this.threadId} already has turn ${this.turn.id} in progress`);
    const nowMs = this.now();
    this.state.turn = {
      id: turnId ?? this.newId(),
      status: "inProgress",
      error: null,
      startedAt: Math.floor(nowMs / 1000),
      startedAtMs: nowMs,
      completedAt: null,
      durationMs: null,
      items: [],
      open: {},
      seen: {}, // transcript line uuids already handled
      tools: {}, // tool-call id -> { toolName, input }
      approval: null, // { itemId, method, params } while a permission prompt is open
      declined: null, // item id the client declined
    };
    this.notify("turn/started", { threadId: this.threadId, turn: this.publicTurn() });
    this.setStatus([]);
    if (input) {
      const item = userMessageItem(this.newId(), input, clientUserMessageId);
      this.itemStarted(item);
      this.itemCompleted(item);
    }
    return this.publicTurn();
  }

  /** End the turn: close open items, then exactly one turn/completed. */
  finishTurn(status, errorMessage = null) {
    const turn = this.turn;
    if (!turn) return null;
    for (const id of Object.keys(turn.open)) {
      const item = this.findItem(id);
      if (item.type === "agentMessage" || item.type === "userMessage") {
        this.itemCompleted(item);
        continue;
      }
      const closeAs = id === turn.declined ? "declined" : status === "completed" ? "completed" : "failed";
      this.itemCompleted(abandonToolItem(item, closeAs));
    }
    const nowMs = this.now();
    turn.status = status;
    turn.completedAt = Math.floor(nowMs / 1000);
    turn.durationMs = Math.max(0, nowMs - turn.startedAtMs);
    turn.approval = null;
    if (status === "failed") {
      turn.error = { message: errorMessage ?? "turn failed", codexErrorInfo: null, additionalDetails: null, misalignment: null };
      this.notify("error", { error: turn.error, willRetry: false, threadId: this.threadId, turnId: turn.id });
    }
    const finished = this.publicTurn(turn);
    this.state.turn = null;
    this.notify("turn/completed", { threadId: this.threadId, turn: finished });
    this.setStatus(null);
    this.emit({ type: "turnEnded", turn: finished });
    return finished;
  }

  ensureTurn(reason) {
    if (!this.turn) {
      this.log(`starting a turn the client did not ask for (${reason})`);
      this.startTurn({});
    }
  }

  // ---- hooks ------------------------------------------------------------

  onHook(payload) {
    if (!payload || typeof payload !== "object") return;
    const name = payload.hook_event_name;
    if (typeof payload.session_id === "string" && typeof payload.transcript_path === "string") {
      this.emit({ type: "session", sessionId: payload.session_id, transcriptPath: payload.transcript_path });
    }
    switch (name) {
      case "SessionStart":
        return;
      case "UserPromptSubmit":
        // A prompt the bridge did not send (for example a background task
        // reporting back) starts a turn of its own.
        if (!this.turn) this.ensureTurn("prompt submitted outside a client turn");
        return;
      case "PreToolUse":
        this.ensureTurn("tool call outside a client turn");
        this.toolStarted(payload.tool_use_id, payload.tool_name, payload.tool_input);
        return;
      case "PostToolUse":
        if (!this.turn) return;
        this.toolFinished(payload.tool_use_id, {
          ok: true,
          response: payload.tool_response,
          text: resultText(payload.tool_response),
          durationMs: numberOrNull(payload.duration_ms),
        });
        return;
      case "PostToolUseFailure":
        if (!this.turn) return;
        this.toolFinished(payload.tool_use_id, {
          ok: false,
          response: null,
          text: payload.error ?? payload.tool_response ?? "",
          durationMs: numberOrNull(payload.duration_ms),
        });
        return;
      case "PermissionRequest":
        this.ensureTurn("permission request outside a client turn");
        this.permissionRequested(payload.tool_name, payload.tool_input);
        return;
      case "Notification":
        // Fallback for a client that fires no PermissionRequest hook.
        if (this.turn && payload.notification_type === "permission_prompt" && !this.turn.approval) {
          this.permissionRequested(null, null);
        }
        return;
      case "Stop":
        if (!this.turn) return;
        this.stopped(payload.last_assistant_message);
        return;
      case "StopFailure":
        if (!this.turn) return;
        this.finishTurn("failed", describeFailure(payload));
        return;
      case "SessionEnd":
        if (this.turn) this.finishTurn("failed", `Claude Code session ended (${payload.reason ?? "unknown reason"})`);
        return;
      default:
        this.log(`unhandled hook event ${JSON.stringify(name)}`);
    }
  }

  toolStarted(toolUseId, toolName, input) {
    if (typeof toolUseId !== "string" || this.findItem(toolUseId)) return; // de-duplicate by tool-call id
    this.itemStarted(startToolItem({ id: toolUseId, toolName, input, cwd: this.cwd, fileExists: this.fileExists }));
    this.turn.tools[toolUseId] = { toolName, input };
  }

  toolFinished(toolUseId, outcome) {
    const item = this.findItem(toolUseId);
    if (!item || !this.turn.open[toolUseId]) return; // unknown or already completed
    if (this.turn.approval?.itemId === toolUseId) this.turn.approval = null;
    if (this.turn.declined === toolUseId) {
      // Claude Code records a refused tool call as a failed result.
      this.itemCompleted(abandonToolItem(item, "declined"));
      return;
    }
    this.itemCompleted(completeToolItem(item, outcome));
  }

  permissionRequested(toolName, input) {
    const turn = this.turn;
    if (turn.approval) return;
    // The hook carries no tool-call id: match the newest open tool item, by
    // tool name and input when the hook gives them.
    const openTools = turn.items.filter((item) => turn.open[item.id] && turn.tools[item.id]);
    let item = openTools.findLast(
      (candidate) =>
        toolName === null ||
        (turn.tools[candidate.id].toolName === toolName && sameJson(turn.tools[candidate.id].input, input)),
    );
    item ??= openTools.at(-1);
    if (!item) {
      if (toolName === null) {
        this.log("permission prompt reported but no tool call is open");
        return;
      }
      const id = this.newId();
      this.toolStarted(id, toolName, input);
      item = this.findItem(id);
    }
    // The generic "needs your permission" notification can trail the
    // PermissionRequest hook by seconds; never ask twice for one tool call.
    turn.asked ??= {};
    if (turn.asked[item.id]) return;
    turn.asked[item.id] = true;
    const tool = turn.tools[item.id];
    const base = { threadId: this.threadId, turnId: turn.id, itemId: item.id, startedAtMs: this.now() };
    let method = "item/commandExecution/requestApproval";
    let params;
    if (toolKind(tool.toolName) === "file") {
      method = "item/fileChange/requestApproval";
      params = { ...base, reason: `Claude Code wants to change ${item.changes?.[0]?.path ?? "a file"}` };
    } else {
      const isCommand = toolKind(tool.toolName) === "command";
      params = {
        ...base,
        kind: "command",
        environmentId: null,
        command: describeTool(tool.toolName, tool.input),
        cwd: this.cwd,
        reason: isCommand
          ? (typeof tool.input?.description === "string" ? tool.input.description : null)
          : `Claude Code wants to use the ${tool.toolName} tool`,
      };
    }
    // Kept in the state so a restarted bridge can ask the client again.
    turn.approval = { itemId: item.id, method, params };
    this.setStatus(["waitingOnApproval"]);
    this.emit({ type: "approval", method, params });
  }

  /** The client answered the open approval and the key reached Claude Code. */
  approvalAnswered(decision, requestId) {
    const turn = this.turn;
    if (!turn?.approval) return;
    if (decision === "decline" || decision === "cancel") turn.declined = turn.approval.itemId;
    turn.approval = null;
    if (requestId !== undefined) this.notify("serverRequest/resolved", { threadId: this.threadId, requestId });
    this.setStatus([]);
  }

  stopped(lastAssistantMessage) {
    const turn = this.turn;
    // The transcript is written in chunks and can trail the stop hook. If the
    // final message has not been seen yet, take it from the hook payload.
    if (typeof lastAssistantMessage === "string" && lastAssistantMessage.trim() !== "") {
      const have = turn.items.some(
        (item) => item.type === "agentMessage" && item.text.trim() === lastAssistantMessage.trim(),
      );
      if (!have) this.agentMessage(this.newId(), lastAssistantMessage);
    }
    this.finishTurn("completed");
  }

  /** True when the stop hook's final message is already in the turn. */
  hasAgentMessage(text) {
    if (typeof text !== "string" || text.trim() === "") return true;
    return Boolean(this.turn?.items.some((item) => item.type === "agentMessage" && item.text.trim() === text.trim()));
  }

  agentMessage(id, text) {
    // Whole messages only: the transcript has no token stream. One delta
    // carries the full text so clients that render from deltas still work.
    this.itemStarted(agentMessageItem(id, ""));
    this.notify("item/agentMessage/delta", { threadId: this.threadId, turnId: this.turn.id, itemId: id, delta: text });
    this.itemCompleted(agentMessageItem(id, text));
  }

  // ---- transcript -------------------------------------------------------

  onTranscriptLine(line) {
    if (!line || typeof line !== "object" || line.isSidechain === true) return;
    const turn = this.turn;
    if (!turn) return; // between turns: nothing to attach it to
    if (typeof line.uuid === "string") {
      if (turn.seen[line.uuid]) return;
      turn.seen[line.uuid] = true;
    }
    const content = line.message?.content;
    if (line.type === "assistant" && Array.isArray(content)) {
      content.forEach((block, index) => {
        if (block?.type === "text" && typeof block.text === "string" && block.text.trim() !== "") {
          this.agentMessage(`${line.uuid ?? this.newId()}-${index}`, block.text);
        } else if (block?.type === "tool_use") {
          this.toolStarted(block.id, block.name, block.input); // no-op when the hook got there first
        }
      });
      return;
    }
    if (line.type === "user" && Array.isArray(content)) {
      for (const block of content) {
        if (block?.type === "tool_result") {
          this.toolFinished(block.tool_use_id, {
            ok: block.is_error !== true,
            response: block.is_error === true ? null : line.toolUseResult,
            text: block.content,
            durationMs: null,
          });
        } else if (block?.type === "text" && typeof block.text === "string" && block.text.startsWith(INTERRUPT_MARKER)) {
          this.finishTurn("interrupted");
          return;
        }
      }
      return;
    }
    if (line.type === "user" && typeof content === "string" && content.startsWith(INTERRUPT_MARKER)) {
      this.finishTurn("interrupted");
    }
  }
}

function numberOrNull(value) {
  return typeof value === "number" && Number.isFinite(value) ? Math.round(value) : null;
}

function sameJson(a, b) {
  try {
    return JSON.stringify(a) === JSON.stringify(b);
  } catch {
    return false;
  }
}

function resultText(response) {
  if (typeof response === "string") return response;
  if (response && typeof response === "object") {
    if (typeof response.stdout === "string") return [response.stdout, response.stderr].filter(Boolean).join("\n");
    if (typeof response.content === "string") return response.content;
  }
  return response === undefined || response === null ? "" : JSON.stringify(response);
}

function describeFailure(payload) {
  const detail = payload.error ?? payload.message ?? payload.reason;
  return detail ? `Claude Code stopped with an error: ${typeof detail === "string" ? detail : JSON.stringify(detail)}` : "Claude Code stopped with an error";
}
