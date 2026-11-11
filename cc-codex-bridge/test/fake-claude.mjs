#!/usr/bin/env node
// A stand-in for the interactive `claude` CLI, for tests. It accepts the same
// flags the bridge passes, fires the configured hooks with payloads shaped
// like Claude Code 2.1.322's, writes a transcript JSONL, and draws the same
// permission and trust prompts. What it does for a prompt is picked by
// keywords in the prompt text:
//   run: <cmd>      Bash tool call behind a permission prompt
//   edit: <path>    Edit tool call behind a permission prompt
//   mcp             an MCP tool call, no prompt
//   slow            keeps working until interrupted with Escape
//   late            fires the stop hook before the transcript has the reply
//   quiet-prompt    permission prompt with no PermissionRequest/Notification hook
//   die             exits mid-turn
//   anything else   replies "echo: <prompt>"
import { readFileSync, appendFileSync, mkdirSync } from "node:fs";
import { spawnSync } from "node:child_process";
import { randomUUID } from "node:crypto";
import path from "node:path";

const argv = process.argv.slice(2);
if (argv.includes("--version")) {
  console.log(`${process.env.FAKE_CLAUDE_VERSION ?? "2.1.322"} (Claude Code)`);
  process.exit(0);
}
const flag = (name) => (argv.includes(name) ? argv[argv.indexOf(name) + 1] : undefined);
const settings = JSON.parse(readFileSync(flag("--settings"), "utf8"));
const resumed = flag("--resume") !== undefined;
const sessionId = flag("--resume") ?? flag("--session-id") ?? randomUUID();
const home = process.env.FAKE_CLAUDE_HOME ?? path.dirname(flag("--settings"));
mkdirSync(home, { recursive: true });
const transcriptPath = path.join(home, `${sessionId}.jsonl`);
const cwd = process.cwd();
const out = (text) => process.stdout.write(text.replace(/\n/g, "\r\n"));

function hook(name, extra = {}) {
  const payload = { session_id: sessionId, transcript_path: transcriptPath, cwd, hook_event_name: name, ...extra };
  for (const group of settings.hooks?.[name] ?? []) {
    for (const entry of group.hooks ?? []) spawnSync("sh", ["-c", entry.command], { input: JSON.stringify(payload) });
  }
}

function transcript(line) {
  appendFileSync(transcriptPath, JSON.stringify({ uuid: randomUUID(), timestamp: new Date().toISOString(), isSidechain: false, sessionId, ...line }) + "\n");
}
const assistant = (content) => transcript({ type: "assistant", message: { role: "assistant", content } });
const userLine = (content, extra = {}) => transcript({ type: "user", message: { role: "user", content }, ...extra });

// ---- keyboard ----------------------------------------------------------
let keyHandler = () => {};
let line = "";
let pending = "";
let escTimer = null;
function feed(chunk) {
  pending += chunk;
  clearTimeout(escTimer);
  for (;;) {
    if (pending === "") return;
    if (pending.startsWith("\x1b[200~")) {
      const end = pending.indexOf("\x1b[201~");
      if (end < 0) return;
      // A terminal pastes line breaks as carriage returns.
      keyHandler({ paste: pending.slice(6, end).replace(/\r/g, "\n") });
      pending = pending.slice(end + 6);
    } else if (pending.startsWith("\x1b[A") || pending.startsWith("\x1b[B") || pending.startsWith("\x1bOA") || pending.startsWith("\x1bOB")) {
      keyHandler({ key: pending[2] === "A" ? "up" : "down" });
      pending = pending.slice(3);
    } else if (pending[0] === "\x1b") {
      if (pending.length === 1) {
        // A lone ESC is the Escape key; wait briefly in case it starts a sequence.
        escTimer = setTimeout(() => {
          pending = "";
          keyHandler({ key: "escape" });
        }, 40);
        return;
      }
      if ("[O".includes(pending[1]) && pending.length < 3) return;
      if (pending[1] === "[" && "200~".startsWith(pending.slice(2, 6)) && pending.length < 6) return;
      keyHandler({ key: "escape" });
      pending = pending.slice(1);
    } else {
      const ch = pending[0];
      pending = pending.slice(1);
      if (ch === "\r" || ch === "\n") keyHandler({ key: "enter" });
      else if (ch === "\x03") process.exit(0);
      else keyHandler({ char: ch });
    }
  }
}
if (process.stdin.isTTY) process.stdin.setRawMode(true);
process.stdin.setEncoding("utf8");
process.stdin.on("data", feed);
out("\x1b[?2004h"); // ask for bracketed paste, as the real client does

const nextKey = () => new Promise((resolve) => (keyHandler = resolve));
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ---- screens -----------------------------------------------------------
async function trustPrompt() {
  let selected = 0;
  const options = ["No, exit", "Yes, I trust this folder"];
  for (;;) {
    out("\x1b[2J\x1b[H Accessing workspace:\n " + cwd + "\n Quick safety check: Is this a project you created or one you trust?\n");
    options.forEach((text, i) => out(` ${i === selected ? "❯" : " "} ${text}\n`));
    out(" Enter to confirm · Esc to cancel\n");
    const key = await nextKey();
    if (key.key === "down") selected = Math.min(1, selected + 1);
    else if (key.key === "up") selected = Math.max(0, selected - 1);
    else if (key.key === "enter") {
      if (selected === 0) process.exit(0);
      out("\x1b[2J\x1b[H");
      return;
    } else if (key.key === "escape") process.exit(0);
  }
}

/** Draws a numbered permission prompt; resolves "yes" | "no" | "escape". */
async function permissionPrompt(title, detail, question, options) {
  out(`\n────────────────\n ${title}\n ${detail}\n ${question}\n`);
  options.forEach((text, i) => out(` ${i === 0 ? "❯" : " "} ${i + 1}. ${text}\n`));
  out(" Esc to cancel · Tab to amend\n");
  for (;;) {
    const key = await nextKey();
    if (key.key === "escape") return "escape";
    const index = Number(key.char) - 1;
    if (Number.isInteger(index) && options[index]) {
      out(`\x1b[2J\x1b[H(chose: ${options[index]})\n`);
      appendFileSync(path.join(home, "choices.log"), options[index] + "\n");
      return /^No/.test(options[index]) ? "no" : "yes";
    }
  }
}

function idle() {
  out("\n❯ \n  ? for shortcuts\n");
}

async function runTool({ name, input, response, resultText, prompt, quiet }) {
  const id = `toolu_${randomUUID().replace(/-/g, "").slice(0, 20)}`;
  assistant([{ type: "tool_use", id, name, input }]);
  hook("PreToolUse", { tool_name: name, tool_input: input, tool_use_id: id });
  if (prompt) {
    if (!quiet) {
      hook("PermissionRequest", { tool_name: name, tool_input: input });
      hook("Notification", { message: "Claude needs your permission", notification_type: "permission_prompt" });
    }
    const answer = await permissionPrompt(...prompt);
    if (answer !== "yes") {
      if (answer === "no") {
        userLine([{ type: "tool_result", tool_use_id: id, content: "The user doesn't want to proceed with this tool use.", is_error: true }]);
      }
      userLine([{ type: "text", text: "[Request interrupted by user for tool use]" }]);
      return false;
    }
  }
  hook("PostToolUse", { tool_name: name, tool_input: input, tool_response: response, tool_use_id: id, duration_ms: 5 });
  userLine([{ type: "tool_result", tool_use_id: id, content: resultText }], { toolUseResult: response });
  return true;
}

async function turn(prompt) {
  hook("UserPromptSubmit", { prompt });
  userLine(prompt);
  out(`\n> ${prompt}\n`);
  const reply = (text) => {
    assistant([{ type: "text", text }]);
    out(`● ${text}\n`);
  };
  if (/\bdie\b/.test(prompt)) process.exit(1);
  if (/\bslow\b/.test(prompt)) {
    reply("working on it");
    const key = await Promise.race([nextKey(), sleep(30_000).then(() => ({}))]);
    if (key.key === "escape") {
      userLine([{ type: "text", text: "[Request interrupted by user]" }]);
      out("  ⎿  Interrupted · What should Claude do instead?\n");
      return;
    }
  }
  const run = /run: (.+)/.exec(prompt);
  if (run) {
    const command = run[1].trim();
    const ok = await runTool({
      name: "Bash",
      input: { command, description: "Run the command" },
      response: { stdout: `ran ${command}`, stderr: "", interrupted: false },
      resultText: `ran ${command}`,
      prompt: ["Bash command", command, "Do you want to proceed?", ["Yes", `Yes, and don’t ask again for: ${command.split(" ")[0]} *`, "Yes, and switch to auto mode · auto mode handles these prompts for you", "No"]],
      quiet: /quiet-prompt/.test(prompt),
    });
    if (!ok) return;
  }
  const edit = /edit: (\S+)/.exec(prompt);
  if (edit) {
    const filePath = path.resolve(cwd, edit[1]);
    const ok = await runTool({
      name: "Edit",
      input: { file_path: filePath, old_string: "hi", new_string: "hello", replace_all: false },
      response: { filePath, oldString: "hi", newString: "hello", structuredPatch: [{ oldStart: 1, oldLines: 1, newStart: 1, newLines: 1, lines: ["-hi", "+hello"] }] },
      resultText: `The file ${filePath} has been updated successfully.`,
      prompt: ["Edit file", filePath, `Do you want to make this edit to ${path.basename(filePath)}?`, ["Yes", "Yes, and switch to accept edits (auto-approve file edits and common file commands) for this session (shift+tab)", "No"]],
    });
    if (!ok) return;
  }
  if (/\bmcp\b/.test(prompt)) {
    await runTool({ name: "mcp__files__search", input: { query: "x" }, response: [{ type: "text", text: "found" }], resultText: "found" });
  }
  const text = run || edit ? "done" : `echo: ${prompt}`;
  if (/\blate\b/.test(prompt)) {
    hook("Stop", { stop_hook_active: false, last_assistant_message: text });
    await sleep(400);
    reply(text);
    return;
  }
  reply(text);
  hook("Stop", { stop_hook_active: false, last_assistant_message: text });
}

// ---- main loop -----------------------------------------------------------
if (process.env.FAKE_CLAUDE_BLOCK === "login") {
  out(" Select login method:\n ❯ 1. Claude account with subscription\n   2. Anthropic Console account\n");
  await sleep(600_000);
}
if (process.env.FAKE_CLAUDE_TRUST === "1" && !resumed) await trustPrompt();
out(`Fake Claude Code — session ${sessionId}${resumed ? " (resumed)" : ""}\n`);
hook("SessionStart", { source: resumed ? "resume" : "startup", model: "fake" });
idle();
for (;;) {
  const key = await nextKey();
  if (key.paste !== undefined) line += key.paste;
  else if (key.char !== undefined) line += key.char;
  else if (key.key === "enter" && line.trim() !== "") {
    const prompt = line;
    line = "";
    await turn(prompt);
    idle();
  }
}
