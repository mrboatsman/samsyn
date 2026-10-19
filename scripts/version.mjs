#!/usr/bin/env node
// Release version tooling. Single source of truth for the version is
// /manifest.json; this script computes the next version and writes it to every
// file that carries it, plugin and server alike. See docs/DEVELOPMENT.md "Releasing".
//
//   node scripts/version.mjs current
//   node scripts/version.mjs next --channel beta|stable --level auto|patch|minor|major|none [--base X.Y.Z] [--current-untagged]
//   node scripts/version.mjs set X.Y.Z[-beta.N]
//
// Versions are X.Y.Z (stable) or X.Y.Z-beta.N (pre-release). GitHub pre-releases
// are what BRAT serves to testers who opt into them.

import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const PRE = "beta";
const RE = /^v?(\d+)\.(\d+)\.(\d+)(?:-beta\.(\d+))?$/;

export function parse(v) {
  const m = RE.exec(String(v).trim());
  if (!m) throw new Error(`Unsupported version "${v}" (want X.Y.Z or X.Y.Z-${PRE}.N)`);
  return { major: +m[1], minor: +m[2], patch: +m[3], pre: m[4] === undefined ? null : +m[4] };
}

export function format({ major, minor, patch, pre }) {
  const base = `${major}.${minor}.${patch}`;
  return pre === null ? base : `${base}-${PRE}.${pre}`;
}

export function compare(a, b) {
  const x = typeof a === "string" ? parse(a) : a;
  const y = typeof b === "string" ? parse(b) : b;
  for (const k of ["major", "minor", "patch"]) if (x[k] !== y[k]) return x[k] < y[k] ? -1 : 1;
  if (x.pre === y.pre) return 0;
  if (x.pre === null) return 1; // release > its pre-releases
  if (y.pre === null) return -1;
  return x.pre < y.pre ? -1 : 1;
}

/** Bump the X.Y.Z part with npm semantics: bumping a pre-release first lands on
 *  its own base (0.2.0-beta.3 + patch -> 0.2.0, + minor -> 0.2.0, + major -> 1.0.0). */
export function bumpBase(current, level) {
  const c = typeof current === "string" ? parse(current) : current;
  let { major, minor, patch } = c;
  const isPre = c.pre !== null;
  switch (level) {
    case "major":
      if (!isPre || minor !== 0 || patch !== 0) { major += 1; minor = 0; patch = 0; }
      else { minor = 0; patch = 0; }
      break;
    case "minor":
      if (!isPre || patch !== 0) { minor += 1; patch = 0; }
      break;
    case "patch":
      if (!isPre) patch += 1;
      break;
    case "none":
      break;
    default:
      throw new Error(`Unknown level "${level}"`);
  }
  return { major, minor, patch, pre: null };
}

/**
 * Next version from the current one.
 *  channel: "stable" -> X.Y.Z, "beta" -> X.Y.Z-beta.N
 *  level:   patch|minor|major|none, or "auto" with `base` (from `git cliff --bumped-version`)
 *  currentUntagged: the current X.Y.Z has no tag yet, so a beta of it is allowed
 */
export function nextVersion(current, { channel, level, base, currentUntagged = false }) {
  const cur = parse(current);
  let target;
  if (level === "auto") {
    if (!base) throw new Error("level=auto needs --base (git cliff --bumped-version)");
    target = parse(base);
    if (target.pre !== null) throw new Error(`--base must be a stable version, got ${base}`);
    // A base behind the current line means nothing to bump; stay on the current base.
    const curBase = { ...cur, pre: null };
    if (compare(target, curBase) < 0) target = curBase;
  } else {
    target = bumpBase(cur, level);
  }

  let next;
  if (channel === "stable") {
    next = target;
  } else if (channel === "beta") {
    const sameBase = compare({ ...cur, pre: null }, target) === 0;
    if (sameBase && cur.pre !== null) next = { ...target, pre: cur.pre + 1 };
    else if (sameBase && currentUntagged) next = { ...target, pre: 1 }; // first beta of an unreleased X.Y.Z
    else if (sameBase && (level === "none" || level === "auto")) {
      throw new Error(`Current ${current} is already released; pick --level patch|minor|major for a new beta line`);
    } else next = { ...target, pre: 1 };
  } else {
    throw new Error(`Unknown channel "${channel}"`);
  }

  // A beta of an untagged X.Y.Z sorts below it by design; everything else must move forward.
  const floor = currentUntagged ? { ...cur, pre: null, patch: cur.patch - 1 } : cur;
  if (compare(next, floor) < 0) throw new Error(`Computed ${format(next)} is older than current ${current}`);
  return format(next);
}

function readJson(p) { return JSON.parse(fs.readFileSync(p, "utf8")); }
function writeJson(p, obj) { fs.writeFileSync(p, JSON.stringify(obj, null, 2) + "\n"); }

export function currentVersion(root = ROOT) {
  return readJson(path.join(root, "manifest.json")).version;
}

/** Write `version` everywhere it lives. versions.json (Obsidian's version ->
 *  minAppVersion map) only gets stable releases. Returns the files touched. */
export function applyVersion(version, root = ROOT) {
  const v = parse(version);
  const touched = [];

  const manifestPath = path.join(root, "manifest.json");
  const manifest = readJson(manifestPath);
  manifest.version = format(v);
  writeJson(manifestPath, manifest);
  touched.push("manifest.json");

  const pkgPath = path.join(root, "plugin", "package.json");
  const pkg = readJson(pkgPath);
  pkg.version = format(v);
  writeJson(pkgPath, pkg);
  touched.push("plugin/package.json");

  const lockPath = path.join(root, "plugin", "package-lock.json");
  if (fs.existsSync(lockPath)) {
    const lock = readJson(lockPath);
    lock.version = format(v);
    if (lock.packages && lock.packages[""]) lock.packages[""].version = format(v);
    writeJson(lockPath, lock);
    touched.push("plugin/package-lock.json");
  }

  // The server reports CARGO_PKG_VERSION from /health and --version; keep it in step.
  const cargoPath = path.join(root, "server", "Cargo.toml");
  if (fs.existsSync(cargoPath)) {
    fs.writeFileSync(cargoPath, setCargoVersion(fs.readFileSync(cargoPath, "utf8"), format(v)));
    touched.push("server/Cargo.toml");
    const cargoLockPath = path.join(root, "server", "Cargo.lock");
    if (fs.existsSync(cargoLockPath)) {
      fs.writeFileSync(cargoLockPath, setCargoLockVersion(fs.readFileSync(cargoLockPath, "utf8"), format(v)));
      touched.push("server/Cargo.lock");
    }
  }

  if (v.pre === null) {
    const versionsPath = path.join(root, "versions.json");
    const versions = fs.existsSync(versionsPath) ? readJson(versionsPath) : {};
    versions[format(v)] = manifest.minAppVersion;
    writeJson(versionsPath, versions);
    touched.push("versions.json");
  }
  return touched;
}

const CARGO_PKG_VERSION = /^(\[package\][^[]*?^version = ")([^"]*)(")/ms;
const CARGO_LOCK_VERSION = /^(name = "samsyn-server"\nversion = ")([^"]*)(")/m;

/** Cargo.toml text with the [package] version replaced. Cargo accepts pre-release suffixes. */
export function setCargoVersion(toml, version) {
  if (!CARGO_PKG_VERSION.test(toml)) throw new Error("server/Cargo.toml: no [package] version found");
  return toml.replace(CARGO_PKG_VERSION, `$1${version}$3`);
}

/** Cargo.lock text with the samsyn-server entry replaced, so `--locked` builds stay happy. */
export function setCargoLockVersion(lock, version) {
  if (!CARGO_LOCK_VERSION.test(lock)) throw new Error("server/Cargo.lock: no samsyn-server entry found");
  return lock.replace(CARGO_LOCK_VERSION, `$1${version}$3`);
}

function cargoVersion(toml) {
  return toml.match(CARGO_PKG_VERSION)?.[2] ?? null;
}

/** Every file that carries a version must agree with manifest.json. */
export function checkConsistency(root = ROOT) {
  const v = currentVersion(root);
  const problems = [];
  const cargoPath = path.join(root, "server", "Cargo.toml");
  if (fs.existsSync(cargoPath)) {
    const cv = cargoVersion(fs.readFileSync(cargoPath, "utf8"));
    if (cv !== v) problems.push(`server/Cargo.toml has ${cv}, manifest.json has ${v}`);
    const cargoLockPath = path.join(root, "server", "Cargo.lock");
    if (fs.existsSync(cargoLockPath)) {
      const lv = fs.readFileSync(cargoLockPath, "utf8").match(CARGO_LOCK_VERSION)?.[2] ?? null;
      if (lv !== v) problems.push(`server/Cargo.lock has ${lv}, manifest.json has ${v}`);
    }
  }
  const pkg = readJson(path.join(root, "plugin", "package.json"));
  if (pkg.version !== v) problems.push(`plugin/package.json has ${pkg.version}, manifest.json has ${v}`);
  const lockPath = path.join(root, "plugin", "package-lock.json");
  if (fs.existsSync(lockPath)) {
    const lock = readJson(lockPath);
    if (lock.version !== v) problems.push(`plugin/package-lock.json has ${lock.version}, manifest.json has ${v}`);
  }
  if (parse(v).pre === null) {
    const versions = readJson(path.join(root, "versions.json"));
    if (!(v in versions)) problems.push(`versions.json lacks an entry for ${v}`);
  }
  return problems;
}

function arg(name, args) {
  const i = args.indexOf(name);
  return i === -1 ? undefined : args[i + 1];
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const [cmd, ...rest] = process.argv.slice(2);
  try {
    switch (cmd) {
      case "current":
        console.log(currentVersion());
        break;
      case "next": {
        const channel = arg("--channel", rest) ?? "beta";
        const level = arg("--level", rest) ?? "auto";
        const base = arg("--base", rest);
        const current = arg("--current", rest) ?? currentVersion();
        const currentUntagged = rest.includes("--current-untagged");
        console.log(nextVersion(current, { channel, level, base, currentUntagged }));
        break;
      }
      case "set": {
        const version = rest[0];
        if (!version) throw new Error("set needs a version");
        for (const f of applyVersion(version)) console.error(`updated ${f}`);
        console.log(format(parse(version)));
        break;
      }
      case "check": {
        const problems = checkConsistency();
        for (const p of problems) console.error(p);
        if (problems.length) process.exit(1);
        console.log(`ok ${currentVersion()}`);
        break;
      }
      default:
        console.error("usage: version.mjs current | next [--channel beta|stable] [--level auto|patch|minor|major|none] [--base X.Y.Z] | set VERSION | check");
        process.exit(2);
    }
  } catch (e) {
    console.error(e.message);
    process.exit(1);
  }
}
