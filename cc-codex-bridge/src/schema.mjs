// Minimal JSON Schema validator for the subset of keywords the Codex
// app-server schema bundle uses: type, properties, required,
// additionalProperties, items, enum, anyOf, oneOf, allOf, $ref, minimum,
// minLength. Zero dependencies on purpose.
import { readFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import path from "node:path";

const here = path.dirname(fileURLToPath(import.meta.url));

/** Where the schema bundle is looked up, in order. */
export function defaultSchemaPaths() {
  const name = "codex_app_server_protocol.schemas.json";
  return [
    process.env.CCB_SCHEMA,
    path.join(here, "..", "schemas", name),
    path.join(here, "..", "..", "codex-rs", "app-server-protocol", "schema", "json", name),
  ].filter(Boolean);
}

export function loadBundle(explicitPath) {
  const candidates = explicitPath ? [explicitPath] : defaultSchemaPaths();
  for (const candidate of candidates) {
    if (existsSync(candidate)) return JSON.parse(readFileSync(candidate, "utf8"));
  }
  return null;
}

function typeOf(value) {
  if (value === null) return "null";
  if (Array.isArray(value)) return "array";
  if (typeof value === "number") return Number.isInteger(value) ? "integer" : "number";
  return typeof value;
}

function typeMatches(expected, actual) {
  return expected === actual || (expected === "number" && actual === "integer");
}

export class SchemaValidator {
  constructor(bundle) {
    this.bundle = bundle;
  }

  resolve(ref) {
    if (!ref.startsWith("#/")) throw new Error(`unsupported $ref ${ref}`);
    let node = this.bundle;
    for (const part of ref.slice(2).split("/")) {
      node = node?.[part.replace(/~1/g, "/").replace(/~0/g, "~")];
    }
    if (node === undefined) throw new Error(`unresolved $ref ${ref}`);
    return node;
  }

  /** Look a definition up by name: v2 first, then the top level. */
  definition(name) {
    const defs = this.bundle.definitions;
    const schema = defs.v2?.[name] ?? defs[name];
    if (!schema) throw new Error(`no schema definition named ${name}`);
    return schema;
  }

  /** Returns a list of error strings; empty means valid. */
  validate(name, value) {
    return this.check(this.definition(name), value, name);
  }

  check(schema, value, at) {
    if (schema === true || schema === undefined) return [];
    if (schema === false) return [`${at}: no value allowed`];
    const errors = [];
    if (schema.$ref) errors.push(...this.check(this.resolve(schema.$ref), value, at));
    const actual = typeOf(value);
    if (schema.type !== undefined) {
      const expected = Array.isArray(schema.type) ? schema.type : [schema.type];
      if (!expected.some((t) => typeMatches(t, actual))) {
        errors.push(`${at}: expected ${expected.join("|")}, got ${actual}`);
        return errors;
      }
    }
    if (schema.enum && !schema.enum.some((option) => option === value)) {
      errors.push(`${at}: ${JSON.stringify(value)} not in enum ${JSON.stringify(schema.enum)}`);
    }
    if (schema.minimum !== undefined && typeof value === "number" && value < schema.minimum) {
      errors.push(`${at}: ${value} < minimum ${schema.minimum}`);
    }
    if (schema.minLength !== undefined && typeof value === "string" && value.length < schema.minLength) {
      errors.push(`${at}: shorter than minLength ${schema.minLength}`);
    }
    for (const sub of schema.allOf ?? []) errors.push(...this.check(sub, value, at));
    if (schema.anyOf) {
      const results = schema.anyOf.map((sub) => this.check(sub, value, at));
      if (!results.some((r) => r.length === 0)) {
        errors.push(`${at}: matches no anyOf branch (${shortest(results)})`);
      }
    }
    if (schema.oneOf) {
      const results = schema.oneOf.map((sub) => this.check(sub, value, at));
      const passing = results.filter((r) => r.length === 0).length;
      if (passing === 0) errors.push(`${at}: matches no oneOf branch (${shortest(results)})`);
      else if (passing > 1) errors.push(`${at}: matches ${passing} oneOf branches`);
    }
    if (actual === "object") {
      for (const key of schema.required ?? []) {
        if (!(key in value)) errors.push(`${at}: missing required "${key}"`);
      }
      const props = schema.properties ?? {};
      for (const [key, child] of Object.entries(value)) {
        if (key in props) errors.push(...this.check(props[key], child, `${at}.${key}`));
        else if (schema.additionalProperties === false) errors.push(`${at}: unexpected "${key}"`);
        else if (typeof schema.additionalProperties === "object") {
          errors.push(...this.check(schema.additionalProperties, child, `${at}.${key}`));
        }
      }
    }
    if (actual === "array" && schema.items) {
      value.forEach((child, i) => errors.push(...this.check(schema.items, child, `${at}[${i}]`)));
    }
    return errors;
  }
}

/** For a failed union, report the branch that came closest. */
function shortest(results) {
  const best = results.reduce((a, b) => (b.length < a.length ? b : a));
  return best.slice(0, 3).join("; ");
}

/** Response type for each client request the facade implements. */
export const RESPONSE_TYPES = {
  initialize: "InitializeResponse",
  "thread/start": "ThreadStartResponse",
  "thread/resume": "ThreadResumeResponse",
  "thread/list": "ThreadListResponse",
  "thread/read": "ThreadReadResponse",
  "thread/archive": "ThreadArchiveResponse",
  "thread/loaded/list": "ThreadLoadedListResponse",
  "thread/unsubscribe": "ThreadUnsubscribeResponse",
  "thread/name/set": "ThreadSetNameResponse",
  "turn/start": "TurnStartResponse",
  "turn/interrupt": "TurnInterruptResponse",
  "model/list": "ModelListResponse",
  "account/read": "GetAccountResponse",
  "config/read": "ConfigReadResponse",
};

/**
 * Validate one outgoing wire message. `pendingMethod` is the client method a
 * response answers. Returns error strings.
 */
export function validateOutgoing(validator, message, pendingMethod) {
  if (message.error !== undefined) return validator.validate("JSONRPCError", message);
  if (message.method !== undefined && message.id !== undefined) {
    return validator.validate("ServerRequest", message);
  }
  if (message.method !== undefined) return validator.validate("ServerNotification", message);
  const type = RESPONSE_TYPES[pendingMethod];
  if (!type) return [`no response type known for ${pendingMethod}`];
  return validator.validate(type, message.result);
}
