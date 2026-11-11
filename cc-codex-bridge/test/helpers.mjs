// Test client: spawns the bridge on stdio, validates every message it sends
// against the Codex app-server schema, and checks the ordering invariants.
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { lineReader } from "../src/jsonrpc.mjs";
import { loadBundle, SchemaValidator, validateOutgoing } from "../src/schema.mjs";

export const ROOT = path.join(path.dirname(fileURLToPath(import.meta.url)), "..");
export const validator = new SchemaValidator(loadBundle());

export function tempDir(prefix = "ccb-test-") {
  return mkdtempSync(path.join(os.tmpdir(), prefix));
}

/** Asserts the turn/item ordering rules over a stream of notifications. */
export class InvariantChecker {
  constructor() {
    this.items = new Map(); // item id -> "started" | "completed"
    this.turns = new Map(); // turn id -> "started" | "ended"
    this.errors = [];
  }

  observe(message) {
    const { method, params } = message;
    if (method === "turn/started") {
      if (this.turns.has(params.turn.id)) this.errors.push(`turn ${params.turn.id} started twice`);
      this.turns.set(params.turn.id, "started");
    } else if (method === "turn/completed") {
      if (this.turns.get(params.turn.id) !== "started") this.errors.push(`turn ${params.turn.id} ended without being in progress`);
      this.turns.set(params.turn.id, "ended");
      for (const item of params.turn.items) {
        if (this.items.get(item.id) !== "completed") this.errors.push(`turn ended with item ${item.id} not completed`);
      }
    } else if (method === "item/started") {
      if (this.items.has(params.item.id)) this.errors.push(`item ${params.item.id} started twice`);
      if (this.turns.get(params.turnId) !== "started") this.errors.push(`item ${params.item.id} started outside a running turn`);
      this.items.set(params.item.id, "started");
    } else if (method === "item/completed") {
      if (this.items.get(params.item.id) !== "started") this.errors.push(`item ${params.item.id} completed without being open`);
      if (this.turns.get(params.turnId) !== "started") this.errors.push(`item ${params.item.id} completed outside a running turn`);
      this.items.set(params.item.id, "completed");
    } else if (method === "item/agentMessage/delta") {
      if (this.items.get(params.itemId) !== "started") this.errors.push(`delta for item ${params.itemId} that is not open`);
    }
  }
}

export class BridgeClient {
  constructor({ config = {}, stateDir = tempDir(), env = {} } = {}) {
    this.stateDir = stateDir;
    mkdirSync(stateDir, { recursive: true });
    const configPath = path.join(stateDir, "config.json");
    writeFileSync(configPath, JSON.stringify({ stateDir, ...config }));
    this.child = spawn(process.execPath, [path.join(ROOT, "bin", "cc-codex-bridge.mjs"), "app-server", "--config", configPath], {
      stdio: ["pipe", "pipe", "pipe"],
      env: { ...process.env, NODE_NO_WARNINGS: "1", ...env },
    });
    this.stderr = "";
    this.child.stderr.on("data", (chunk) => (this.stderr += chunk));
    this.nextId = 1;
    this.pending = new Map();
    this.messages = []; // everything the bridge sent
    this.notifications = [];
    this.serverRequests = [];
    this.schemaErrors = [];
    this.invariants = new InvariantChecker();
    this.waiters = [];
    this.child.stdout.setEncoding("utf8");
    this.child.stdout.on("data", lineReader((message) => this.onMessage(message)));
    this.exited = new Promise((resolve) => this.child.on("exit", resolve));
  }

  onMessage(message) {
    this.messages.push(message);
    const isResponse = message.method === undefined;
    const pending = isResponse ? this.pending.get(message.id) : undefined;
    const errors = validateOutgoing(validator, message, pending?.method);
    if (errors.length) this.schemaErrors.push({ message, errors });
    if (isResponse) {
      this.pending.delete(message.id);
      if (message.error) pending?.reject(Object.assign(new Error(message.error.message), { code: message.error.code }));
      else pending?.resolve(message.result);
    } else if (message.id !== undefined) {
      this.serverRequests.push(message);
    } else {
      this.notifications.push(message);
      this.invariants.observe(message);
    }
    this.waiters = this.waiters.filter((waiter) => !waiter(message));
  }

  send(message) {
    this.child.stdin.write(JSON.stringify(message) + "\n");
  }

  request(method, params) {
    const id = this.nextId++;
    const promise = new Promise((resolve, reject) => this.pending.set(id, { method, resolve, reject }));
    this.send({ id, method, params });
    return promise;
  }

  respond(id, result) {
    this.send({ id, result });
  }

  async initialize() {
    const result = await this.request("initialize", {
      clientInfo: { name: "ccb-test", title: null, version: "0" },
      capabilities: null,
    });
    this.send({ method: "initialized" });
    return result;
  }

  /** Resolve with the first message (past or future) matching `predicate`. */
  waitFor(predicate, { timeoutMs = 20_000, what = "message", from = 0 } = {}) {
    const existing = this.messages.slice(from).find(predicate);
    if (existing) return Promise.resolve(existing);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        reject(new Error(`timed out waiting for ${what}\nstderr:\n${this.stderr}\nlast messages:\n${this.messages.slice(-8).map((m) => JSON.stringify(m).slice(0, 300)).join("\n")}`));
      }, timeoutMs);
      this.waiters.push((message) => {
        if (!predicate(message)) return false;
        clearTimeout(timer);
        resolve(message);
        return true;
      });
    });
  }

  turnCompleted(turnId, options) {
    return this.waitFor((m) => m.method === "turn/completed" && m.params.turn.id === turnId, { what: `turn ${turnId} to complete`, ...options });
  }

  async close() {
    this.child.stdin.end();
    const timer = setTimeout(() => this.child.kill("SIGKILL"), 3000);
    await this.exited;
    clearTimeout(timer);
  }

  /** Throw if any message broke the schema or the ordering rules. */
  assertClean() {
    if (this.schemaErrors.length) {
      const first = this.schemaErrors[0];
      throw new Error(`${this.schemaErrors.length} schema violation(s); first: ${first.errors.join("; ")}\n${JSON.stringify(first.message).slice(0, 600)}`);
    }
    if (this.invariants.errors.length) throw new Error(`ordering violations: ${this.invariants.errors.join("; ")}`);
  }
}
