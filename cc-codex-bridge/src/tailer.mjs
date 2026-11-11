// Tail a JSONL file by byte offset. Only complete lines are consumed, so a
// half-written line is picked up on the next read and nothing is replayed.
import { openSync, readSync, closeSync, fstatSync } from "node:fs";

/**
 * Read complete lines from `file` starting at `offset`.
 * Returns { lines: [{text, end}], offset } where `end` is the byte offset
 * just past that line.
 */
export function readLinesFrom(file, offset) {
  let fd;
  try {
    fd = openSync(file, "r");
  } catch {
    return { lines: [], offset };
  }
  try {
    const size = fstatSync(fd).size;
    if (size < offset) offset = 0; // truncated or replaced
    if (size === offset) return { lines: [], offset };
    const buffer = Buffer.alloc(size - offset);
    const read = readSync(fd, buffer, 0, buffer.length, offset);
    const lines = [];
    let start = 0;
    for (;;) {
      const newline = buffer.indexOf(0x0a, start);
      if (newline < 0 || newline >= read) break;
      lines.push({ text: buffer.toString("utf8", start, newline), end: offset + newline + 1 });
      start = newline + 1;
    }
    return { lines, offset: offset + start };
  } finally {
    closeSync(fd);
  }
}
