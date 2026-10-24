# Development

## Prerequisites

- Node 20+ and npm
- Rust stable (1.80+) with cargo
- Obsidian desktop, and a phone with Obsidian mobile for mobile testing

## Vendored dependency: yrs

`third_party/yrs` is a copy of `yrs` 0.28.0 with one security fix (decode count caps; see `third_party/yrs/PATCH.md` and the upstream report, y-crdt/y-crdt#675), pulled in via `[patch.crates-io]` in `server/Cargo.toml` and `server/fuzz/Cargo.toml`. It is temporary: when the fix lands upstream, delete the directory and both `[patch]` stanzas and bump the `yrs` version. Do not make unrelated local edits to it; the only non-patch change is a `[lints]` table in its `Cargo.toml` that keeps its rustc warnings out of every build (path dependencies are not lint-capped the way registry crates are).

## Server crate layout

`server/` is one package with a library and a thin binary. `src/lib.rs` exposes every module (`samsyn_server::ticket`, `ids`, `limits`, `storage`, `vault`, `bridge`, `presence`, `ws`, `guest`, `sharing`, `remote`, `client`, `app`, …); `src/main.rs` only parses the command line and calls `samsyn_server::serve` or the CLI/client entry points. Anything that should be reusable in another program (ticket format, path/hash validation, the headless client, the storage layer) is therefore importable with `samsyn-server = { path = "…" }`, and the fuzz targets in `server/fuzz/` link against it. Unit tests live in the library (`cargo test --lib`).

## Server

```sh
cd server
cargo run -- serve --bind 127.0.0.1:8787 --data-dir ./data
```

Useful during development:

```sh
RUST_LOG=debug cargo run -- serve          # verbose logs
cargo run -- user add test                  # extra user + token
cargo run -- vault list                     # what is stored
cargo test                                  # unit tests
```

The data dir is throwaway; delete `./data` to start from scratch (a new owner token is printed).

## Plugin on desktop

Obsidian loads plugins from `<vault>/.obsidian/plugins/<id>/` and needs three files there: `main.js`, `manifest.json`, `styles.css`. `plugin/main.js` and `plugin/manifest.json` are build output (`manifest.json` is copied from the repo root, where Obsidian's community-plugin tooling reads it; edit the root one).

1. Create a scratch vault for development, for example `~/ObsidianDev`.
2. Point the plugin's build output at it. A symlink is the easiest:

   ```sh
   cd plugin
   npm install
   ln -s "$(pwd)" ~/ObsidianDev/.obsidian/plugins/samsyn
   ```

   On Windows use a junction: `mklink /J C:\path\to\vault\.obsidian\plugins\samsyn C:\path\to\replica\plugin`.

3. Start the watcher:

   ```sh
   npm run dev
   ```

   This rebuilds `main.js` on every save (with inline source maps).

4. In Obsidian: Settings → Community plugins → turn off Restricted mode → enable **Samsyn**.

5. After each rebuild, reload the plugin. Either toggle it off/on in settings, or install the community plugin **Hot Reload** (pjeby/hot-reload), which reloads any plugin whose folder contains a `.hotreload` file:

   ```sh
   touch plugin/.hotreload
   ```

6. Open the developer console with `Ctrl/Cmd+Shift+I`. Samsyn logs with the `[samsyn]` prefix. To inspect the shared document from the console:

   ```js
   const p = app.plugins.plugins.samsyn;
   p.engine.texts.toJSON();           // every synced text file
   p.engine.blobs.toJSON();           // every synced binary
   ```

### Two devices on one machine

Open a second vault (File → Open another vault) with the same Server URL, token and **Vault ID**. Each Obsidian window is a separate client, so you can watch live sync between two windows side by side.

### Emulating mobile on desktop

Most mobile behaviour (layout, `Platform.isMobile`) can be checked on desktop. In the console:

```js
app.emulateMobile(true);   // reload later with app.emulateMobile(false)
```

This does not emulate the mobile WebView, so always test the real thing before a release.

## Plugin on a phone

Obsidian mobile loads plugins from the same `.obsidian/plugins/samsyn/` folder inside the vault, but there is no symlink or watcher. You copy the built files in. Build first:

```sh
cd plugin
npm run build      # production main.js, minified, no source map
```

Then get `main.js`, `manifest.json` and `styles.css` into `<vault>/.obsidian/plugins/samsyn/` on the phone.

### iOS

The Files app hides folders that start with a dot, so you cannot browse into `.obsidian` from the phone itself. Pick one of these:

**A. iCloud vault, copy from a Mac (easiest).** If the mobile vault is stored in iCloud Drive, it is also on your Mac at:

```
~/Library/Mobile Documents/iCloud~md~obsidian/Documents/<VaultName>/
```

Copy the files there and iCloud pushes them to the phone:

```sh
V=~/Library/Mobile\ Documents/iCloud~md~obsidian/Documents/MyVault
mkdir -p "$V/.obsidian/plugins/samsyn"
cp plugin/main.js plugin/manifest.json plugin/styles.css "$V/.obsidian/plugins/samsyn/"
```

Wait for the iCloud upload (Finder shows a cloud icon until done), then on the phone: Settings → Community plugins → reload (pull down or toggle the plugin). The first time, enable it from the list.

**B. On-device vault, copy via Finder/USB.** Connect the phone, open Finder → iPhone → Files → Obsidian → drag the `samsyn` folder into the vault's `.obsidian/plugins/` folder. Finder does show dot-folders here. (On older macOS this is in iTunes → File Sharing.)

**C. BRAT (recommended for testers).** Install the BRAT community plugin on the phone, *Add beta plugin* → `mrboatsman/samsyn`. BRAT downloads `main.js`/`manifest.json`/`styles.css` from the latest GitHub release and keeps them updated. Enable pre-releases for Samsyn in BRAT to get the `X.Y.Z-beta.N` builds; without it BRAT only installs stable releases. See "Releasing" below for how releases are cut.

**D. Samsyn itself.** Once one device is syncing, you can tick "sync the `.obsidian` folder" in a later version and let Samsyn carry the plugin to the other devices. Not implemented yet, and risky for plugin files; listed for completeness.

### Android

Same three files into `<vault>/.obsidian/plugins/samsyn/`. Any file manager that shows hidden files works (Material Files, Solid Explorer, or the built-in Files app with "Show hidden files"), or from a computer:

```sh
adb push plugin/main.js plugin/manifest.json plugin/styles.css \
  /sdcard/Documents/MyVault/.obsidian/plugins/samsyn/
```

### Debugging on a phone

Obsidian mobile does not expose a developer console. Options, in order of usefulness:

0. **Settings → Samsyn → Server tab → Status.** Live status, whether the initial sync completed, peer count, file counts, the exact WebSocket URL, last error and a hint. Also the command **Samsyn: Show sync status**. Check this first.
1. **Watch the server.** `RUST_LOG=debug cargo run -- serve` shows every connection, auth failure and message. Most mobile issues (wrong URL, token, mixed content) are visible there.
2. **Use the status bar and Notices.** Samsyn reports connection state in the status bar and shows a Notice on errors that matter.
3. **Android: Chrome remote debugging.** With USB debugging enabled, `chrome://inspect` on a desktop Chrome lists the Obsidian WebView and gives a full console. Obsidian on Android must be a debuggable build for this; recent releases are, if not, fall back to 1 and 2.
4. **iOS: Safari Web Inspector** only works for apps built with a developer profile, which the App Store Obsidian is not. Use 1 and 2.

### Mobile gotchas

- **iOS needs `https://`.** The WebView is a secure context, so `new WebSocket("ws://…")` is refused as insecure mixed content, while `Test connection` still passes because `requestUrl` goes through native code. Symptoms: test OK, status stays "Offline" or "Connecting", hint on the settings page mentions ws://. Fix: Tailscale serve, Caddy, or `--tls-cert/--tls-key` (see README). Android is more lenient but treat it the same.
- Self-signed certificates: install the CA on the phone and enable full trust for it (iOS: Settings → General → About → Certificate Trust Settings). `mkcert -install` plus AirDropping `rootCA.pem` is the quickest dev setup.
- iOS suspends the app in the background and closes the WebSocket. Samsyn reconnects when the app returns and the reconcile step catches up on anything missed.
- `WebSocket` headers cannot be set on mobile, which is why the token also goes in the URL query string.

## Plugin settings sync

`src/config-sync.ts`. Files under `.obsidian/plugins/<id>/` have no `TFile` and no vault events, so they go through `vault.adapter` and are polled (`stat`, every 10 s) plus the undocumented `vault.on("raw")` where it fires. They are stored as blobs in the home document under their vault path (`.obsidian/plugins/<id>/data.json`), last writer wins by mtime. The selection lives in a third top-level map `config` (`id → {devices: "all" | [install ids], code: bool}`) and devices register themselves in a fourth map `devices` (`installId → {name, platform, lastSeen}`, refreshed every 5 minutes; awareness carries the install id so the picker can show who is online), so both are shared; the sync engine ignores every key under `.obsidian/` (`shouldSync` rejects dot segments), and so do the headless client and the server bridges. Pulled settings prompt a reload of that plugin via `app.plugins.disablePlugin/enablePlugin` (untyped API).

## Deep links

`registerObsidianProtocolHandler("samsyn", …)` in `main.ts` handles `obsidian://samsyn?op=mount&ticket=…` (opens the mount dialog) and `obsidian://samsyn?op=setup&server=…&token=…&vid=…[&user=…]` (asks before replacing the connection settings). `vault` cannot be used as a parameter name: Obsidian reserves it for choosing a local vault and silently drops the link when no local vault has that name, which is why the server-side vault id travels as `vid`. QR codes are those links rendered with `qrcode-generator` (`src/qr.ts`). Test on desktop by pasting the link into a browser's address bar; on iOS point the camera at the QR.

## Engines and scopes

`SyncEngine` is instantiated once per sync target (`EngineTarget`): the *home* engine (own server, whole vault, keys == vault paths) and one *guest* engine per guest mount (owner's server, one folder, keys relative to the folder). All path mapping happens at the Obsidian boundary: `keyOf(vaultPath)` returns the document key or `null` when the path is outside this engine's scope (other engines' roots, config dir, excludes); `pathOf(key)` goes back. Everything between (observers, reconcile, conflict copies, mass-delete guard) works on keys and is identical for both kinds. `SamsynPlugin.engines` holds them; `plugin.engine` is the home engine.

Local files are read from **disk**, not from Obsidian's index. `vault.getFiles()` and the `create`/`modify` events only cover extensions Obsidian detects (Markdown, canvas, PDF, images, audio, video) plus files created through its own API; a `.js`, `.css`, `.html` or `.txt` file dropped into the vault by git or Finder has no `TFile` unless the user turns on "Detect all file extensions". Reconcile therefore walks the adapter (`src/walk.ts`, unioned with the index) and the engine reads, writes and deletes through the adapter whenever there is no `TFile` for a path (`readLocalText`/`readLocalBinary`/`localStat`). Such files have no live events: they sync at startup, on "Reconcile all files now", and from any device where they arrived through samsyn (which creates them via the vault API, so they are indexed there).

## Local cache on the device

The plugin keeps two things per device and vault, both in **IndexedDB** (`src/persistence.ts`), never in the vault folder:

- the Yjs document, via `y-indexeddb`: an append-only log of updates, compacted into one record every 500 entries. A keystroke costs one small write; nothing rewrites the whole document. Database `samsyn-doc-<installId>|<vaultId>`.
- the per-path "last synced" hashes, one record per path, flushed in batches about a second after a change. Database `samsyn-state-…`.

Why not files in `.obsidian/plugins/samsyn/`: the previous `doc.bin`/`state.json` were rewritten in full on every change (a 50 MB vault meant 50 MB writes while typing), and a copied or iCloud-synced `.obsidian` carried one device's bookkeeping to another, which is how a phone once deleted a vault. Old files are migrated into IndexedDB on first start and removed. Without IndexedDB (not a case in Obsidian today) the file backend is still there as a fallback.

"Reset local state" deletes both databases. The Status section shows the backend, the encoded document size and the browser's storage usage/quota.

### Size limits, and what actually bounds a vault on a phone

APFS has no 4 GB file limit (that is FAT32); IndexedDB stores are split into many records anyway. What bounds a vault on iOS is **RAM**: Yjs keeps the whole document in memory, and iOS kills an app that goes past about 1–1.5 GB. Measured with `yjs` 13.6 in Node (heap growth per byte of text):

| How the text got into the document | Heap per byte of text |
|---|---|
| Inserted whole (first sync, files written outside the editor) | ≈ 1× |
| Typed live, word by word, in the current session | ≈ 10× |
| Typed live, character by character, in the current session | ≈ 32× |
| Any of the above after a reload (items merge on load) | ≈ 1× |
| 20 % of the text deleted (tombstones remain) | ≈ 2× |

So at startup a vault costs about its text size, plus tombstones; live typing only inflates what was typed since the last start. Loading a 50 MB document takes ~20 ms and encoding it ~40 ms (what `y-indexeddb` does every 500 updates). Rules of thumb for a phone: up to ~200 MB of Markdown is fine, 200–500 MB works with slow first sync, beyond that the single-document model needs the partial-loading design below. Binary files never enter the document (only `{hash, size, mtime}`), so attachments do not count.

What still costs at the high end: the first sync sends the whole state as one WebSocket message (a buffer the size of the vault, decoded on the phone), and the server holds every loaded vault in RAM too. The Status section warns above 100 MB of document. If a user hits the limit the fix is per-file documents loaded on demand behind a small index document (Yjs subdocs over a multiplexed stream); it is a protocol change on both sides and is not started.

## Safety behaviour worth knowing while testing

- **Sync state is device-bound.** The "last synced" hashes live in IndexedDB on the device (see "Local cache on the device"), never in the vault folder, so copying `.obsidian` to another device cannot carry one device's bookkeeping along. (The old `state.json` was the cause of a phone deleting a vault once; it is migrated and removed on first start.)
- **Mass-delete guard.** If one reconcile would remove at least 5 files and more than half of the shared vault, the plugin refuses, re-downloads them, shows a Notice, and lists them under *Held back* on the settings page. The command **Samsyn: Apply held-back deletions** does it for real. Single and small deletions are applied normally.
- **Remote deletions go to trash**, never a hard delete (Obsidian's "Deleted files" setting decides system trash vs `.trash/`).

## Headless client

`samsyn sync` lives in `server/src/client/`. It reuses the server's yrs dependency and speaks the same y-sync protocol as a client. Structure:

- `mod.rs` — flags, URL building, the main `select!` loop (WebSocket events, watcher batches, 2 s housekeeping tick, `--once` exit).
- `ws.rs` — connection task with exponential reconnect; hands the engine a per-connection sender.
- `engine.rs` — owns the `Doc`/`Awareness`, `texts` and `blobs` maps, reconcile, local → doc pushes, doc → disk writes, three-way merge for pending local edits, mass-delete guard.
- `diff.rs` — byte-offset single-span diff (yrs text indexes are UTF-8 bytes by default).
- `state.rs` — `<dir>/.samsyn/{state.json,doc.bin,trash/}`; install id derived from hostname + absolute path.

Quick manual test with two directories:

```sh
cargo run -- serve --data-dir /tmp/rd --bind 127.0.0.1:8787    # note the token
mkdir -p /tmp/a /tmp/b
cargo run -- sync --server http://127.0.0.1:8787 --token rp_… --vault t --dir /tmp/a --device A
cargo run -- sync --server http://127.0.0.1:8787 --token rp_… --vault t --dir /tmp/b --device B
echo hi > /tmp/a/x.md && sleep 1 && cat /tmp/b/x.md
```

Leftover `sync` processes keep watching their directory. Kill them before deleting a test directory, or the guard will (rightly) hold back the deletions and log about it.

## Sharing (server-side bridges)

`server/src/bridge.rs` and the `DocRegistry` in `server/src/vault.rs`. Every CRDT apply on the server goes through `DocRegistry::apply_lock`; after an apply the caller runs `propagate`, which drains the changes recorded by observers and replays them across bridges. Rules that bit during development:

- Never call `doc.get_or_insert_map` while a `transact_mut` on the same doc is alive (self-deadlock with the `sync` feature); use `txn.get_or_insert_map`.
- Do not hold a `vaults`/`shares` map guard across a call that locks it again (`if let Some(x) = self.map.lock()...` keeps the guard for the whole `if let`).
- All vaults that take part in a bridge are loaded at startup (`preload_bridged`), otherwise a share can advance while one end is unloaded and the later reconcile overwrites it.

Scenario test (one server, three users, headless clients as plugins): `server/scripts/share-e2e.sh`. Needs a debug build (`cargo build` in `server/`), uses port 8798 and a temp dir (`SAMSYN_E2E_DIR=… ` to keep it), kills every running `samsyn` first. Covers create, overlap race, mount, both-way and concurrent edits, blobs, read-only revert and rescue copy, revoke, restart, unmount. Turning it into a Rust integration test is still on the list.

Guest mode: `server/scripts/guest-e2e.sh` (owner server with `--public-url`, two headless guests via `--ticket`, rw and ro, auth hygiene, revoke). Presence across a bridge: `server/scripts/presence-e2e.sh` (one server, Node y-websocket clients from `plugin/tests/presence-e2e.mjs`, needs `npm install` in `plugin/`).

### Server to server (iroh)

`server/src/remote.rs`: endpoint, accept handler (`ShareProtocol`), link task per remote mount (`spawn_link`), framing and blob fetch. Things worth knowing:

- Both servers on one machine need different `--p2p-port`s. `--relay none` makes the test deterministic (direct connection over loopback/LAN, no internet); without it the endpoint registers with the public n0 relays, which takes a few seconds and is done in the background so HTTP is not delayed.
- A ticket embeds the owner's direct addresses *with port*, so a random p2p port (`--p2p-port 0`) breaks old tickets after a restart unless a relay is configured. Hence the fixed default.
- Blob bytes cross the link lazily: the metadata can reach a vault before the bytes do. Both clients retry a 404 with backoff; the server copies fetched bytes into the bridged vault store (`DocRegistry::blob_arrived`).
- When the owner denies a hello it waits for the reply to drain before closing, otherwise the peer only sees "connection lost" and keeps retrying instead of marking the mount revoked.

Scenario test: `server/scripts/remote-e2e.sh` (two servers, ports 8798/8799 + p2p 9798/9799, `SAMSYN_E2E_RELAY=none|default|<url>`). Covers probe of a bad ticket, mount over p2p, blobs both ways, concurrent edits, read-only, revoke → mount revoked on the other server, restart of either server, unmount. It takes 3–4 minutes.

## Changing the registry schema

Numbered migrations in `server/src/storage/registry.rs` (`migrate()`, `REGISTRY_SCHEMA_VERSION`, `PRAGMA user_version`). Add a step, bump the version, add a `migration_tests` case that opens the old shape. `CREATE TABLE IF NOT EXISTS` never changes an existing table, which is exactly how the first scaffold's reserved `mounts` table survived into a running server and broke it. See docs/STORAGE.md "Schema migrations".

## Fuzzing

`server/fuzz/` (cargo-fuzz, needs nightly: `rustup toolchain install nightly && cargo install cargo-fuzz`). Targets, one per decoder that sees untrusted bytes:

| Target | Exercises |
|---|---|
| `ticket_decode` | `Ticket::decode` (base32 + postcard, v1/v2) |
| `ysync_payload` | `ws::handle_payload`: y-sync updates, state vectors, awareness, with and without write access |
| `p2p_hello` | postcard decoding of the p2p `Hello` / `HelloReply` |
| `keys_and_hashes` | `valid_key` / `valid_hash`, asserting the invariants that keep paths inside the vault |
| `text_delta_replay` | `bridge::replay_text_delta` with arbitrary deltas against arbitrary text; must never panic inside yrs and must leave the text equal to the fallback |

```sh
cd server/fuzz
# -O = release, matching the shipped build. yrs has debug-only assertions on malformed CRDT
# input, so ysync_payload is fuzzed in release; the others keep debug assertions.
cargo fuzz run -O ysync_payload -- -max_total_time=300 -rss_limit_mb=2048 -malloc_limit_mb=2048   # still finds F-31 (open, upstream yrs)
cargo fuzz run text_delta_replay -- -max_total_time=300
```

Why `-O` for `ysync_payload`: yrs 0.28 uses `debug_assert!` in its CRDT decoder (e.g. a client id over 53 bits), which fires only in debug builds and aborts libfuzzer. The server ships in release, where those are compiled out; `handle_payload` additionally wraps the decode in `catch_unwind` and all locks are non-poisoning, so even a debug build contains such a panic as a dropped message (see the `malformed_crdt_message_is_an_error_not_a_panic` test). See [[F-31]] in the vault.

Crashes land in `fuzz/artifacts/<target>/`; turn each into a unit test next to the decoder before fixing. CI runs every target for 90 s on the manual/cron pipeline (`fuzz` step), not on every push.

## Tests

Sharing scenarios also exist as Rust integration tests in `server/tests/` (`sharing_same_server`, `sharing_remote`, `sharing_guest`, `sharing_mixed`; helpers in `tests/common/mod.rs`). They spawn the freshly built binary as servers and headless clients in a temp dir and take 1–3 minutes each:

```sh
cd server && cargo test --test sharing_same_server   # or --test sharing_remote / --test sharing_guest
```

They run as part of plain `cargo test` too. The zsh scripts under `server/scripts/` remain for poking at a scenario by hand.

```sh
cd plugin && npm test                       # diff/hash unit tests (node:test)
cd plugin && node tests/e2e.mjs ws://127.0.0.1:8787/ws rp_yourtoken   # against a running server
cd server && cargo test
```

## Releasing

Two workflows, no hand edits. Both read the version from `/manifest.json` and the changelog from Conventional Commits via [git-cliff](https://git-cliff.org) (`cliff.toml`).

**Stable: merge the release PR.** `.github/workflows/release-pr.yml` runs on every push to `main` and keeps one PR titled `chore(release): X.Y.Z` open (branch `release-pr`, label `release`). It picks the version from the commits since the last stable tag (`feat` → minor, anything else → patch, `!`/`BREAKING CHANGE` → minor while on 0.x), writes it into `manifest.json`, `plugin/package.json`, `plugin/package-lock.json`, `versions.json`, `server/Cargo.toml` and `server/Cargo.lock`, and prepends the section to `CHANGELOG.md`. The PR body is the release notes; review them, then merge. Merging lands the release commit on `main`, and `.github/workflows/release.yml` tags `X.Y.Z`, builds, and publishes the GitHub release with `main.js`, `manifest.json`, `styles.css`. The PR refreshes itself on each push to `main`, so it is always one merge away from a release. GitHub does not run CI on PRs opened with the Actions token; the PR only touches version files, so that is fine.

**Beta: press the button.** Actions → Release → Run workflow (`main`). Inputs: `level` (`auto` default, or `patch`/`minor`/`major` to choose the base) and `dry_run`. It computes `X.Y.Z-beta.N` where `X.Y.Z` is what the release PR would ship and `N` continues from the existing beta tags, stamps that version into the built `manifest.json`, tags, and publishes a GitHub **pre-release**. Nothing is committed to `main`; the repo stays on the last stable version. BRAT users only see betas if they enabled pre-releases for Samsyn.

Version arithmetic lives in `scripts/version.mjs` (tests in `scripts/version.test.mjs`): `0.1.0` + `feat` → PR for `0.2.0`, betas `0.2.0-beta.1`, `-beta.2`, …; merging the PR ships `0.2.0` and the next beta line starts fresh. CI fails if the version files drift (`node scripts/version.mjs check`).

The server reports its version from `/health` and `samsyn --version` (`samsyn_server::VERSION`). A stable release stamps `server/Cargo.toml`, so a local `cargo build` of that commit reports it. Betas commit nothing, so `.github/workflows/docker.yml` passes the tag as the `SAMSYN_VERSION` build argument, which the server prefers over `CARGO_PKG_VERSION`; images built from `main` report `X.Y.Z+<sha>`. The plugin shows both versions in Settings → Samsyn → Status.

Typical flow:

1. Merge work to `main`. The release PR updates itself.
2. Need testers on it? Run Release (beta). Repeat as fixes land; `N` increments.
3. Happy? Merge the release PR. Stable `X.Y.Z` goes out; everyone on BRAT gets it.

First release: the first push after this lands opens `chore(release): 0.1.0` (no stable tag exists yet, so the current version is released as is). Merge it.

Pushes and tags made by the workflows use `GITHUB_TOKEN`, which does not trigger other workflows, so `release.yml` starts the Docker image build for the tag itself (`gh workflow run docker.yml`). `chore(release): …` is reserved for the release PR; do not write it by hand, since the release workflow keys on it.

Still manual, later:

- Submit to the Obsidian community plugin list following the official guidelines (`isDesktopOnly: false`, no `innerHTML`, root `manifest.json`/`versions.json`, a stable release). After that, Obsidian itself offers updates and BRAT is only needed for betas.
- Server binaries (`cargo build --release`) for Linux x86_64/arm64, macOS, Windows. The Docker image is already built per tag.
