// Transports: newline-delimited JSON on stdio, and WebSocket (one JSON message
// per text frame) over TCP or a Unix socket, which is how the Codex client
// reaches a remote app-server.
import net from "node:net";
import http from "node:http";
import { createHash } from "node:crypto";
import { existsSync, unlinkSync } from "node:fs";
import { Connection, lineReader } from "./jsonrpc.mjs";

export function serveStdio(makeHandlers, { input = process.stdin, output = process.stdout, log } = {}) {
  return new Promise((resolve) => {
    const connection = new Connection({
      send: (message) => output.write(JSON.stringify(message) + "\n"),
      log,
      onClose: resolve,
      ...makeHandlersLazy(makeHandlers, () => connection),
    });
    // Like Codex's own server, input that is not JSON is logged and dropped:
    // the schema has no error shape without a request id.
    const feed = lineReader(
      (message) => connection.receive(message),
      (err) => log?.(`dropping unparseable input: ${err.message}`),
    );
    input.setEncoding("utf8");
    input.on("data", feed);
    input.on("end", () => connection.close());
    input.on("close", () => connection.close());
  });
}

function makeHandlersLazy(makeHandlers, getConnection) {
  let handlers;
  const get = () => (handlers ??= makeHandlers(getConnection()));
  return {
    onRequest: (method, params) => get().onRequest(method, params),
    onNotification: (method, params) => get().onNotification(method, params),
  };
}

/** Parse `ws://host:port` or `unix:///path/to.sock`. */
export function parseListen(spec) {
  if (spec.startsWith("unix://")) return { kind: "unix", path: spec.slice("unix://".length) };
  const url = new URL(spec);
  if (url.protocol !== "ws:") throw new Error(`unsupported listen address ${spec}`);
  return { kind: "tcp", host: url.hostname, port: Number(url.port || 0) };
}

const WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

export function serveWebSocket(listen, makeHandlers, { log = () => {} } = {}) {
  const server = http.createServer((req, res) => {
    res.writeHead(426, { "content-type": "text/plain" });
    res.end("WebSocket upgrade required\n");
  });
  server.on("upgrade", (req, socket) => {
    const key = req.headers["sec-websocket-key"];
    if (!key || String(req.headers.upgrade).toLowerCase() !== "websocket") {
      socket.destroy();
      return;
    }
    const accept = createHash("sha1").update(key + WS_GUID).digest("base64");
    socket.write(
      "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );
    attachWebSocket(socket, makeHandlers, log);
  });
  return new Promise((resolve, reject) => {
    server.once("error", reject);
    if (listen.kind === "unix") {
      if (existsSync(listen.path)) unlinkSync(listen.path);
      server.listen(listen.path, () => resolve(server));
    } else {
      if (!isLoopback(listen.host)) {
        reject(new Error(`refusing to listen on non-loopback host ${listen.host}: this bridge is single-user, single-machine`));
        return;
      }
      server.listen(listen.port, listen.host, () => resolve(server));
    }
  });
}

function isLoopback(host) {
  return host === "localhost" || host === "::1" || host === "[::1]" || net.isIPv4(host) && host.startsWith("127.");
}

function frame(opcode, payload) {
  const length = payload.length;
  let header;
  if (length < 126) header = Buffer.from([0x80 | opcode, length]);
  else if (length < 65536) {
    header = Buffer.alloc(4);
    header[0] = 0x80 | opcode;
    header[1] = 126;
    header.writeUInt16BE(length, 2);
  } else {
    header = Buffer.alloc(10);
    header[0] = 0x80 | opcode;
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(length), 2);
  }
  return Buffer.concat([header, payload]);
}

function attachWebSocket(socket, makeHandlers, log) {
  const connection = new Connection({
    send: (message) => socket.writable && socket.write(frame(0x1, Buffer.from(JSON.stringify(message)))),
    log,
    ...makeHandlersLazy(makeHandlers, () => connection),
  });
  let buffered = Buffer.alloc(0);
  let fragments = [];
  socket.on("data", (chunk) => {
    buffered = Buffer.concat([buffered, chunk]);
    for (;;) {
      if (buffered.length < 2) return;
      const fin = (buffered[0] & 0x80) !== 0;
      const opcode = buffered[0] & 0x0f;
      const masked = (buffered[1] & 0x80) !== 0;
      let length = buffered[1] & 0x7f;
      let offset = 2;
      if (length === 126) {
        if (buffered.length < 4) return;
        length = buffered.readUInt16BE(2);
        offset = 4;
      } else if (length === 127) {
        if (buffered.length < 10) return;
        length = Number(buffered.readBigUInt64BE(2));
        offset = 10;
      }
      const maskOffset = offset;
      if (masked) offset += 4;
      if (buffered.length < offset + length) return;
      const payload = Buffer.from(buffered.subarray(offset, offset + length));
      if (masked) {
        const mask = buffered.subarray(maskOffset, maskOffset + 4);
        for (let i = 0; i < payload.length; i++) payload[i] ^= mask[i % 4];
      }
      buffered = buffered.subarray(offset + length);
      if (opcode === 0x8) {
        socket.end(frame(0x8, Buffer.alloc(0)));
        return;
      }
      if (opcode === 0x9) {
        socket.write(frame(0xa, payload));
        continue;
      }
      if (opcode === 0xa) continue;
      fragments.push(payload);
      if (!fin) continue;
      const text = Buffer.concat(fragments).toString("utf8");
      fragments = [];
      let message;
      try {
        message = JSON.parse(text);
      } catch (err) {
        log(`dropping unparseable frame: ${err.message}`);
        continue;
      }
      connection.receive(message);
    }
  });
  socket.on("end", () => socket.end());
  socket.on("close", () => connection.close());
  socket.on("error", () => connection.close());
}
