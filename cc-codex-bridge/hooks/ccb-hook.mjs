#!/usr/bin/env node
// Claude Code hook: append the hook's stdin JSON, with a timestamp, as one
// line to the events file given as argv[2]. Prints nothing and always exits 0
// so it can never change what Claude Code does.
import { appendFileSync } from "node:fs";

const eventsPath = process.argv[2];
let raw = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => (raw += chunk));
process.stdin.on("end", () => {
  if (!eventsPath) return;
  let payload;
  try {
    payload = JSON.parse(raw);
  } catch {
    payload = { hook_event_name: "Unparseable", raw };
  }
  try {
    // One appendFileSync call is one O_APPEND write, so concurrent hooks
    // cannot interleave inside a line.
    appendFileSync(eventsPath, JSON.stringify({ ts: Date.now(), payload }) + "\n");
  } catch {
    // Never fail the hook.
  }
});
