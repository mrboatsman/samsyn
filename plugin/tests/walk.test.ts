import { test } from "node:test";
import assert from "node:assert/strict";
import { walkDisk } from "../src/walk";

/** A fake adapter over a flat {folder: [entries]} map; entries ending in "/" are folders. */
function fake(tree: Record<string, string[]>) {
  const calls: string[] = [];
  return {
    calls,
    async list(dir: string) {
      calls.push(dir);
      const entries = tree[dir];
      if (!entries) throw new Error("ENOENT " + dir);
      const prefix = dir === "/" ? "" : dir + "/";
      return {
        files: entries.filter((e) => !e.endsWith("/")).map((e) => prefix + e),
        folders: entries.filter((e) => e.endsWith("/")).map((e) => prefix + e.slice(0, -1)),
      };
    },
  };
}

const tree = {
  "/": ["a.md", "script.js", "Notes/", ".obsidian/", ".git/", "Private/", "Shared/"],
  Notes: ["b.md", "style.css", "deep/", ".hidden/"],
  "Notes/deep": ["c.txt"],
  "Notes/.hidden": ["nope.md"],
  ".obsidian": ["app.json"],
  ".git": ["HEAD"],
  Private: ["secret.md"],
  Shared: ["Guest/"],
  "Shared/Guest": ["d.md"],
};

test("walks every file regardless of extension, skips dot folders", async () => {
  const a = fake(tree);
  const files = await walkDisk(a, "", () => true);
  assert.deepEqual(files.sort(), ["Notes/b.md", "Notes/deep/c.txt", "Notes/style.css", "Private/secret.md", "Shared/Guest/d.md", "a.md", "script.js"]);
  assert.ok(!a.calls.includes(".obsidian") && !a.calls.includes(".git") && !a.calls.includes("Notes/.hidden"));
});

test("descend prunes whole subtrees without listing them", async () => {
  const a = fake(tree);
  const files = await walkDisk(a, "", (f) => f !== "Private" && f !== "Shared");
  assert.deepEqual(files.sort(), ["Notes/b.md", "Notes/deep/c.txt", "Notes/style.css", "a.md", "script.js"]);
  assert.ok(!a.calls.includes("Private") && !a.calls.includes("Shared/Guest"));
});

test("walks a subfolder and strips slashes", async () => {
  const files = await walkDisk(fake(tree), "/Shared/", () => true);
  assert.deepEqual(files, ["Shared/Guest/d.md"]);
});

test("a folder that cannot be listed yields nothing", async () => {
  assert.deepEqual(await walkDisk(fake(tree), "Missing", () => true), []);
});
