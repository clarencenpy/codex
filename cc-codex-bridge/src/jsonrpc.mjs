// JSON-RPC 2.0 peer as Codex's app-server speaks it: one JSON object per
// message and no "jsonrpc" member on the wire.

export const ErrorCode = {
  InvalidRequest: -32600,
  MethodNotFound: -32601,
  InvalidParams: -32602,
  Internal: -32603,
};

export class RpcError extends Error {
  constructor(code, message, data) {
    super(message);
    this.code = code;
    this.data = data;
  }
}

/**
 * One client connection. `send(obj)` writes a message; the transport calls
 * `receive(obj)` for each parsed message and `close()` when the peer goes.
 */
export class Connection {
  constructor({ send, onRequest, onNotification, onClose, log }) {
    this.sendRaw = send;
    this.onRequest = onRequest;
    this.onNotification = onNotification;
    this.onClose = onClose;
    this.log = log ?? (() => {});
    this.nextId = 1;
    this.pending = new Map(); // server-initiated request id -> {resolve, reject}
    this.closed = false;
    /** Client method per in-flight request id, for response validation. */
    this.inFlight = new Map();
    this.outgoingHook = null; // (message, pendingMethod) => void, set by tests
  }

  write(message, pendingMethod) {
    if (this.closed) return;
    this.outgoingHook?.(message, pendingMethod);
    this.sendRaw(message);
  }

  notify(method, params) {
    this.write(params === undefined ? { method } : { method, params });
  }

  /** Server-initiated request; resolves with the client's result. */
  request(method, params) {
    const id = this.nextId++;
    const promise = new Promise((resolve, reject) => this.pending.set(id, { resolve, reject }));
    this.write({ id, method, params });
    return { id, promise };
  }

  async receive(message) {
    if (message === null || typeof message !== "object" || Array.isArray(message)) {
      this.log("dropping a message that is not a JSON object");
      return;
    }
    const hasId = message.id !== undefined && message.id !== null;
    if (typeof message.method === "string") {
      if (!hasId) {
        try {
          await this.onNotification(message.method, message.params);
        } catch (err) {
          this.log(`notification ${message.method} failed: ${err.stack ?? err}`);
        }
        return;
      }
      this.inFlight.set(message.id, message.method);
      try {
        const result = await this.onRequest(message.method, message.params);
        this.write({ id: message.id, result: result ?? {} }, message.method);
      } catch (err) {
        const known = err instanceof RpcError;
        if (!known) this.log(`request ${message.method} failed: ${err.stack ?? err}`);
        const error = { code: known ? err.code : ErrorCode.Internal, message: String(err.message ?? err) };
        if (known && err.data !== undefined) error.data = err.data;
        this.write({ id: message.id, error });
      } finally {
        this.inFlight.delete(message.id);
      }
      return;
    }
    if (hasId && this.pending.has(message.id)) {
      const { resolve, reject } = this.pending.get(message.id);
      this.pending.delete(message.id);
      if (message.error) reject(new RpcError(message.error.code, message.error.message, message.error.data));
      else resolve(message.result);
      return;
    }
    this.log(`dropping unrecognised message: ${JSON.stringify(message).slice(0, 200)}`);
  }

  close() {
    if (this.closed) return;
    this.closed = true;
    for (const { reject } of this.pending.values()) reject(new Error("connection closed"));
    this.pending.clear();
    this.onClose?.();
  }
}

/** Split a byte stream into newline-delimited JSON messages. */
export function lineReader(onMessage, onParseError) {
  let buffer = "";
  return (chunk) => {
    buffer += chunk;
    let newline;
    while ((newline = buffer.indexOf("\n")) >= 0) {
      const line = buffer.slice(0, newline).trim();
      buffer = buffer.slice(newline + 1);
      if (!line) continue;
      let parsed;
      try {
        parsed = JSON.parse(line);
      } catch (err) {
        onParseError?.(err, line);
        continue;
      }
      onMessage(parsed);
    }
  };
}
