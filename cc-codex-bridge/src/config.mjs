// Bridge configuration. Everything has a default except the model list, which
// is deliberately empty until the user names the Claude models they want.
import { readFileSync, existsSync, mkdirSync } from "node:fs";
import os from "node:os";
import path from "node:path";

/** Claude Code version the hook, transcript and pane handling was tested with. */
export const TESTED_CLAUDE_VERSION = "2.1.322";

export function defaultConfigPath() {
  return (
    process.env.CCB_CONFIG ??
    path.join(process.env.XDG_CONFIG_HOME ?? path.join(os.homedir(), ".config"), "cc-codex-bridge", "config.json")
  );
}

export function loadConfig(configPath = defaultConfigPath(), overrides = {}) {
  let file = {};
  if (existsSync(configPath)) {
    try {
      file = JSON.parse(readFileSync(configPath, "utf8"));
    } catch (err) {
      throw new Error(`cannot parse ${configPath}: ${err.message}`);
    }
  }
  const merged = { ...file, ...overrides };
  const stateDir =
    merged.stateDir ??
    path.join(process.env.XDG_STATE_HOME ?? path.join(os.homedir(), ".local", "state"), "cc-codex-bridge");
  const config = {
    // Command that starts the engine. The bridge appends --settings and
    // --session-id/--resume (and --model when the client picks one). It runs
    // in the bridge's own environment, so whichever login the user set up for
    // `claude` (subscription login, or ANTHROPIC_API_KEY exported before the
    // bridge started) is the one used. The bridge never reads either.
    engineCommand: merged.engineCommand ?? ["claude"],
    // Claude models offered to the Codex client, e.g.
    // [{"id": "sonnet", "displayName": "Claude Sonnet", "isDefault": true}].
    models: merged.models ?? [],
    stateDir,
    tmuxSocket: merged.tmuxSocket ?? "ccb",
    paneWidth: merged.paneWidth ?? 200,
    paneHeight: merged.paneHeight ?? 50,
    testedClaudeVersion: merged.testedClaudeVersion ?? TESTED_CLAUDE_VERSION,
    // Answer Claude Code's "do you trust this folder" prompt automatically.
    // Off by default: the bridge fails thread start and says why instead.
    autoTrustWorkspace: merged.autoTrustWorkspace ?? false,
    startupTimeoutMs: merged.startupTimeoutMs ?? 45_000,
    interruptTimeoutMs: merged.interruptTimeoutMs ?? 5_000,
    pollIntervalMs: merged.pollIntervalMs ?? 150,
    // How long a turn may be silent before the pane is checked for a prompt
    // or a dead client that no hook reported.
    paneCheckAfterMs: merged.paneCheckAfterMs ?? 4_000,
  };
  if (!Array.isArray(config.engineCommand) || config.engineCommand.length === 0) {
    throw new Error("engineCommand must be a non-empty array, e.g. [\"claude\"]");
  }
  config.models = config.models.map((model) => (typeof model === "string" ? { id: model } : model));
  mkdirSync(path.join(config.stateDir, "sessions"), { recursive: true });
  return config;
}
