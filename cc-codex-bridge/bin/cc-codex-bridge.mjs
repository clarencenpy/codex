#!/usr/bin/env node
// cc-codex-bridge app-server [--listen stdio://|ws://127.0.0.1:PORT|unix:///path] [--config FILE]
import { appendFileSync } from "node:fs";
import path from "node:path";
import { loadConfig, defaultConfigPath } from "../src/config.mjs";
import { Bridge, BRIDGE_VERSION } from "../src/server.mjs";
import { serveStdio, serveWebSocket, parseListen } from "../src/transports.mjs";

const USAGE = `cc-codex-bridge ${BRIDGE_VERSION}

Usage:
  cc-codex-bridge app-server [--listen URL] [--config FILE]

  --listen URL   stdio:// (default), ws://127.0.0.1:PORT, or unix:///path/to.sock
  --config FILE  bridge config (default: ${defaultConfigPath()})
`;

function parseArgs(argv) {
  const options = { listen: "stdio://", config: undefined, command: null };
  for (let i = 0; i < argv.length; i++) {
    const arg = argv[i];
    if (arg === "--listen") options.listen = argv[++i];
    else if (arg === "--config") options.config = argv[++i];
    else if (arg === "-h" || arg === "--help") options.help = true;
    else if (arg === "--version") options.version = true;
    else if (!options.command) options.command = arg;
    else throw new Error(`unexpected argument ${arg}`);
  }
  return options;
}

async function main() {
  let options;
  try {
    options = parseArgs(process.argv.slice(2));
  } catch (err) {
    process.stderr.write(`${err.message}\n\n${USAGE}`);
    process.exit(2);
  }
  if (options.version) return void process.stdout.write(`${BRIDGE_VERSION}\n`);
  if (options.help || options.command !== "app-server") {
    process.stderr.write(USAGE);
    process.exit(options.help ? 0 : 2);
  }
  const config = loadConfig(options.config);
  const logFile = path.join(config.stateDir, "bridge.log");
  // stdout carries the protocol on stdio, so logs go to stderr and a file.
  const log = (message) => {
    const line = `${new Date().toISOString()} ${message}\n`;
    process.stderr.write(line);
    try {
      appendFileSync(logFile, line);
    } catch {
      // logging must never take the bridge down
    }
  };
  const bridge = new Bridge(config, { log });
  await bridge.start();
  const makeHandlers = (connection) => {
    const handlers = bridge.handlers(connection);
    const previousClose = connection.onClose;
    connection.onClose = () => {
      bridge.connectionClosed(connection);
      previousClose?.();
    };
    return handlers;
  };
  const shutdown = () => {
    // tmux sessions are left running on purpose: they outlive the bridge.
    bridge.stop();
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
  if (options.listen === "stdio://" || options.listen === "stdio") {
    await serveStdio(makeHandlers, { log });
    shutdown();
  } else {
    const server = await serveWebSocket(parseListen(options.listen), makeHandlers, { log });
    const address = server.address();
    log(`listening on ${typeof address === "string" ? `unix://${address}` : `ws://${address.address}:${address.port}`}`);
  }
}

main().catch((err) => {
  process.stderr.write(`cc-codex-bridge: ${err.stack ?? err}\n`);
  process.exit(1);
});
