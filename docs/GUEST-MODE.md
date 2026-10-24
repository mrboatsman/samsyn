# Guest mode: participate in a share with only the plugin

Status: phases A (server, headless guest), B (plugin) and C (sessions, per-share quota, ticket activity, public-server guide in PUBLIC-SERVER.md, deep links + QR codes) implemented. Deviations from the design below: the plugin decodes tickets locally (`src/ticket.ts`) so guest mounts need no server at all; the key mapping lives at the Obsidian boundary (`SyncEngine.keyOf`/`pathOf`) rather than as a separate `EngineScope` object; persistence is namespaced through the IndexedDB scope (`guest:<share_id>`), not file names. See SHARING.md "Guests".

Goal: Bob runs a Samsyn server that is reachable on the public internet. He shares `Projects/X` and sends Alice the ticket. Alice installs the Samsyn plugin, pastes the ticket, picks a local folder — and is in the live sync. She has no Samsyn server, no account on Bob's server, and nothing outside that folder ever syncs anywhere. Many-to-many: a share can have any number of guests, and one vault can carry any number of guest mounts from different servers, alongside (or instead of) its own home-server sync.

## The idea

The plugin already speaks y-sync to a Y.Doc with two maps (`texts`, `blobs`). A share document (SHARING.md) is a document of exactly that shape, with keys relative to the share prefix. So guest mode is not a new sync mechanism:

> A guest mount is one more y-sync WebSocket connection, opened directly to the **owner's** server, on the **share document**, authenticated by the **ticket secret** instead of a user token. The plugin maps relative share keys ↔ local paths under the mount folder. No server on the guest side; the plugin plays the role the server-side bridge plays for server mounts.

```
Bob's vault doc ──bridge── share doc ◄──ws──► Alice's plugin ──► Shared/X/… (her disk)
                              ▲ ◄──ws──► Carol's plugin
                              └─(phase 2: iroh)── Dave's own server …
```

Everything in the existing engine (observers, three-way reconcile, conflict copies, mass-delete guard) is reused per connection; what changes is that an engine instance can be *scoped* to a folder and a key mapping.

Relation to server mounts: same ticket, two ways to consume it. With your own server you mount server-side (today same-server, phase 2 over iroh). Without one, the plugin mounts it directly — which requires the owner's server to be reachable over HTTPS from the guest's devices. That reachability requirement is the trade; iroh mounts don't have it.

## Ticket v2

A guest client is a plain WebSocket — it cannot dial an iroh node id. The ticket gains the server's public URL:

- `TicketV2 = TicketV1 + url: String` (e.g. `https://sync.bob.example`). Empty = owner's server has no public URL; ticket works for server mounts only.
- Postcard is not self-describing, so this is a version bump: decode dispatches on the leading version byte and accepts v1 and v2. New tickets are always v2.
- The server learns its URL from `--public-url` / `SAMSYN_PUBLIC_URL`. When unset, share creation still works; the UI notes that guests cannot use the ticket.

## Server

New guest surface, all authenticated by `(share_id, secret)` via the existing `check_grant` (secrets are stored hashed; lookups go through `token_hash`):

| Method | Path | Notes |
|---|---|---|
| GET | `/guest/shares/{share_id}?secret=` | name, access, `rw_paths`, path kind; lets the plugin validate a ticket and detect revocation |
| GET | `/guest/ws/{share_id}?secret=` | y-sync WebSocket on the share's LiveDoc |
| GET/PUT | `/guest/blobs/{share_id}/{hash}?secret=` | share-scoped blob store; PUT only for grants that can write somewhere (`rw`, or `ro` with `rw_paths`) and subject to `max_blob` |

Implementation notes:

- **Share LiveDocs.** `DocRegistry` already loads share docs for bridges; add `get_or_load_share(share_id)` and reuse `ws::handle_socket` unchanged — it only needs a `LiveDoc`. Guest updates flow through the same server-wide apply lock and propagate across the owner bridge like any other edit.
- **Read-only enforcement is server-side.** On an `ro` connection, incoming sync updates and awareness are parsed but **not applied**; the server replies with sync-step-1 so the client converges back to the server state. On an `ro` connection with `rw_paths`, updates are applied and the changes on keys outside those paths are undone right away from the owner's vault (see SHARING.md, *Read-only mount with writable paths*). The plugin additionally behaves like the bridge does for ro mounts (reverts local edits, never pushes from the read-only part), but the server never trusts that.
- **Revocation disconnects.** Keep a map `(share_id, grant_label) → CancellationToken` for live guest sockets; `revoke_ticket` and `delete_share` cancel them. A reconnect re-checks the grant and gets 403.
- **No mount rows for guests.** A guest mount lives in the guest's plugin settings, not in the owner's registry. For owner visibility, add `last_seen_at` (and optionally a connected-count) to `share_grants`, updated on guest connect, surfaced in the sharing UI ("ticket-2 · last seen 5 min ago").
- **Public-endpoint hygiene.** The secret is a bearer credential on a public server: respond identically to unknown share and bad secret; never log the query string on `/guest/*`; rate-limit failed guest auth per IP (simple in-memory bucket is enough). Browsers cannot set WS headers, so the secret rides in the query string for v1; a later hardening step can exchange the ticket for a short-lived session token via `POST /guest/session` and keep secrets out of URLs entirely.
- **Quotas.** An `rw` guest can upload blobs. v1 ships with `max_blob` only; per-share size quota is listed under Later.

## Headless guest

`samsyn sync --ticket <ticket> --dir /path` — the headless client in guest mode, same mapping as the plugin. Comes almost free once the guest WS exists (the client already speaks y-sync + blobs), and is the natural integration-test harness: two headless guests + one owner exercise the whole feature without Obsidian.

## Plugin

### Engine scoping

`SyncEngine` today assumes doc keys == vault paths and one connection per plugin. Refactor (no behavior change for home sync):

```ts
interface EngineScope {
  root: string;              // "" for home, "Shared/X/" for a guest mount
  extraExcludes: string[];   // home engine: all guest mount roots
  keyOf(path: string): string | null;  // vault path -> doc key (null = out of scope)
  pathOf(key: string): string;         // doc key -> vault path
}
```

Home engine: identity mapping, `extraExcludes` = every guest mount root (so guest folders never reach the home server — and the home vault's files never reach Bob). Guest engine: handles only paths under `root`; doc keys are share-relative, exactly what the share document contains. Reconcile, observers, conflict copies, rename handling and the mass-delete guard all operate post-mapping and need no semantic changes; the guard's ratio is naturally per-scope.

### Lifecycle and settings

- `settings.guestMounts: { id, url, shareId, secret, access, rwPaths, name, mountPath, enabled }[]` (parsed from the ticket at paste time; the raw ticket is not kept; `rwPaths` is refreshed from the owner's `/guest/session` answer on every connect).
- The plugin runs N+1 engines: optional home engine (server URL may now be empty — Alice's case) plus one per enabled guest mount. Status bar and `Show sync status` aggregate; per-mount state in settings.
- Persistence is namespaced per engine (`doc-home.bin`, `doc-guest-<share_id>.bin`, same for state) instead of the current single pair.
- Each guest engine gets a `WebsocketProvider` against `wss://<url>/guest/ws` with room `share_id` and `params: {secret}`, and a `BlobClient` against the guest blob routes.
- `ro` mounts: local edits under the mount are reverted from the doc (same policy as the server bridge), pushes suppressed, folder marked read-only in the UI. With `rwPaths`, keys under those paths are pushed like on an `rw` mount.
- Mobile: the owner URL must be `https://` (same WebKit constraint as today); the UI says so when a ticket's URL is plain http.

### UI

Sharing tab, *Shared with me* grows a second path: pasting a ticket whose URL is set offers **Mount as guest** (local folder, default `Shared/<name>`) next to the existing server mount. No home server configured → guest mount is the only option and the rest of the settings stay dormant. Overlap rules client-side: guest mount roots may not nest each other or existing files (same "must be empty" rule as server mounts). A guest mount inside one of your own shares is fine and is never carried onward by it, since the home engine excludes the folder; the Sharing tab says so on the card.

## Many-to-many

- Many guests on one share: every guest socket joins the same share LiveDoc; CRDT merges; awareness is shared, so guests see each other (and bridged owner devices, phase-C) as peers.
- Many mounts in one vault: independent engines, independent servers, independent failure domains. One unreachable owner server degrades one folder, nothing else.
- Guests and server mounts coexist on the same share; revoking one grant kills exactly the sessions and mounts made with it.
- Migration guest → own server later: unmount guest (files stay), then server-mount the same ticket. The server mount requires an empty prefix, so the local copy must be moved aside first; a smoother handover is listed under Later.

## Phases

- **A — server.** Ticket v2 + `--public-url`; `get_or_load_share`; guest WS/info/blob routes with ro-drop, revocation kill, rate limit, log hygiene; `last_seen_at` on grants; headless `sync --ticket`. Integration test: owner + two headless guests, rw and ro.
- **B — plugin.** `EngineScope` refactor (home sync identical before/after); multi-engine lifecycle + namespaced persistence; guest mounts in settings + Sharing UI; status aggregation; ro behavior.
- **C — polish.** Owner UI for ticket activity; presence names across the bridge; `POST /guest/session` token exchange; per-share quotas; docs (public exposure guide: Caddy, rate limits, what a leaked ticket can and cannot do).

## Later / out of scope

- Per-share storage quotas and upload accounting.
- Guest → server-mount handover without moving files aside.
- E2E encryption of share docs (owner's server currently sees plaintext by design).
