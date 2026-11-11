# cc-codex-bridge

A local bridge that speaks the Codex app-server protocol (JSON-RPC 2.0) to a
Codex client while an unmodified, interactive Claude Code session in tmux does
the work. The Codex client believes it is talking to `codex app-server`; the
bridge turns its thread and turn requests into keystrokes in a tmux pane, and
turns Claude Code's hook events and transcript back into Codex notifications.

```
Codex client ──JSON-RPC──▶ facade ──▶ session manager ──keystrokes──▶ tmux pane: claude
     ▲                        │                                             │
     └── notifications ◀── translator ◀── events.jsonl (hooks) + transcript ┘
```

## Before you use it

- **Whether driving the interactive Claude Code client from a script is
  allowed on a consumer (subscription) plan is unverified.** Read the terms
  that apply to your plan and decide for yourself. The bridge does not make
  that choice for you: it has no default login mode (see "Which login is
  used").
- Single user, single machine. There is no hosted relay and no multi-tenant
  mode, and the WebSocket transport refuses to listen on a non-loopback
  address. Do not share one subscription across people with it.
- The bridge runs the real `claude` CLI as you. It never reads, copies,
  proxies or forwards OAuth tokens or API keys, never calls Anthropic
  endpoints, and never imitates Claude Code's client headers.
- No Windows support. No models other than Claude's.

## Requirements

- Node.js 22.13 or newer (uses the built-in `node:sqlite`; there are no npm
  dependencies)
- tmux
- Claude Code, already set up: run `claude` once in a terminal to finish the
  first-run screens and log in

## Run

```bash
node cc-codex-bridge/bin/cc-codex-bridge.mjs app-server
```

That serves newline-delimited JSON on stdio, which is what a Codex client
spawns locally or over SSH. Point the client at this command instead of
`codex app-server`. Two more transports carry the same messages as WebSocket
text frames:

```bash
node cc-codex-bridge/bin/cc-codex-bridge.mjs app-server --listen ws://127.0.0.1:4500
```

```bash
node cc-codex-bridge/bin/cc-codex-bridge.mjs app-server --listen unix:///tmp/ccb.sock
```

Logs go to stderr and to `bridge.log` in the state directory.

## Configuration

`~/.config/cc-codex-bridge/config.json` (or `--config FILE`, or `$CCB_CONFIG`).
Every key is optional.

```json
{
  "engineCommand": ["claude"],
  "models": [
    { "id": "sonnet", "displayName": "Claude Sonnet", "isDefault": true },
    { "id": "opus", "displayName": "Claude Opus" }
  ],
  "autoTrustWorkspace": false,
  "testedClaudeVersion": "2.1.322"
}
```

| Key | Default | Meaning |
| --- | --- | --- |
| `engineCommand` | `["claude"]` | Command that starts the engine. The bridge appends `--settings`, `--session-id` or `--resume`, and `--model`. |
| `models` | `[]` | Claude models offered to the client in `model/list`. Empty until you fill it in; the bridge invents none. A model the client asks for must be on this list. |
| `autoTrustWorkspace` | `false` | Answer Claude Code's "do you trust this folder" prompt with yes. When false, starting a thread in an untrusted folder fails with a message saying what to do. |
| `stateDir` | `~/.local/state/cc-codex-bridge` | SQLite file, per-thread hook settings and event logs, bridge log. |
| `tmuxSocket` | `ccb` | Name of the dedicated tmux server (`tmux -L ccb`). |
| `paneWidth`, `paneHeight` | `200`, `50` | Fixed pane size, so wrapping is predictable. |
| `testedClaudeVersion` | `2.1.322` | A different installed version produces a warning. |
| `startupTimeoutMs`, `interruptTimeoutMs`, `paneCheckAfterMs`, `pollIntervalMs` | `45000`, `5000`, `4000`, `150` | Timings. |

### Which login is used

Neither API-key mode nor subscription login is the default; both are equal
paths and the bridge does not choose. It starts `engineCommand` in its own
environment, so Claude Code uses whatever you set up for it:

- **Subscription login**: you logged in with `claude` in a terminal; nothing
  else to do.
- **API key**: export `ANTHROPIC_API_KEY` before starting the bridge. Claude
  Code picks it up from the environment it inherits. The tmux server keeps the
  environment it was first started with, so after changing the variable run
  `tmux -L ccb kill-server` (this ends running threads' sessions; they resume
  on next use).

The bridge itself never reads the key or the stored login.

## What the client can do

Implemented requests: `initialize`, `thread/start`, `thread/resume`,
`thread/list`, `thread/read`, `thread/archive`, `thread/loaded/list`,
`thread/unsubscribe`, `thread/name/set`, `turn/start`, `turn/interrupt`,
`model/list`, and fixed-answer stubs for `account/read` and `config/read`.
Every other method gets a JSON-RPC method-not-found error (`-32601`).

Messages the bridge sends: `thread/started`, `thread/status/changed`,
`thread/archived`, `thread/name/updated`, `turn/started`, `turn/completed`,
`item/started`, `item/completed`, `item/agentMessage/delta`, `error`,
`serverRequest/resolved`, `configWarning`, and the approval requests
`item/commandExecution/requestApproval` and `item/fileChange/requestApproval`.

| Claude Code event | Source | Codex message |
| --- | --- | --- |
| Prompt sent by the bridge | bridge | `turn/started`, `userMessage` item |
| Assistant text block | transcript | `agentMessage` item: started, one delta with the whole text, completed |
| Bash tool call | `PreToolUse`/`PostToolUse` hooks, transcript | `commandExecution` item with command, cwd, output, exit code |
| Edit, Write, MultiEdit, NotebookEdit | hooks, transcript | `fileChange` item with a unified diff |
| `mcp__server__tool` call | hooks, transcript | `mcpToolCall` item |
| Any other tool | hooks, transcript | `dynamicToolCall` item |
| Permission prompt | `PermissionRequest` hook; `Notification` hook and pane as fallbacks | approval request, held open |
| Turn finished | `Stop` hook | `turn/completed` (completed), thread idle |
| Interrupted or permission refused | transcript marker | `turn/completed` (interrupted) |
| `StopFailure`, `SessionEnd`, dead or logged-out client | hooks, pane | `error`, `turn/completed` (failed) |

Approvals: the client's answer selects the option in Claude Code's prompt by
its text, never by position. `accept` presses the option reading exactly
"Yes"; `acceptForSession` the "Yes, and don't ask again / always allow" option
(falling back to "Yes" when there is none, never the "switch to auto mode"
option); `decline` presses "No"; `cancel` sends Escape. If the prompt or a
matching option is not on screen, no key is sent and the turn fails.

## How it works

- **tmux**: one session per thread, named `ccb-<threadId>`, on the dedicated
  server `tmux -L ccb`. Your own tmux sessions are never touched. Sessions
  outlive the bridge; only `thread/archive` ends one. You can watch a thread
  with `tmux -L ccb attach -t ccb-<threadId>` (read-only use recommended: keys
  you type go to Claude Code).
- **Hooks without touching your settings**: each thread gets its own settings
  file, passed with `--settings`, whose hooks run `hooks/ccb-hook.mjs`. That
  script appends the hook's JSON, with a timestamp, to the thread's
  `events.jsonl`, prints nothing and always exits 0.
- **Transcript**: the hook payloads give the transcript path; the bridge tails
  it by byte offset.
- **Order of trust**: hooks, then transcript, then pane text. The two
  structured sources are merged by timestamp and de-duplicated by tool-call
  id. Pane text is read for three things only: the choices in a permission
  prompt, the trust / first-run / logged-out screens, and as a watchdog when a
  turn has been silent. All of its patterns are in `src/pane-patterns.mjs`,
  recorded for one Claude Code version.
- **Prompts** are sent with bracketed paste (`load-buffer`, `paste-buffer -p`)
  and a separate Enter, one at a time, only when the session is idle; prompts
  that arrive earlier are queued. A prompt that starts with `/`, `!` or `#`
  gets a leading space so it stays a plain prompt instead of switching Claude
  Code into command, shell or memory mode.
- **Persistence**: thread id, Claude session id, working directory, tmux name,
  both byte offsets, the in-progress turn and finished turns live in
  `bridge.sqlite`. After a restart the bridge reattaches to the `ccb-*`
  sessions and resumes from the saved offsets, so nothing is replayed twice.
  A thread whose tmux session is gone is relaunched with `--resume`.

## Known limitations

- Whole messages, not token streaming: the transcript is written in chunks, so
  each assistant message arrives as one delta carrying its full text.
- Declining a permission prompt ends the turn. In Claude Code, "No" stops the
  request instead of letting the model carry on, so the turn completes as
  `interrupted` with the item marked `declined`.
- Claude Code reports no exit code for a command that succeeded; the bridge
  reports `0`. For a failed command it reports the code Claude Code states in
  its error text, or `null`.
- An approval for a tool that is neither a shell command nor a file edit is
  sent as a command-execution approval whose `command` describes the tool call,
  because the schema has no closer fit.
- Codex's approval policy and sandbox settings are not applied. Claude Code's
  own permission system decides what needs approval; the bridge reports
  `approvalPolicy: "on-request"` and `sandbox: dangerFullAccess` because it
  adds no sandbox of its own, and it never starts Claude Code in a
  permission-bypassing mode.
- Image and audio inputs are not forwarded; a local image is passed as its
  path.
- Anything Claude Code does not log (internal thinking, UI-only state) is
  invisible to the Codex client. Subagent (sidechain) transcript lines are
  skipped.
- Pane patterns depend on Claude Code's UI wording and will need updating when
  it changes.
- When Claude Code starts work by itself (a background task reporting back),
  the bridge reports it as a turn with no user message.

## Verified against the installed tools

Checked on Claude Code 2.1.322, tmux 3.4 and the app-server schema in this
repository (`codex-rs/app-server-protocol/schema`), before writing code.
Where the original design notes and the tools disagreed, the tools won:

| Point | Finding |
| --- | --- |
| Hook events | `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `PermissionRequest`, `Notification`, `Stop`, `StopFailure`, `SessionEnd`. Payloads carry `session_id`, `transcript_path`, `cwd`; tool hooks carry `tool_name`, `tool_input`, `tool_use_id`; `Stop` carries `last_assistant_message`. |
| Permission prompts | There is a dedicated `PermissionRequest` hook with the tool name and input (but no tool-call id), so the bridge uses it first and the generic `Notification` hook only as a fallback. The notification can arrive seconds after it. |
| Transcript path | Not guessed: every hook payload states it. |
| Extra settings file | `--settings <file-or-json>`. |
| New / resumed session | `--session-id <uuid>` and `--resume <id>`. |
| Interrupt key | Escape. It fires **no** `Stop` hook; the transcript gets a `[Request interrupted by user]` user line, which is what ends the turn. |
| Refused permission | "No" also fires no `Stop` hook and writes `[Request interrupted by user for tool use]`. |
| Stop hook vs transcript | The final assistant text can reach the transcript after the `Stop` hook; the bridge waits briefly, then takes the text from the hook payload. |
| First run and trust | A new machine shows theme and security screens, and a new folder shows a trust prompt, before any hook fires. These are detected on the pane and reported as errors (or the trust prompt answered, if configured). |
| Wire format | No `jsonrpc` member. Remote transports are WebSocket, including over a Unix socket. |
| Schema | This fork's schema has more required fields than the usual shape (for example `Thread` and `Turn`); item names used are `userMessage`, `agentMessage`, `commandExecution`, `fileChange`, `mcpToolCall`, `dynamicToolCall`. |

A possible improvement, not used here: a `PermissionRequest` hook can itself
return a decision, which would replace the keystroke for approvals. The bridge
keeps to keystrokes matched by option text.

## Schema

Tests validate every message the bridge sends against the generated schema
bundle. By default that is the one in this repository
(`codex-rs/app-server-protocol/schema/json/codex_app_server_protocol.schemas.json`).
To build against your installed Codex version instead, vendor its output and
it takes precedence:

```bash
codex app-server generate-json-schema --out cc-codex-bridge/schemas
```

(or set `$CCB_SCHEMA` to a bundle file).

## Tests

```bash
cd cc-codex-bridge && npm test
```

- `test/schema.test.mjs`: the validator.
- `test/translator.test.mjs`: replays hook events and transcript lines
  recorded from a real Claude Code session (`test/fixtures`) and checks schema
  validity and the ordering invariants: items start before they complete, no
  delta follows a completed item, every turn ends with exactly one terminal
  notification.
- `test/facade.test.mjs`: handshake, stubs, errors, model list, version
  warning, WebSocket over a Unix socket.
- `test/e2e.test.mjs`: the whole bridge with a real tmux server and a fake
  engine (`test/fake-claude.mjs`) that fires the same hooks, writes the same
  transcript shapes and draws the same prompts: turns, queued prompts, tool
  items, approvals by option text, pane fallback, interrupt, kill-and-restart
  mid-turn, relaunch with `--resume`, engine death, trust prompt, archive.
  Skipped when tmux is missing.

## Layout

```
bin/cc-codex-bridge.mjs   CLI
hooks/ccb-hook.mjs        hook script: append stdin JSON to events.jsonl
src/server.mjs            app-server facade (component A)
src/session.mjs           tmux session manager and event pump (B, C)
src/translator.mjs        Claude Code events -> Codex messages (D)
src/items.mjs             tool call -> ThreadItem mapping
src/pane-patterns.mjs     every on-screen pattern, version-pinned
src/tmux.mjs, tailer.mjs, store.mjs, jsonrpc.mjs, transports.mjs, schema.mjs, config.mjs
```
