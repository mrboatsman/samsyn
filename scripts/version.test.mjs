import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { nextVersion, bumpBase, format, compare, applyVersion, checkConsistency, setCargoVersion, setCargoLockVersion } from "./version.mjs";

const next = (cur, channel, level, base) => nextVersion(cur, { channel, level, base });

test("stable bumps", () => {
  assert.equal(next("0.1.0", "stable", "patch"), "0.1.1");
  assert.equal(next("0.1.0", "stable", "minor"), "0.2.0");
  assert.equal(next("0.1.0", "stable", "major"), "1.0.0");
  assert.equal(next("0.1.0", "stable", "none"), "0.1.0", "re-release current version (first release)");
});

test("beta lines", () => {
  assert.equal(next("0.1.0", "beta", "minor"), "0.2.0-beta.1");
  assert.equal(next("0.2.0-beta.1", "beta", "none"), "0.2.0-beta.2");
  assert.equal(next("0.2.0-beta.2", "beta", "minor"), "0.2.0-beta.3", "minor on a beta stays on its base");
  assert.equal(next("0.2.0-beta.2", "beta", "major"), "1.0.0-beta.1");
  assert.throws(() => next("0.1.0", "beta", "none"), /pick --level/);
});

test("beta of a version that is in manifest.json but not tagged yet", () => {
  const untagged = (cur, level, base) => nextVersion(cur, { channel: "beta", level, base, currentUntagged: true });
  assert.equal(untagged("0.1.0", "auto", "0.1.0"), "0.1.0-beta.1");
  assert.equal(untagged("0.1.0", "none"), "0.1.0-beta.1");
  assert.equal(untagged("0.1.0", "minor"), "0.2.0-beta.1", "explicit level still bumps");
  assert.equal(untagged("0.2.0-beta.3", "none"), "0.2.0-beta.4");
});

test("finishing a beta", () => {
  assert.equal(next("0.2.0-beta.3", "stable", "none"), "0.2.0");
  assert.equal(next("0.2.0-beta.3", "stable", "patch"), "0.2.0");
  assert.equal(next("0.2.0-beta.3", "stable", "minor"), "0.2.0");
  assert.equal(next("0.2.0-beta.3", "stable", "major"), "1.0.0");
  assert.equal(next("1.0.0-beta.1", "stable", "major"), "1.0.0");
});

test("auto uses git-cliff's base", () => {
  assert.equal(next("0.1.0", "stable", "auto", "0.1.1"), "0.1.1");
  assert.equal(next("0.1.0", "beta", "auto", "0.2.0"), "0.2.0-beta.1");
  assert.equal(next("0.2.0-beta.1", "beta", "auto", "0.2.0"), "0.2.0-beta.2");
  assert.equal(next("0.2.0-beta.1", "beta", "auto", "0.3.0"), "0.3.0-beta.1");
  assert.equal(next("0.2.0-beta.4", "stable", "auto", "0.2.0"), "0.2.0");
  assert.equal(next("0.1.0", "stable", "auto", "0.1.0"), "0.1.0", "first release: initial_tag equals manifest");
  assert.equal(next("0.2.0-beta.1", "beta", "auto", "0.1.1"), "0.2.0-beta.2", "stale base never moves backwards");
  assert.throws(() => next("0.1.0", "stable", "auto"), /--base/);
  assert.throws(() => next("0.1.0", "stable", "auto", "0.2.0-beta.1"), /stable/);
});

test("ordering", () => {
  assert.equal(compare("0.2.0-beta.1", "0.2.0"), -1);
  assert.equal(compare("0.2.0-beta.2", "0.2.0-beta.1"), 1);
  assert.equal(compare("0.10.0", "0.9.9"), 1);
  assert.equal(format(bumpBase("v1.2.3", "patch")), "1.2.4");
});

test("applyVersion writes every carrier; versions.json only for stable", () => {
  const root = fs.mkdtempSync(path.join(os.tmpdir(), "samsyn-version-"));
  fs.mkdirSync(path.join(root, "plugin"));
  const w = (p, o) => fs.writeFileSync(path.join(root, p), JSON.stringify(o, null, 2) + "\n");
  w("manifest.json", { id: "samsyn", version: "0.1.0", minAppVersion: "1.5.0" });
  w("versions.json", { "0.1.0": "1.5.0" });
  w("plugin/package.json", { name: "samsyn", version: "0.1.0" });
  w("plugin/package-lock.json", { name: "samsyn", version: "0.1.0", packages: { "": { name: "samsyn", version: "0.1.0" } } });
  fs.mkdirSync(path.join(root, "server"));
  const cargo = '[package]\nname = "samsyn-server"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nserde = { version = "1.0" }\n';
  const lock = '[[package]]\nname = "other"\nversion = "0.1.0"\n\n[[package]]\nname = "samsyn-server"\nversion = "0.1.0"\ndependencies = []\n';
  fs.writeFileSync(path.join(root, "server/Cargo.toml"), cargo);
  fs.writeFileSync(path.join(root, "server/Cargo.lock"), lock);

  applyVersion("0.2.0-beta.1", root);
  const r = (p) => JSON.parse(fs.readFileSync(path.join(root, p), "utf8"));
  const t = (p) => fs.readFileSync(path.join(root, p), "utf8");
  assert.equal(r("manifest.json").version, "0.2.0-beta.1");
  assert.match(t("server/Cargo.toml"), /^version = "0\.2\.0-beta\.1"$/m);
  assert.match(t("server/Cargo.toml"), /serde = \{ version = "1\.0" \}/, "dependency versions untouched");
  assert.match(t("server/Cargo.lock"), /name = "samsyn-server"\nversion = "0\.2\.0-beta\.1"/);
  assert.match(t("server/Cargo.lock"), /name = "other"\nversion = "0\.1\.0"/, "other lock entries untouched");
  assert.equal(r("plugin/package.json").version, "0.2.0-beta.1");
  assert.equal(r("plugin/package-lock.json").packages[""].version, "0.2.0-beta.1");
  assert.deepEqual(r("versions.json"), { "0.1.0": "1.5.0" });
  assert.deepEqual(checkConsistency(root), []);

  applyVersion("0.2.0", root);
  assert.deepEqual(r("versions.json"), { "0.1.0": "1.5.0", "0.2.0": "1.5.0" });
  assert.deepEqual(checkConsistency(root), []);

  w("plugin/package.json", { name: "samsyn", version: "0.1.0" });
  assert.match(checkConsistency(root)[0], /package\.json has 0\.1\.0/);
  fs.writeFileSync(path.join(root, "server/Cargo.toml"), cargo);
  assert.ok(checkConsistency(root).some((p) => /Cargo\.toml has 0\.1\.0/.test(p)));
});

test("cargo version rewrites only the package entry", () => {
  const toml = '[package]\nname = "x"\nversion = "0.1.0"\n\n[dependencies]\ny = { version = "0.1.0" }\n';
  assert.equal(setCargoVersion(toml, "1.2.3"), '[package]\nname = "x"\nversion = "1.2.3"\n\n[dependencies]\ny = { version = "0.1.0" }\n');
  assert.throws(() => setCargoVersion('[dependencies]\ny = "1"\n', "1.0.0"));
  assert.throws(() => setCargoLockVersion('[[package]]\nname = "y"\nversion = "1"\n', "1.0.0"));
});
