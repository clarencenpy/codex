import test from "node:test";
import assert from "node:assert/strict";
import { validator } from "./helpers.mjs";
import { validateOutgoing, RESPONSE_TYPES } from "../src/schema.mjs";

test("accepts a valid message and rejects an invalid one", () => {
  const ok = { userAgent: "x", codexHome: "/a", platformFamily: "unix", platformOs: "linux" };
  assert.deepEqual(validator.validate("InitializeResponse", ok), []);
  assert.notDeepEqual(validator.validate("InitializeResponse", { userAgent: 1 }), []);
});

test("checks notifications against the method they name", () => {
  assert.deepEqual(validateOutgoing(validator, { method: "thread/archived", params: { threadId: "t" } }), []);
  assert.notDeepEqual(validateOutgoing(validator, { method: "thread/archived", params: { threadId: 1 } }), []);
  assert.notDeepEqual(validateOutgoing(validator, { method: "not/a/method", params: {} }), []);
});

test("every response type the facade uses exists in the schema", () => {
  for (const type of Object.values(RESPONSE_TYPES)) assert.ok(validator.definition(type), type);
});

test("error responses validate", () => {
  assert.deepEqual(validateOutgoing(validator, { id: 1, error: { code: -32601, message: "method not found" } }), []);
});
