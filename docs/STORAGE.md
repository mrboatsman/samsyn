# Server storage model

Goals: many users, each with many vaults; every vault self-contained on disk; trivial to back up, snapshot, encrypt and restore after a disaster; room to add shared folders between users later.

## Layout

```
<data-dir>/
  server.json                       instance id, schema version
  registry.db                       SQLite: users, token hashes, vault index, shares, grants, mounts
  identity.key                      ed25519 secret (hex, 0600); public half = node id in tickets
  users/
    <user-id>/
      shares/<share-id>/            share documents this user owns (same shape as a vault)
      mounts/<mount-id>/            local replicas of shares mounted from other servers
      vaults/
        <vault-id>/
          vault.json                owner, name, created (plain text, for recovery by hand)
          doc.db                    SQLite: append-only log of Yjs updates
          blobs/
            ab/abcdef…              immutable, content-addressed (SHA-256) binary files
          snapshots/
            2026-10-14T18-16-11Z.ybin   full Yjs state at that moment
            2026-10-14T18-16-11Z.json   manifest: counts, referenced blob hashes
```

IDs are random 16-hex strings so renaming a user or vault never moves data. `vault.json` carries the human-readable names so a directory is still meaningful without `registry.db`.

## Identity and access

- A **user** owns vaults. A user has one or more **tokens** (one per device is the intention). Only the SHA-256 of a token is stored; the plaintext is printed once by `user add` / `user token`.
- Clients present the token as `Authorization: Bearer` or `?token=` (mobile WebSocket cannot set headers).
- The vault name in the URL (`/ws/<name>`, `/blobs/<name>/<hash>`) is resolved **inside the authenticated user's namespace**. Two users with a vault named `notes` get two unrelated vaults. A vault is created on first use, so a new device needs nothing more than the token and the name.
- On a fresh data dir the server creates user `owner` and prints its token, so a single-person install needs no admin step.

## The document

One Yjs document per vault, two top-level maps:

| Map     | Key  | Value                          | Merge semantics                 |
|---------|------|--------------------------------|---------------------------------|
| `texts` | path | `Y.Text`                       | character-level CRDT merge      |
| `blobs` | path | `{hash, size, mtime}`          | last writer wins per key        |

Delete = remove the key. Rename = new key with a copy + remove old key; a folder rename is that for every file in it, and a client applying the deletes removes the folders they leave empty (stopping at the sync root and at shared roots), so the old name does not linger on other devices. The server never interprets the content; it only stores and relays updates, so the schema can evolve on the plugin side.

## Schema migrations

`registry.db` carries its schema version in SQLite's `PRAGMA user_version` (`REGISTRY_SCHEMA_VERSION` in `server/src/storage/registry.rs`, currently 5: v3 added `share_grants.last_seen_at`, v4 added `rw_paths` on `share_grants` and `mounts`, v5 added `mounts.forward_shares`). On every open the server first creates any *missing* tables in their current shape, then runs the numbered steps in `migrate()` from the stored version up to the current one and records the new version. A database written by a newer server is refused with a clear error instead of being touched.

Rules for changing the schema:

1. Never edit an existing `CREATE TABLE` and hope `IF NOT EXISTS` applies it: it does not. Add a numbered step.
2. Steps are additive and idempotent (`ADD COLUMN` guarded by a column check, new tables, new indexes). A table that must change shape and may hold data is renamed to `<name>_legacy_vN`, never dropped.
3. Bump `REGISTRY_SCHEMA_VERSION` with the step, add a test in `migration_tests` that opens a database of the old shape, and note the change here.
4. Document data (`doc.db`, blobs, snapshots) has no schema beyond the Yjs update log; it is versioned by `server.json`'s `schema_version` and would get the same treatment.

History: v1 replaced the empty, reserved `mounts` table from the first scaffold with the sharing model (renamed to `mounts_legacy_v0` if it ever had rows); v2 added `home_addrs` and `only_file` for server-to-server mounts.

## Durability

- Every update a client sends is appended to `doc.db` **before** it is broadcast (the persistence callback runs inside the commit). SQLite runs in WAL mode with `synchronous=NORMAL`: durable across process crashes, and across power loss except for the last few milliseconds.
- When a log exceeds 500 rows it is **compacted**: replaced by a single full-state update. This happens on load, after client disconnects, and on every snapshot. Compaction loses nothing (Yjs state is a complete merge of history).
- Blobs are written to a temp file and renamed into place, so a reader never sees a half-written file. They are immutable and content-addressed, so re-uploading is a no-op and the same bytes under two paths are stored once per vault.

## Snapshots

A snapshot is the entire document as one `.ybin` file plus a JSON manifest listing which blob hashes it references. They are:

- taken automatically for every loaded vault every `--snapshot-hours` (default 24) and on graceful shutdown, keeping the newest `--snapshot-keep` (default 14);
- taken on demand with `samsyn snapshot [user/vault]`;
- plain files, so any file-level backup tool (restic, borg, rsync, Time Machine) picks them up without caring about SQLite.

Roll a vault back with:

```sh
samsyn restore-snapshot owner/notes path/to/2026-10-14T18-16-11Z.ybin --yes
```

A safety snapshot of the current state is written first. Because Yjs merges, clients that still hold newer state will push it back after a rollback. That includes *deletions*: a device whose cached document contains "file X was deleted" will delete it again on reconnect. So a rollback is a three-step dance:

1. Stop the server, `restore-snapshot … --yes`, start the server.
2. On **every** device: Settings → Samsyn → **Reset local sync state** (drops the cached document; files on disk are untouched).
3. Each device reconnects, downloads the restored state, and reconciles its disk against it. Files that exist on disk but not in the snapshot are pushed up again as new files.

## Backups

`samsyn backup` writes a `tar.gz` that mirrors the data dir layout exactly. SQLite files go through the online backup API, so it is **safe to run while the server is serving**. Scope it with `--user` and `--vault`.

Encrypt by streaming to stdout:

```sh
samsyn backup --out - | age -r age1yourkey > samsyn-$(date +%F).tar.gz.age
# or
samsyn backup --out - | gpg -e -r you@example.com > samsyn.tar.gz.gpg
```

Restore into an empty dir, or merge selected vaults into a live one:

```sh
samsyn restore --from samsyn.tar.gz --data-dir /srv/samsyn/data
samsyn restore --from one-vault.tar.gz --merge      # adds/overwrites vaults in the archive
```

Because the archive is plain `tar`, a human can also `tar xzf` it and copy a single vault directory by hand in an emergency; `vault.json` says whose it is.

### Encryption at rest

The server does not encrypt data itself. Recommended: run on an encrypted filesystem or volume (LUKS, FileVault, cloud provider disk encryption) and encrypt backups as above. All disk writes go through `VaultStore`, so a per-vault at-rest cipher can be added later behind one interface without changing the layout.

## Disaster playbook

| Situation | Action |
|-----------|--------|
| Server disk lost | `restore --from <latest backup>`; devices reconnect and re-merge anything newer they still hold |
| One vault corrupted | `restore-snapshot user/vault <snapshot> --yes`, or `restore --merge` from a backup containing just that vault |
| `registry.db` lost, vault dirs intact | re-create users (`user add`), then `restore --merge` from any backup, or copy vault dirs and re-register them from their `vault.json` (tooling for this is a TODO) |
| Files vanished from the shared vault (a device pushed mass deletions) | Restore them from that device's Obsidian/system trash and let reconcile push them back, or roll back with `restore-snapshot` + reset local state on every device (see Snapshots). The plugin's mass-delete guard now refuses such pushes without explicit confirmation. |
| Token leaked | `user revoke <name> --label <label>` (or all), then `user token <name>` |
| Need to move servers | stop, `backup`, `restore` on the new host, point the plugin at the new URL |

## Shares and mounts

Shares are separate documents stored like vaults under `users/<uid>/shares/<share_id>/`; a share mounted from another server is replicated into `users/<uid>/mounts/<mount_id>/`. Registry tables `shares`, `share_grants` (hashed ticket secrets) and `mounts` (including the owner's node id, addresses and the ticket secret, which is why `registry.db` should be treated as sensitive) describe who shares what and who mounted it. The server keeps shared subtrees equal between documents at runtime; see `docs/SHARING.md`.

Full backups (`backup` without `--user`/`--vault`) include share and mount directories; a full `restore` puts them back. Filtered backups and `restore --merge` only carry vaults. That is survivable: an owned share is refilled from the owner's vault on the next start, and a remote replica is refilled from the owner's server. Losing a replica means the mount path is briefly emptied and then re-downloaded, so prefer full backups on servers that mount remote shares.
