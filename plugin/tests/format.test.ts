import { test } from "node:test";
import assert from "node:assert/strict";
import { formatBytes, utf8Length } from "../src/format";

test("utf8Length matches TextEncoder", () => {
  for (const s of ["", "abc", "åäö", "€", "😀 emoji", "\ud800 lone surrogate"]) {
    assert.equal(utf8Length(s), new TextEncoder().encode(s).byteLength, JSON.stringify(s));
  }
});

test("formatBytes", () => {
  assert.equal(formatBytes(0), "0 B");
  assert.equal(formatBytes(1023), "1023 B");
  assert.equal(formatBytes(1536), "1.5 KB");
  assert.equal(formatBytes(12 * 1024 * 1024), "12 MB");
  assert.equal(formatBytes(315045 * 1024 * 1024), "308 GB");
});
