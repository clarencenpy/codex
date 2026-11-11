// Milestone 1: a client can connect, initialize, list threads and models, and
// every message passes schema validation. No engine is started.
import test from "node:test";
import assert from "node:assert/strict";
import net from "node:net";
import { randomBytes } from "node:crypto";
import path from "node:path";
import { BridgeClient, tempDir, validator } from "./helpers.mjs";
import { Bridge } from "../src/server.mjs";
import { loadConfig } from "../src/config.mjs";
import { serveWebSocket, parseListen } from "../src/transports.mjs";
import { validateOutgoing } from "../src/schema.mjs";

const engine = { engineCommand: [process.execPath, path.join(import.meta.dirname, "fake-claude.mjs")] };

test("handshake, empty thread list, stubs and method-not-found over stdio", async () => {
  const client = new BridgeClient({ config: { ...engine, tmuxSocket: `ccbt-${process.pid}-f` } });
  try {
    await assert.rejects(client.request("thread/list", {}), /Not initialized/);
    const init = await client.initialize();
    assert.match(init.userAgent, /^cc-codex-bridge\//);
    assert.deepEqual(await client.request("thread/list", {}), { data: [], nextCursor: null, backwardsCursor: null });
    assert.deepEqual((await client.request("model/list", {})).data, []);
    assert.equal((await client.request("account/read", {})).requiresOpenaiAuth, false);
    await client.request("config/read", {});
    await assert.rejects(client.request("fs/readFile", { path: "/etc/passwd" }), (err) => err.code === -32601);
    await assert.rejects(client.request("thread/read", { threadId: "missing" }), /thread not found/);
    await assert.rejects(client.request("initialize", { clientInfo: { name: "x", title: null, version: "0" }, capabilities: null }), /Already initialized/);
    client.send("not an object");
    client.child.stdin.write("{broken json\n");
    assert.deepEqual((await client.request("thread/loaded/list", {})).data, []);
    client.assertClean();
  } finally {
    await client.close();
  }
});

test("model list comes from the config, with one default", async () => {
  const client = new BridgeClient({
    config: { ...engine, tmuxSocket: `ccbt-${process.pid}-m`, models: ["sonnet", { id: "opus", displayName: "Claude Opus", isDefault: true }] },
  });
  try {
    await client.initialize();
    const { data } = await client.request("model/list", {});
    assert.deepEqual(data.map((m) => [m.id, m.displayName, m.isDefault]), [["sonnet", "sonnet", false], ["opus", "Claude Opus", true]]);
    await assert.rejects(client.request("thread/start", { cwd: tempDir(), model: "gpt-5" }), /not in the bridge's configured model list/);
    client.assertClean();
  } finally {
    await client.close();
  }
});

test("a different engine version produces a warning, not a failure", async () => {
  const client = new BridgeClient({ config: { ...engine, tmuxSocket: `ccbt-${process.pid}-v` }, env: { FAKE_CLAUDE_VERSION: "9.9.9" } });
  try {
    await client.initialize();
    const warning = await client.waitFor((m) => m.method === "configWarning");
    assert.match(warning.params.summary, /9\.9\.9.*tested with/);
    client.assertClean();
  } finally {
    await client.close();
  }
});

/** Minimal WebSocket client over a Unix socket, enough for one exchange. */
function wsExchange(socketPath, messages) {
  return new Promise((resolve, reject) => {
    const socket = net.connect(socketPath);
    const received = [];
    let buffer = Buffer.alloc(0);
    let upgraded = false;
    const sendFrame = (text) => {
      const payload = Buffer.from(text);
      const mask = randomBytes(4);
      const header = payload.length < 126 ? Buffer.from([0x81, 0x80 | payload.length]) : Buffer.from([0x81, 0x80 | 126, payload.length >> 8, payload.length & 0xff]);
      socket.write(Buffer.concat([header, mask, Buffer.from(payload.map((byte, i) => byte ^ mask[i % 4]))]));
    };
    socket.on("connect", () => {
      socket.write(`GET /rpc HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: ${randomBytes(16).toString("base64")}\r\nSec-WebSocket-Version: 13\r\n\r\n`);
    });
    socket.on("data", (chunk) => {
      buffer = Buffer.concat([buffer, chunk]);
      if (!upgraded) {
        const end = buffer.indexOf("\r\n\r\n");
        if (end < 0) return;
        if (!buffer.subarray(0, end).toString().startsWith("HTTP/1.1 101")) return reject(new Error("no upgrade"));
        buffer = buffer.subarray(end + 4);
        upgraded = true;
        messages.forEach((message) => sendFrame(JSON.stringify(message)));
      }
      while (buffer.length >= 2) {
        let length = buffer[1] & 0x7f;
        let offset = 2;
        if (length === 126) {
          if (buffer.length < 4) return;
          length = buffer.readUInt16BE(2);
          offset = 4;
        }
        if (buffer.length < offset + length) return;
        received.push(JSON.parse(buffer.subarray(offset, offset + length).toString()));
        buffer = buffer.subarray(offset + length);
        if (received.length === messages.filter((m) => m.id !== undefined).length) {
          socket.end();
          resolve(received);
        }
      }
    });
    socket.on("error", reject);
  });
}

test("the same protocol is served as WebSocket over a Unix socket", async () => {
  const stateDir = tempDir();
  const bridge = new Bridge(loadConfig(path.join(stateDir, "none.json"), { stateDir, ...engine, tmuxSocket: `ccbt-${process.pid}-w` }));
  const socketPath = path.join(stateDir, "bridge.sock");
  const server = await serveWebSocket(parseListen(`unix://${socketPath}`), (connection) => bridge.handlers(connection));
  try {
    const replies = await wsExchange(socketPath, [
      { id: 1, method: "initialize", params: { clientInfo: { name: "ws", title: null, version: "0" }, capabilities: null } },
      { method: "initialized" },
      { id: 2, method: "thread/list", params: {} },
    ]);
    assert.equal(replies[0].id, 1);
    assert.deepEqual(validateOutgoing(validator, replies[0], "initialize"), []);
    assert.deepEqual(replies[1], { id: 2, result: { data: [], nextCursor: null, backwardsCursor: null } });
  } finally {
    server.close();
    bridge.stop();
  }
});

test("refuses to listen on a non-loopback address", async () => {
  await assert.rejects(serveWebSocket(parseListen("ws://0.0.0.0:0"), () => ({})), /non-loopback/);
});
