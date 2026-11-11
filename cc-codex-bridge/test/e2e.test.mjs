// End to end: the real bridge, a real tmux server, and the fake Claude Code
// engine (test/fake-claude.mjs). Covers milestones 2 to 5 and the pane
// fallbacks. Skipped when tmux is not installed.
import test from "node:test";
import assert from "node:assert/strict";
import { execFileSync, spawnSync } from "node:child_process";
import { readFileSync, existsSync } from "node:fs";
import path from "node:path";
import { BridgeClient, tempDir } from "./helpers.mjs";

const hasTmux = spawnSync("tmux", ["-V"]).status === 0;
const FAKE = path.join(import.meta.dirname, "fake-claude.mjs");
let counter = 0;

const text = (value) => [{ type: "text", text: value, text_elements: [] }];
const shape = (turn) => `${turn.status}: ${turn.items.map((item) => `${item.type}${item.status ? `(${item.status})` : ""}`).join(", ")}`;

/** A bridge with its own state dir and tmux server. */
function setup({ config = {}, env = {} } = {}) {
  const stateDir = tempDir();
  const socket = `ccbt-${process.pid}-${++counter}`;
  const options = {
    stateDir,
    config: { engineCommand: [process.execPath, FAKE], tmuxSocket: socket, startupTimeoutMs: 8000, paneCheckAfterMs: 600, interruptTimeoutMs: 3000, ...config },
    env: { FAKE_CLAUDE_HOME: path.join(stateDir, "fake-home"), ...env },
  };
  const tmux = (...args) => spawnSync("tmux", ["-L", socket, ...args], { encoding: "utf8" });
  return {
    stateDir,
    cwd: tempDir("ccb-work-"),
    connect: () => new BridgeClient(options),
    tmux,
    choices: () => (existsSync(path.join(stateDir, "fake-home", "choices.log")) ? readFileSync(path.join(stateDir, "fake-home", "choices.log"), "utf8").trim().split("\n") : []),
    cleanup: () => tmux("kill-server"),
  };
}

async function startThread(client, cwd) {
  await client.initialize();
  const { thread } = await client.request("thread/start", { cwd });
  return thread.id;
}

async function runTurn(client, threadId, prompt, { decide } = {}) {
  const from = client.messages.length;
  const { turn } = await client.request("turn/start", { threadId, input: text(prompt) });
  if (decide) {
    (async () => {
      const seen = new Set();
      for (;;) {
        const ask = await client
          .waitFor((m) => m.id !== undefined && /requestApproval$/.test(m.method ?? "") && m.params.turnId === turn.id && !seen.has(m.id), { from, timeoutMs: 15_000 })
          .catch(() => null);
        if (!ask) return;
        seen.add(ask.id);
        client.respond(ask.id, { decision: decide(ask) });
      }
    })();
  }
  const done = await client.turnCompleted(turn.id);
  return done.params.turn;
}

test("one thread, one turn: prompt in, agent message out", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    await client.waitFor((m) => m.method === "thread/started");
    assert.equal(env.tmux("has-session", "-t", `=ccb-${threadId}`).status, 0);
    assert.match(env.tmux("display-message", "-p", "-t", `=ccb-${threadId}:`, "#{window_width}x#{window_height}").stdout, /200x50/);
    const turn = await runTurn(client, threadId, "hello bridge");
    assert.equal(shape(turn), "completed: userMessage, agentMessage");
    assert.equal(turn.items[1].text, "echo: hello bridge");
    const delta = client.notifications.find((m) => m.method === "item/agentMessage/delta");
    assert.equal(delta.params.delta, "echo: hello bridge");
    const listed = await client.request("thread/list", {});
    assert.equal(listed.data[0].preview, "hello bridge");
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("multi-line and command-looking prompts arrive as one plain prompt", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const multi = await runTurn(client, threadId, "line one\nline two\n\nline four");
    assert.equal(multi.items[1].text, "echo: line one\nline two\n\nline four");
    const bang = await runTurn(client, threadId, "!rm -rf /tmp/nothing");
    assert.equal(bang.items[1].text, "echo:  !rm -rf /tmp/nothing"); // leading space keeps it out of shell mode
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("prompts sent while a turn runs are queued, not interleaved", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const started = await Promise.all(["first", "second", "third"].map((p) => client.request("turn/start", { threadId, input: text(p) })));
    const done = [];
    for (const { turn } of started) done.push((await client.turnCompleted(turn.id)).params.turn);
    assert.deepEqual(done.map((turn) => turn.items[1].text), ["echo: first", "echo: second", "echo: third"]);
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("tool calls map to command, file-change and MCP items", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const turn = await runTurn(client, threadId, "please mcp then run: make test\nand edit: notes.txt", { decide: () => "accept" });
    assert.equal(shape(turn), "completed: userMessage, commandExecution(completed), fileChange(completed), mcpToolCall(completed), agentMessage");
    const command = turn.items[1];
    assert.equal(command.command, "make test");
    assert.equal(command.aggregatedOutput, "ran make test");
    assert.equal(command.exitCode, 0);
    assert.equal(turn.items[2].changes[0].diff, "@@ -1,1 +1,1 @@\n-hi\n+hello\n");
    assert.equal(turn.items[3].server, "files");
    assert.deepEqual(env.choices(), ["Yes", "Yes"]);
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("approval answers select the prompt option by its text", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const forSession = await runTurn(client, threadId, "run: git status", { decide: () => "acceptForSession" });
    assert.equal(shape(forSession), "completed: userMessage, commandExecution(completed), agentMessage");
    const declined = await runTurn(client, threadId, "run: git push", { decide: () => "decline" });
    assert.equal(shape(declined), "interrupted: userMessage, commandExecution(declined)");
    const cancelled = await runTurn(client, threadId, "edit: a.txt", { decide: () => "cancel" });
    assert.equal(shape(cancelled), "interrupted: userMessage, fileChange(declined)");
    // Never "switch to auto mode", whatever its position.
    assert.deepEqual(env.choices(), ["Yes, and don’t ask again for: git *", "No"]);
    const asks = client.serverRequests.map((m) => m.method);
    assert.deepEqual(asks, ["item/commandExecution/requestApproval", "item/commandExecution/requestApproval", "item/fileChange/requestApproval"]);
    assert.equal(client.notifications.filter((m) => m.method === "serverRequest/resolved").length, 3);
    const waiting = client.notifications.filter((m) => m.method === "thread/status/changed" && m.params.status.activeFlags?.includes("waitingOnApproval"));
    assert.equal(waiting.length, 3);
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("a permission prompt no hook reported is found on the pane", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const turn = await runTurn(client, threadId, "quiet-prompt run: ls", { decide: () => "accept" });
    assert.equal(shape(turn), "completed: userMessage, commandExecution(completed), agentMessage");
    assert.equal(client.serverRequests.length, 1);
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("interrupt stops the turn and the thread stays usable", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const { turn } = await client.request("turn/start", { threadId, input: text("slow job") });
    await client.waitFor((m) => m.method === "item/completed" && m.params.item.type === "agentMessage");
    assert.deepEqual(await client.request("turn/interrupt", { threadId, turnId: turn.id }), {});
    const done = (await client.turnCompleted(turn.id)).params.turn;
    assert.equal(shape(done), "interrupted: userMessage, agentMessage");
    const next = await runTurn(client, threadId, "still there?");
    assert.equal(next.items[1].text, "echo: still there?");
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("the final message is not lost when the transcript trails the stop hook", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const turn = await runTurn(client, threadId, "late reply please");
    assert.equal(shape(turn), "completed: userMessage, agentMessage");
    assert.equal(turn.items[1].text, "echo: late reply please");
    const after = await runTurn(client, threadId, "next");
    assert.equal(shape(after), "completed: userMessage, agentMessage"); // no stray duplicate
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("bridge killed mid-turn: restart, resume, answer the open approval, continue", { skip: !hasTmux }, async () => {
  const env = setup();
  let client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    await runTurn(client, threadId, "before the crash");
    const { turn } = await client.request("turn/start", { threadId, input: text("run: deploy") });
    const ask = await client.waitFor((m) => m.method === "item/commandExecution/requestApproval");
    client.child.kill("SIGKILL");
    await client.exited;
    assert.equal(env.tmux("has-session", "-t", `=ccb-${threadId}`).status, 0, "tmux session outlives the bridge");

    client = env.connect();
    await client.initialize();
    const resumed = await client.request("thread/resume", { threadId });
    assert.deepEqual(resumed.thread.status, { type: "active", activeFlags: ["waitingOnApproval"] });
    assert.deepEqual(resumed.thread.turns.map((t) => t.status), ["completed", "inProgress"]);
    const again = await client.waitFor((m) => m.method === "item/commandExecution/requestApproval");
    assert.equal(again.params.itemId, ask.params.itemId);
    client.respond(again.id, { decision: "accept" });
    const done = (await client.turnCompleted(turn.id)).params.turn;
    assert.equal(shape(done), "completed: userMessage, commandExecution(completed), agentMessage");

    const read = await client.request("thread/read", { threadId, includeTurns: true });
    assert.deepEqual(read.thread.turns.map(shape), [
      "completed: userMessage, agentMessage",
      "completed: userMessage, commandExecution(completed), agentMessage",
    ]);
    assert.deepEqual(client.schemaErrors, []);
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("a turn that finished while the bridge was down is recorded once", { skip: !hasTmux }, async () => {
  const env = setup();
  let client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const { turn } = await client.request("turn/start", { threadId, input: text("slow job") });
    await client.waitFor((m) => m.method === "item/completed" && m.params.item.type === "agentMessage");
    client.child.kill("SIGKILL");
    await client.exited;
    env.tmux("send-keys", "-t", `=ccb-${threadId}:`, "Escape"); // the turn ends with nobody listening
    await new Promise((resolve) => setTimeout(resolve, 500));

    client = env.connect();
    await client.initialize();
    await client.request("thread/resume", { threadId });
    await client.waitFor((m) => m.method === "thread/status/changed" && m.params.status.type === "idle").catch(() => {});
    const read = await client.request("thread/read", { threadId, includeTurns: true });
    assert.deepEqual(read.thread.turns.map((t) => [t.id, shape(t)]), [[turn.id, "interrupted: userMessage, agentMessage"]]);
    const next = await runTurn(client, threadId, "after restart");
    assert.equal(next.items[1].text, "echo: after restart");
    assert.deepEqual(client.schemaErrors, []);
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("a thread whose tmux session is gone is relaunched with --resume", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    await runTurn(client, threadId, "one");
    env.tmux("kill-session", "-t", `=ccb-${threadId}`);
    const { turn } = await client.request("turn/start", { threadId, input: text("two") });
    const done = (await client.turnCompleted(turn.id)).params.turn;
    assert.equal(done.items[1].text, "echo: two");
    assert.match(env.tmux("capture-pane", "-p", "-t", `=ccb-${threadId}:`).stdout, /\(resumed\)/);
    const read = await client.request("thread/read", { threadId, includeTurns: true });
    assert.equal(read.thread.turns.length, 2);
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("engine dying mid-turn fails the turn with a message", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    const turn = await runTurn(client, threadId, "please die now");
    assert.equal(turn.status, "failed");
    assert.match(turn.error.message, /exited/);
    assert.ok(client.notifications.some((m) => m.method === "error"));
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("workspace trust prompt: refused by default, answered by text when allowed", { skip: !hasTmux }, async () => {
  const refused = setup({ env: { FAKE_CLAUDE_TRUST: "1" } });
  let client = refused.connect();
  try {
    await client.initialize();
    await assert.rejects(client.request("thread/start", { cwd: refused.cwd }), /trust/);
    assert.deepEqual((await client.request("thread/list", {})).data, []);
    assert.equal(refused.tmux("list-sessions").stdout.trim(), "");
    client.assertClean();
  } finally {
    await client.close();
    refused.cleanup();
  }
  const allowed = setup({ env: { FAKE_CLAUDE_TRUST: "1" }, config: { autoTrustWorkspace: true } });
  client = allowed.connect();
  try {
    const threadId = await startThread(client, allowed.cwd);
    const turn = await runTurn(client, threadId, "trusted");
    assert.equal(turn.items[1].text, "echo: trusted");
    client.assertClean();
  } finally {
    await client.close();
    allowed.cleanup();
  }
});

test("a logged-out engine fails thread start with a clear message", { skip: !hasTmux }, async () => {
  const env = setup({ env: { FAKE_CLAUDE_BLOCK: "login" } });
  const client = env.connect();
  try {
    await client.initialize();
    await assert.rejects(client.request("thread/start", { cwd: env.cwd }), /not logged in/);
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});

test("archive ends the tmux session; the user's own tmux server is never touched", { skip: !hasTmux }, async () => {
  const env = setup();
  const client = env.connect();
  try {
    const threadId = await startThread(client, env.cwd);
    await runTurn(client, threadId, "bye");
    await client.request("thread/name/set", { threadId, name: "Farewell" });
    assert.equal((await client.request("thread/read", { threadId })).thread.name, "Farewell");
    await client.request("thread/archive", { threadId });
    await client.waitFor((m) => m.method === "thread/archived");
    assert.notEqual(env.tmux("has-session", "-t", `=ccb-${threadId}`).status, 0);
    assert.deepEqual((await client.request("thread/list", {})).data, []);
    assert.equal((await client.request("thread/list", { archived: true })).data.length, 1);
    await assert.rejects(client.request("turn/start", { threadId, input: text("x") }), /archived/);
    client.assertClean();
  } finally {
    await client.close();
    env.cleanup();
  }
});
