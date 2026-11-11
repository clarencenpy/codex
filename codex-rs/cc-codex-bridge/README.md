# cc-codex-bridge

A local bridge that speaks the Codex app-server protocol (JSON-RPC 2.0) to a
Codex client while an unmodified Claude Code process does the work. The Codex
client believes it is talking to `codex app-server`.

```
Codex client ──JSON-RPC──▶ facade ──▶ thread session ──stream-json stdin──▶ claude
     ▲                                     │                                  │
     └────────── notifications ◀── translator ◀────── stream-json stdout ─────┘
```

The bridge starts one `claude` process per thread with
`--input-format stream-json --output-format stream-json`, writes each turn's
prompt to its stdin as a JSON line, and turns the JSON messages it prints into
Codex thread, turn and item notifications. Permission prompts arrive as
structured requests on the same stream and are answered the same way. Nothing
is scraped from a terminal.

## Before you use it

- **This drives Claude Code in its non-interactive (piped stdio) mode.**
  Whether doing that from a script is allowed on a consumer (subscription)
  plan is unverified. Read the terms that apply to your plan and decide for
  yourself; the bridge has no default login mode (see "Which login is used").
- Claude Code skips its workspace trust dialog in this mode. Only start
  threads in directories you trust.
- Single user, single machine. There is no hosted relay and no multi-tenant
  mode, and the WebSocket transport refuses to listen on a non-loopback
  address. Do not share one subscription across people with it.
- The bridge runs the real `claude` CLI as you. It never reads, copies,
  proxies or forwards OAuth tokens or API keys, never calls Anthropic
  endpoints, and never imitates Claude Code's client headers.

## Build and run

```bash
cd codex-rs && cargo build --release -p codex-cc-bridge
```

```bash
codex-rs/target/release/cc-codex-bridge app-server
```

That serves newline-delimited JSON on stdio, which is what a Codex client
spawns locally or over SSH. Point the client at this command instead of
`codex app-server`. The same messages are also served as WebSocket text
frames:

```bash
cc-codex-bridge app-server --listen ws://127.0.0.1:4500
```

```bash
cc-codex-bridge app-server --listen unix:///tmp/ccb.sock
```

Claude Code must already be set up: run `claude` once in a terminal to finish
its first-run screens and log in. Logs go to stderr.

## Configuration

`~/.config/cc-codex-bridge/config.json` (or `--config FILE`, or `$CCB_CONFIG`).
Every key is optional.

```json
{
  "engineCommand": ["claude"],
  "models": [
    { "id": "sonnet", "displayName": "Claude Sonnet", "isDefault": true },
    "opus"
  ]
}
```

| Key | Default | Meaning |
| --- | --- | --- |
| `engineCommand` | `["claude"]` | Command that starts the engine. The bridge appends the stream-json flags, `--session-id` or `--resume`, and `--model`. |
| `models` | `[]` | Claude models offered to the client in `model/list`. Empty until you fill it in; the bridge invents none. A model the client asks for must be on this list. |
| `stateDir` | `~/.local/state/cc-codex-bridge` | Thread records and finished turns. |
| `testedClaudeVersion` | `2.1.322` | A different installed version produces a `configWarning`. |
| `interruptTimeoutMs` | `5000` | How long an interrupt may go unconfirmed before the engine process is stopped. |

### Which login is used

Neither API-key mode nor subscription login is the default; the bridge does
not choose. It starts `engineCommand` in its own environment, so Claude Code
uses whatever you set up for it: a subscription login done with `claude` in a
terminal, or `ANTHROPIC_API_KEY` exported before starting the bridge. The
bridge itself never reads either.

## What the client can do

Implemented requests: `initialize`, `thread/start`, `thread/resume`,
`thread/list`, `thread/read`, `thread/archive`, `thread/loaded/list`,
`thread/unsubscribe`, `thread/name/set`, `turn/start`, `turn/interrupt`,
`model/list`, and fixed-answer stubs for `account/read` and `config/read`.
Every other method gets a JSON-RPC method-not-found error (`-32601`).

| Claude Code stream message | Codex message |
| --- | --- |
| Prompt written by the bridge | `turn/started`, `userMessage` item |
| `stream_event` text deltas, then the `assistant` text block | `agentMessage` item: started, `item/agentMessage/delta` per chunk, completed |
| `assistant` `tool_use` for Bash | `commandExecution` item with command, cwd, output, exit code |
| ... for Edit, Write, MultiEdit, NotebookEdit | `fileChange` item with a unified diff |
| ... for `mcp__server__tool` | `mcpToolCall` item |
| ... for any other tool | `dynamicToolCall` item |
| `control_request` `can_use_tool` | approval request, held open |
| `user` `tool_result` | the tool's item completes |
| `result` | `turn/completed` (completed, interrupted or failed), thread idle |
| Process exit mid-turn | `error`, `turn/completed` (failed) |

Approvals map onto Claude Code's permission response:

| Client decision | Sent to Claude Code |
| --- | --- |
| `accept` | allow, this once |
| `acceptForSession` | allow, plus Claude Code's own suggested rule applied for this session only (never written to your settings files) |
| `decline` | deny; Claude Code carries on with the turn |
| `cancel` | deny and stop the turn |

Interrupt sends Claude Code's interrupt control request; the turn ends as
`interrupted` when it reports back. If it does not within
`interruptTimeoutMs`, the process is stopped and the session resumes on the
next turn.

## How it works

- **One process per thread**, started on the thread's first turn and kept for
  later turns. Prompts are sent one at a time; a prompt that arrives while a
  turn runs is queued.
- **Sessions**: each thread maps to one Claude Code session id. A new thread
  starts with `--session-id`; any later process for it uses `--resume`, so the
  conversation continues after an engine crash or a bridge restart.
- **Persistence**: `threads.json` and one JSONL file of finished turns per
  thread under the state directory. A turn that was running when the bridge
  stopped is kept and shown as interrupted; its Claude Code process ends with
  the bridge.
- **Types**: every outgoing message is built from the structs in
  `codex-app-server-protocol`, so the wire shape is checked by the compiler.
- A prompt that starts with `/` gets a leading space so Claude Code does not
  treat it as one of its own commands.

## Known limitations

- A turn in flight does not survive a bridge restart (the process is a child
  of the bridge). The thread does: the next turn resumes the session.
- Claude Code reports no exit code for a command that succeeded; the bridge
  reports `0`. For a failed command it reports the code Claude Code states in
  its error text, or none.
- An approval for a tool that is neither a shell command nor a file edit is
  sent as a command-execution approval whose `command` describes the call.
- Codex's approval policy and sandbox settings are not applied. Claude Code's
  own permission system decides what needs approval; the bridge reports
  `approvalPolicy: on-request` and `sandbox: dangerFullAccess` because it adds
  no sandbox of its own, and it never starts Claude Code in a
  permission-bypassing mode.
- Image and audio inputs are not forwarded; a local image is passed as its
  path.
- Thinking is not shown, and subagent messages are skipped.
- When Claude Code starts work by itself (a background task reporting back),
  the bridge reports it as a turn with no user message.
- The stream-json format is not a stable contract. Unknown messages are
  ignored, and a version other than the tested one produces a warning.

## Verified against Claude Code 2.1.322

- Stream-json input and output work with piped stdio and no `--print` flag;
  `--verbose` is required for stream-json output.
- `--permission-prompt-tool stdio` turns permission prompts into
  `control_request` messages with subtype `can_use_tool` (tool name, input,
  tool-call id, suggested rules), answered with a `control_response`.
- A denied tool call lets the model continue; `interrupt: true` in the denial
  stops the turn.
- `{"type":"control_request","request":{"subtype":"interrupt"}}` on stdin
  interrupts; the stream then carries a `[Request interrupted by user]` user
  message and a `result`.
- One process handles many turns; each ends with a `result` message.

Run end to end against the real CLI: a Bash call behind an approval, token
streaming, and an interrupt. `acceptForSession` was accepted by Claude Code,
but that its session rule suppresses a later prompt was not confirmed.

## Tests

```bash
cd codex-rs && cargo test -p codex-cc-bridge
```

The tests run the whole bridge in process against a scripted engine
(`tests/common/mod.rs`) that emits the message shapes above. Every
notification is deserialised back into `ServerNotification`, and the ordering
rules are checked: items start before they complete, no delta follows a
completed item, every turn ends with exactly one `turn/completed`.
