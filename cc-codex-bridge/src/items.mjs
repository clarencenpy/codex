// Build Codex ThreadItem objects from Claude Code tool calls. Field names
// follow the generated app-server schema (codex-rs/app-server-protocol).

const FILE_TOOLS = new Set(["Edit", "Write", "MultiEdit", "NotebookEdit"]);

export function toolKind(toolName) {
  if (toolName === "Bash") return "command";
  if (FILE_TOOLS.has(toolName)) return "file";
  if (typeof toolName === "string" && toolName.startsWith("mcp__")) return "mcp";
  return "other";
}

function asText(value) {
  if (typeof value === "string") return value;
  if (value === undefined || value === null) return "";
  if (Array.isArray(value)) {
    return value.map((part) => (part && typeof part === "object" && typeof part.text === "string" ? part.text : asText(part))).join("\n");
  }
  try {
    return JSON.stringify(value);
  } catch {
    return String(value);
  }
}

function hunk(oldLines, newLines) {
  const header = `@@ -${oldLines.length ? 1 : 0},${oldLines.length} +${newLines.length ? 1 : 0},${newLines.length} @@`;
  return [header, ...oldLines.map((l) => `-${l}`), ...newLines.map((l) => `+${l}`)].join("\n") + "\n";
}

function splitLines(text) {
  if (typeof text !== "string" || text === "") return [];
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  return lines;
}

function patchToDiff(structuredPatch) {
  return structuredPatch
    .map((h) => `@@ -${h.oldStart},${h.oldLines} +${h.newStart},${h.newLines} @@\n${(h.lines ?? []).join("\n")}\n`)
    .join("");
}

/** Proposed changes, from the tool input alone (before the tool has run). */
function proposedChanges(toolName, input, fileExists) {
  const filePath = String(input?.file_path ?? input?.notebook_path ?? "");
  if (toolName === "Write") {
    const content = asText(input?.content);
    return fileExists(filePath)
      ? [{ path: filePath, kind: { type: "update", move_path: null }, diff: hunk([], splitLines(content)) }]
      : [{ path: filePath, kind: { type: "add" }, diff: content }];
  }
  if (toolName === "MultiEdit" && Array.isArray(input?.edits)) {
    const diff = input.edits.map((e) => hunk(splitLines(e?.old_string), splitLines(e?.new_string))).join("");
    return [{ path: filePath, kind: { type: "update", move_path: null }, diff }];
  }
  if (toolName === "NotebookEdit") {
    return [{ path: filePath, kind: { type: "update", move_path: null }, diff: hunk([], splitLines(asText(input?.new_source))) }];
  }
  return [
    {
      path: filePath,
      kind: { type: "update", move_path: null },
      diff: hunk(splitLines(input?.old_string), splitLines(input?.new_string)),
    },
  ];
}

/** Item for a tool call that has started. */
export function startToolItem({ id, toolName, input, cwd, fileExists = () => true }) {
  const kind = toolKind(toolName);
  if (kind === "command") {
    const command = asText(input?.command);
    return {
      type: "commandExecution",
      id,
      pluginId: null,
      scriptPath: null,
      command,
      cwd,
      processId: null,
      source: "agent",
      status: "inProgress",
      commandActions: [{ type: "unknown", command }],
      aggregatedOutput: null,
      exitCode: null,
      durationMs: null,
    };
  }
  if (kind === "file") {
    return { type: "fileChange", id, changes: proposedChanges(toolName, input, fileExists), status: "inProgress" };
  }
  if (kind === "mcp") {
    const [, server = "", ...rest] = toolName.split("__");
    return {
      type: "mcpToolCall",
      id,
      server,
      tool: rest.join("__"),
      status: "inProgress",
      arguments: input ?? {},
      appContext: null,
      mcpAppUi: null,
      pluginId: null,
      readOnlyHint: null,
      result: null,
      error: null,
      durationMs: null,
    };
  }
  return {
    type: "dynamicToolCall",
    id,
    namespace: null,
    tool: String(toolName ?? "unknown"),
    arguments: input ?? {},
    status: "inProgress",
    contentItems: null,
    success: null,
    durationMs: null,
  };
}

/**
 * Complete a started item.
 * outcome: { ok, response (hook tool_response or transcript toolUseResult),
 *            text (tool_result content or error text), durationMs }
 */
export function completeToolItem(item, outcome) {
  const { ok, response, durationMs = null } = outcome;
  const text = asText(outcome.text);
  if (item.type === "commandExecution") {
    let output = text;
    if (response && typeof response === "object" && ("stdout" in response || "stderr" in response)) {
      output = [response.stdout, response.stderr].filter((part) => typeof part === "string" && part !== "").join("\n");
    }
    // Claude Code reports no exit code for a command that succeeded, and
    // states "Exit code N" in the error text for one that failed.
    const stated = /Exit code (\d+)/.exec(text);
    const exitCode = ok ? 0 : stated ? Number(stated[1]) : null;
    return { ...item, status: ok ? "completed" : "failed", aggregatedOutput: output, exitCode, durationMs };
  }
  if (item.type === "fileChange") {
    let changes = item.changes;
    if (ok && response && typeof response === "object") {
      const filePath = String(response.filePath ?? item.changes[0]?.path ?? "");
      if (response.type === "create") {
        changes = [{ path: filePath, kind: { type: "add" }, diff: asText(response.content) }];
      } else if (Array.isArray(response.structuredPatch) && response.structuredPatch.length > 0) {
        changes = [{ path: filePath, kind: { type: "update", move_path: null }, diff: patchToDiff(response.structuredPatch) }];
      }
    }
    return { ...item, changes, status: ok ? "completed" : "failed" };
  }
  if (item.type === "mcpToolCall") {
    return ok
      ? { ...item, status: "completed", durationMs, result: { content: [{ type: "text", text }], structuredContent: null, _meta: null } }
      : { ...item, status: "failed", durationMs, error: { message: text || "tool call failed" } };
  }
  return {
    ...item,
    status: ok ? "completed" : "failed",
    success: ok,
    durationMs,
    contentItems: [{ type: "inputText", text }],
  };
}

/** Close an item that never reported a result, when its turn ends. */
export function abandonToolItem(item, status) {
  if (item.type === "commandExecution" || item.type === "fileChange") return { ...item, status };
  const failed = status !== "completed";
  if (item.type === "mcpToolCall") {
    return failed
      ? { ...item, status: "failed", error: { message: status === "declined" ? "declined" : "no result reported" } }
      : { ...item, status: "completed", result: { content: [], structuredContent: null, _meta: null } };
  }
  return { ...item, status: failed ? "failed" : "completed", success: !failed, contentItems: [] };
}

export function userMessageItem(id, input, clientId = null) {
  return { type: "userMessage", id, clientId, content: input };
}

export function agentMessageItem(id, text) {
  return {
    type: "agentMessage",
    id,
    text,
    phase: null,
    moderation: null,
    memoryCitation: null,
    delivery: null,
    questions: null,
  };
}

/** A one-line description of a tool call, for approval prompts. */
export function describeTool(toolName, input) {
  if (toolName === "Bash") return asText(input?.command);
  return `${toolName} ${asText(input ?? {})}`.slice(0, 2000);
}
