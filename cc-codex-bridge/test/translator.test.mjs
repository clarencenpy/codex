// Replays hook events and transcript lines recorded from a real Claude Code
// session through the translator, and checks schema and ordering.
import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import path from "node:path";
import { Translator } from "../src/translator.mjs";
import { validateOutgoing } from "../src/schema.mjs";
import { ROOT, validator, InvariantChecker } from "./helpers.mjs";

function loadFixture() {
  const read = (name) =>
    readFileSync(path.join(ROOT, "test", "fixtures", name), "utf8").split("\n").filter(Boolean).map((line) => JSON.parse(line));
  const hooks = read("events.jsonl").map((entry) => ({ source: "hook", ts: entry.ts, payload: entry.payload }));
  let last = 0;
  const transcript = read("transcript.jsonl").map((payload) => {
    const ts = Date.parse(payload.timestamp ?? "");
    if (Number.isFinite(ts)) last = ts;
    return { source: "transcript", ts: last, payload };
  });
  return [...hooks, ...transcript].sort((a, b) => a.ts - b.ts);
}

/** Drive a translator the way the session runtime does. */
function harness({ decide = () => "accept", state } = {}) {
  let clock = 1_700_000_000_000;
  const events = [];
  const checker = new InvariantChecker();
  const schemaErrors = [];
  let requestId = 0;
  const translator = new Translator({
    threadId: "thread-1",
    cwd: "/work",
    now: () => (clock += 10),
    fileExists: () => true,
    state,
    emit: (event) => {
      events.push(event);
      if (event.type === "notification") {
        const message = { method: event.method, params: event.params };
        const errors = validateOutgoing(validator, message);
        if (errors.length) schemaErrors.push({ method: event.method, errors });
        checker.observe(message);
      } else if (event.type === "approval") {
        const message = { id: ++requestId, method: event.method, params: event.params };
        const errors = validateOutgoing(validator, message);
        if (errors.length) schemaErrors.push({ method: event.method, errors });
        queueMicrotask(() => translator.approvalAnswered(decide(event), requestId));
      }
    },
  });
  return { translator, events, checker, schemaErrors };
}

async function replay(h, entries) {
  for (const entry of entries) {
    if (entry.source === "hook") {
      const { payload } = entry;
      // The bridge starts a turn when it sends a prompt; the recording's
      // prompts were typed, so start the turn where the bridge would have.
      if (payload.hook_event_name === "UserPromptSubmit" && !payload.prompt.startsWith("<task-notification>")) {
        h.translator.startTurn({ input: [{ type: "text", text: payload.prompt, text_elements: [] }] });
      }
      h.translator.onHook(payload);
    } else {
      h.translator.onTranscriptLine(entry.payload);
    }
    await null; // let a queued approval answer land, as it would between ticks
  }
}

const turnsOf = (events) => events.filter((e) => e.type === "turnEnded").map((e) => e.turn);
const shape = (turn) => `${turn.status}: ${turn.items.map((item) => `${item.type}${item.status ? `(${item.status})` : ""}`).join(", ")}`;

test("recorded session maps to schema-valid, correctly ordered turns", async () => {
  const h = harness({ decide: (event) => (event.params.command?.startsWith("curl") ? "decline" : "accept") });
  await replay(h, loadFixture());
  assert.deepEqual(h.schemaErrors, []);
  assert.deepEqual(h.checker.errors, []);
  assert.deepEqual(turnsOf(h.events).map(shape), [
    "completed: userMessage, commandExecution(completed), fileChange(completed), agentMessage",
    "completed: userMessage, commandExecution(failed), commandExecution(completed), agentMessage",
    "interrupted: userMessage, agentMessage",
    "interrupted: userMessage, commandExecution(declined)",
    // The background task reported back by itself, then the session was closed.
    "failed: ",
  ]);
  assert.equal(h.translator.turn, null);
});

test("tool items carry command output and a real diff", async () => {
  const h = harness();
  await replay(h, loadFixture());
  const [first] = turnsOf(h.events);
  const command = first.items.find((item) => item.type === "commandExecution");
  assert.equal(command.command, "echo hi > a.txt");
  assert.equal(command.exitCode, 0);
  assert.equal(command.cwd, "/work");
  const change = first.items.find((item) => item.type === "fileChange").changes[0];
  assert.equal(change.path, "/work/a.txt");
  assert.equal(change.diff, "@@ -1,1 +1,1 @@\n-hi\n+hello\n");
  assert.equal(first.items.at(-1).text, "done");
});

test("permission prompts become approval requests for the right item", async () => {
  const h = harness();
  await replay(h, loadFixture());
  const approvals = h.events.filter((event) => event.type === "approval");
  assert.deepEqual(approvals.map((a) => a.method), [
    "item/commandExecution/requestApproval",
    "item/fileChange/requestApproval",
    "item/commandExecution/requestApproval",
  ]);
  assert.equal(approvals[0].params.itemId, "toolu_01XXvRd4gpmP3SnNyMbyuD5o");
  assert.equal(approvals[0].params.command, "echo hi > a.txt");
  assert.equal(approvals[1].params.itemId, "toolu_01F3Httx3DAuKSi7oUcNN7zT");
});

test("each tool call yields one item even though hooks and transcript both report it", async () => {
  const h = harness();
  await replay(h, loadFixture());
  const started = h.events.filter((e) => e.type === "notification" && e.method === "item/started").map((e) => e.params.item.id);
  assert.equal(new Set(started).size, started.length);
});

test("a turn survives a snapshot and restore mid-way", async () => {
  const entries = loadFixture();
  const cut = entries.findIndex((entry) => entry.payload.hook_event_name === "PostToolUse");
  const before = harness();
  await replay(before, entries.slice(0, cut));
  assert.equal(before.translator.turn.status, "inProgress");
  const after = harness({ state: JSON.parse(JSON.stringify(before.translator.snapshot())) });
  await replay(after, entries.slice(cut));
  assert.deepEqual(after.schemaErrors, []);
  assert.equal(shape(turnsOf(after.events)[0]), "completed: userMessage, commandExecution(completed), fileChange(completed), agentMessage");
});

test("the stop hook supplies the final message when the transcript trails it", () => {
  const h = harness();
  h.translator.startTurn({ input: [{ type: "text", text: "hi", text_elements: [] }] });
  h.translator.onHook({ hook_event_name: "Stop", last_assistant_message: "hello there" });
  const [turn] = turnsOf(h.events);
  assert.equal(shape(turn), "completed: userMessage, agentMessage");
  assert.equal(turn.items[1].text, "hello there");
  // The late transcript line must not produce anything after the turn ended.
  const count = h.events.length;
  h.translator.onTranscriptLine({ type: "assistant", uuid: "late", message: { content: [{ type: "text", text: "hello there" }] } });
  assert.equal(h.events.length, count);
  assert.deepEqual(h.checker.errors, []);
});

test("unknown shapes are ignored, not fatal", () => {
  const h = harness();
  h.translator.startTurn({ input: [{ type: "text", text: "hi", text_elements: [] }] });
  for (const junk of [null, 7, "x", {}, { type: "assistant" }, { type: "user", message: { content: [null, 3, { type: "tool_result" }] } }]) {
    h.translator.onTranscriptLine(junk);
  }
  for (const junk of [null, {}, { hook_event_name: "FutureEvent" }, { hook_event_name: "PreToolUse" }, { hook_event_name: "PostToolUse", tool_use_id: "nope" }]) {
    h.translator.onHook(junk);
  }
  h.translator.onHook({ hook_event_name: "PreToolUse", tool_use_id: "t1", tool_name: "WeirdNewTool", tool_input: { a: 1 } });
  h.translator.onHook({ hook_event_name: "PreToolUse", tool_use_id: "t2", tool_name: "mcp__srv__do_thing", tool_input: {} });
  h.translator.onHook({ hook_event_name: "PostToolUse", tool_use_id: "t2", tool_response: [{ type: "text", text: "ok" }] });
  h.translator.onHook({ hook_event_name: "Stop" });
  assert.deepEqual(h.schemaErrors, []);
  assert.deepEqual(h.checker.errors, []);
  assert.equal(shape(turnsOf(h.events)[0]), "completed: userMessage, dynamicToolCall(completed), mcpToolCall(completed)");
});

test("a failure hook ends the turn as failed with an error", () => {
  const h = harness();
  h.translator.startTurn({ input: [{ type: "text", text: "hi", text_elements: [] }] });
  h.translator.onHook({ hook_event_name: "StopFailure", error: "rate limited" });
  const [turn] = turnsOf(h.events);
  assert.equal(turn.status, "failed");
  assert.match(turn.error.message, /rate limited/);
  assert.equal(h.events.filter((e) => e.method === "turn/completed").length, 1);
});
