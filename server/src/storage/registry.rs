//! Users, tokens and the vault index. One small SQLite file.

use std::path::Path;
use std::sync::Arc;
use parking_lot::Mutex;

use anyhow::Context;
use rusqlite::{params, Connection, OptionalExtension};

use crate::ids::{new_id, new_token, token_hash, valid_name};

#[derive(Debug, Clone)]
pub struct User {
    pub id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone)]
pub struct VaultInfo {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ShareInfo {
    pub id: String,
    pub user_id: String,
    pub vault_id: String,
    pub name: String,
    /// Folder prefix with trailing slash, or "" for the vault root.
    pub prefix: String,
    /// Set when the share is a single note inside `prefix`.
    pub only_file: Option<String>,
    pub created_at: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct GrantInfo {
    pub label: String,
    pub access: String,
    /// Share-relative folders (`Inbox/`) or notes (`todo.md`) a read-only ticket may still write.
    pub rw_paths: Vec<String>,
    pub created_at: String,
    pub revoked_at: Option<String>,
    /// Last time a guest client authenticated with this ticket.
    pub last_seen_at: Option<String>,
}

/// What a valid, unrevoked ticket secret grants: who it is for and what it may write.
#[derive(Debug, Clone)]
pub struct GrantAccess {
    pub label: String,
    pub access: String,
    pub rw_paths: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct MountInfo {
    pub id: String,
    pub user_id: String,
    pub vault_id: String,
    pub share_id: String,
    pub share_name: String,
    pub mount_prefix: String,
    pub access: String,
    /// Share-relative paths the owner lets this (read-only) mount write; see `GrantInfo::rw_paths`.
    pub rw_paths: Vec<String>,
    pub home_node: String,
    /// Owner server's relay/direct addresses as printed in the ticket (remote mounts).
    pub home_addrs: Vec<String>,
    /// Set when the share is a single note (remote mounts; local ones read it from the share).
    pub only_file: Option<String>,
    /// Ids of this vault's own shares that enclose the mount folder and are allowed to carry
    /// the mounted content onward to their holders. An enclosing share not listed here skips
    /// the mount's subtree. Empty for mounts outside every share.
    pub forward_shares: Vec<String>,
    #[serde(skip_serializing)]
    pub secret: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
}

#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub label: String,
    pub created_at: String,
    pub last_used_at: Option<String>,
    /// The token that made the request listing them.
    pub current: bool,
}

#[derive(Clone)]
pub struct Registry {
    conn: Arc<Mutex<Connection>>,
    /// Key for mount secrets at rest (derived from identity.key); None until the storage layer
    /// provides it, in which case secrets are handled as stored.
    secret_key: Arc<Mutex<Option<[u8; 32]>>>,
}

const ENC_PREFIX: &str = "enc1:";

fn seal(key: &[u8; 32], plain: &str) -> String {
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};
    let cipher = XChaCha20Poly1305::new(key.into());
    let mut nonce_bytes = [0u8; 24];
    rand::Rng::fill_bytes(&mut rand::rng(), &mut nonce_bytes);
    let nonce = XNonce::from_slice(&nonce_bytes);
    let ct = cipher.encrypt(nonce, plain.as_bytes()).expect("xchacha20poly1305 encrypt cannot fail");
    use base64::Engine as _;
    format!("{ENC_PREFIX}{}{}", base64::engine::general_purpose::STANDARD.encode(nonce_bytes), base64::engine::general_purpose::STANDARD.encode(ct))
}

fn open_sealed(key: &[u8; 32], stored: &str) -> Option<String> {
    use base64::Engine as _;
    use chacha20poly1305::aead::{Aead, KeyInit};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};
    let body = stored.strip_prefix(ENC_PREFIX)?;
    // nonce is 24 bytes → 32 base64 chars.
    if body.len() < 32 {
        return None;
    }
    let (n, c) = body.split_at(32);
    let nonce_bytes = base64::engine::general_purpose::STANDARD.decode(n).ok()?;
    let ct = base64::engine::general_purpose::STANDARD.decode(c).ok()?;
    let cipher = XChaCha20Poly1305::new(key.into());
    let pt = cipher.decrypt(XNonce::from_slice(&nonce_bytes), ct.as_ref()).ok()?;
    String::from_utf8(pt).ok()
}

impl Registry {
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS users (
                id         TEXT PRIMARY KEY,
                name       TEXT NOT NULL UNIQUE,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS tokens (
                hash         TEXT PRIMARY KEY,
                user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                label        TEXT NOT NULL,
                created_at   TEXT NOT NULL,
                last_used_at TEXT
            );
            CREATE INDEX IF NOT EXISTS tokens_user ON tokens(user_id);
            CREATE TABLE IF NOT EXISTS vaults (
                id         TEXT PRIMARY KEY,
                user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                name       TEXT NOT NULL,
                created_at TEXT NOT NULL,
                UNIQUE (user_id, name)
            );
            CREATE TABLE IF NOT EXISTS shares (
                id         TEXT PRIMARY KEY,
                user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                vault_id   TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                name       TEXT NOT NULL,
                prefix     TEXT NOT NULL,
                only_file  TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS share_grants (
                share_id    TEXT NOT NULL REFERENCES shares(id) ON DELETE CASCADE,
                secret_hash TEXT NOT NULL,
                label       TEXT NOT NULL,
                access      TEXT NOT NULL,
                rw_paths    TEXT NOT NULL DEFAULT '',
                created_at  TEXT NOT NULL,
                revoked_at  TEXT,
                PRIMARY KEY (share_id, label)
            );
            CREATE TABLE IF NOT EXISTS mounts (
                id           TEXT PRIMARY KEY,
                user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                vault_id     TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                share_id     TEXT NOT NULL,
                share_name   TEXT NOT NULL,
                mount_prefix TEXT NOT NULL,
                access       TEXT NOT NULL,
                rw_paths     TEXT NOT NULL DEFAULT '',
                home_node    TEXT NOT NULL,
                secret       TEXT NOT NULL,
                created_at   TEXT NOT NULL,
                revoked_at   TEXT,
                forward_shares TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS mounts_vault ON mounts(vault_id);
            CREATE INDEX IF NOT EXISTS mounts_share ON mounts(share_id);
            "#,
        )?;
        migrate(&conn)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)), secret_key: Arc::new(Mutex::new(None)) })
    }

    /// Provide the at-rest key. Any mount secret still stored in plaintext is sealed now.
    pub fn set_secret_key(&self, key: [u8; 32]) -> anyhow::Result<()> {
        *self.secret_key.lock() = Some(key);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, secret FROM mounts")?;
        let rows: Vec<(String, String)> = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .collect();
        drop(stmt);
        let mut sealed = 0;
        for (id, secret) in rows {
            if !secret.starts_with(ENC_PREFIX) {
                conn.execute("UPDATE mounts SET secret = ?1 WHERE id = ?2", params![seal(&key, &secret), id])?;
                sealed += 1;
            }
        }
        if sealed > 0 {
            tracing::info!("registry: sealed {sealed} mount secret(s) at rest");
        }
        Ok(())
    }

    fn protect_secret(&self, secret: &str) -> String {
        match *self.secret_key.lock() {
            Some(k) => seal(&k, secret),
            None => secret.to_string(),
        }
    }

    fn reveal_secret(&self, stored: &str) -> String {
        if !stored.starts_with(ENC_PREFIX) {
            return stored.to_string();
        }
        match *self.secret_key.lock() {
            Some(k) => open_sealed(&k, stored).unwrap_or_else(|| {
                tracing::error!("a mount secret could not be decrypted (identity.key changed?)");
                String::new()
            }),
            None => String::new(),
        }
    }

    fn now() -> String {
        chrono::Utc::now().to_rfc3339()
    }

    // ------------------------------------------------------------------ users

    pub fn create_user(&self, name: &str) -> anyhow::Result<User> {
        anyhow::ensure!(valid_name(name), "invalid user name (use letters, digits, - _ .)");
        let user = User { id: new_id(), name: name.to_string(), created_at: Self::now() };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO users (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![user.id, user.name, user.created_at],
        )
        .with_context(|| format!("creating user {name} (already exists?)"))?;
        Ok(user)
    }

    pub fn user_by_name(&self, name: &str) -> anyhow::Result<Option<User>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT id, name, created_at FROM users WHERE name = ?1",
                params![name],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn user_by_id(&self, id: &str) -> anyhow::Result<Option<User>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT id, name, created_at FROM users WHERE id = ?1",
                params![id],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn list_users(&self) -> anyhow::Result<Vec<User>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, name, created_at FROM users ORDER BY name")?;
        let rows = stmt.query_map([], |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn user_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
    }

    pub fn delete_user(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM users WHERE id = ?1", params![id])? > 0)
    }

    // ----------------------------------------------------------------- tokens

    /// Issue a new token for a user. The plaintext is returned once and never stored.
    pub fn create_token(&self, user_id: &str, label: &str) -> anyhow::Result<String> {
        let token = new_token();
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO tokens (hash, user_id, label, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![token_hash(&token), user_id, label, Self::now()],
        )?;
        Ok(token)
    }

    /// Resolve a presented token to its user, updating `last_used_at`.
    pub fn authenticate(&self, token: &str) -> anyhow::Result<Option<User>> {
        let hash = token_hash(token);
        let conn = self.conn.lock();
        let user = conn
            .query_row(
                "SELECT u.id, u.name, u.created_at FROM tokens t JOIN users u ON u.id = t.user_id WHERE t.hash = ?1",
                params![hash],
                |r| Ok(User { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? }),
            )
            .optional()?;
        if user.is_some() {
            let _ = conn.execute(
                "UPDATE tokens SET last_used_at = ?1 WHERE hash = ?2",
                params![Self::now(), hash],
            );
        }
        Ok(user)
    }

    /// A user's tokens, oldest first. `presented` is the plaintext token of the caller, if any,
    /// so its own row can be marked `current`.
    pub fn list_tokens(&self, user_id: &str, presented: Option<&str>) -> anyhow::Result<Vec<TokenInfo>> {
        let own = presented.map(token_hash);
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT label, created_at, last_used_at, hash FROM tokens WHERE user_id = ?1 ORDER BY created_at")?;
        let rows = stmt.query_map(params![user_id], |r| {
            let hash: String = r.get(3)?;
            Ok(TokenInfo {
                label: r.get(0)?,
                created_at: r.get(1)?,
                last_used_at: r.get(2)?,
                current: own.as_deref() == Some(hash.as_str()),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Does this user already have a token with this label? Labels are what tokens are revoked
    /// by, so they are kept unique per user.
    pub fn label_taken(&self, user_id: &str, label: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT 1 FROM tokens WHERE user_id = ?1 AND label = ?2", params![user_id, label], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Is `presented` one of this user's tokens carrying this label? Used to stop a device
    /// revoking the token it is talking with.
    pub fn is_token(&self, user_id: &str, label: &str, presented: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT 1 FROM tokens WHERE user_id = ?1 AND label = ?2 AND hash = ?3",
                params![user_id, label, token_hash(presented)],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    pub fn revoke_tokens(&self, user_id: &str, label: Option<&str>) -> anyhow::Result<usize> {
        let conn = self.conn.lock();
        Ok(match label {
            Some(l) => conn.execute("DELETE FROM tokens WHERE user_id = ?1 AND label = ?2", params![user_id, l])?,
            None => conn.execute("DELETE FROM tokens WHERE user_id = ?1", params![user_id])?,
        })
    }

    // ----------------------------------------------------------------- vaults

    pub fn get_or_create_vault(&self, user_id: &str, name: &str) -> anyhow::Result<VaultInfo> {
        anyhow::ensure!(valid_name(name), "invalid vault name");
        let conn = self.conn.lock();
        if let Some(v) = conn
            .query_row(
                "SELECT id, user_id, name, created_at FROM vaults WHERE user_id = ?1 AND name = ?2",
                params![user_id, name],
                row_to_vault,
            )
            .optional()?
        {
            return Ok(v);
        }
        let v = VaultInfo { id: new_id(), user_id: user_id.to_string(), name: name.to_string(), created_at: Self::now() };
        conn.execute(
            "INSERT INTO vaults (id, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![v.id, v.user_id, v.name, v.created_at],
        )?;
        Ok(v)
    }

    /// Look a vault up without creating it.
    pub fn vault_by_name(&self, user_id: &str, name: &str) -> anyhow::Result<Option<VaultInfo>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT id, user_id, name, created_at FROM vaults WHERE user_id = ?1 AND name = ?2", params![user_id, name], row_to_vault)
            .optional()?)
    }

    pub fn vault_count(&self, user_id: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.query_row("SELECT count(*) FROM vaults WHERE user_id = ?1", params![user_id], |r| r.get::<_, i64>(0))? as usize)
    }

    pub fn vault_by_id(&self, id: &str) -> anyhow::Result<Option<VaultInfo>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT id, user_id, name, created_at FROM vaults WHERE id = ?1", params![id], row_to_vault)
            .optional()?)
    }

    pub fn list_vaults(&self, user_id: Option<&str>) -> anyhow::Result<Vec<VaultInfo>> {
        let conn = self.conn.lock();
        let mut out = Vec::new();
        match user_id {
            Some(u) => {
                let mut stmt =
                    conn.prepare("SELECT id, user_id, name, created_at FROM vaults WHERE user_id = ?1 ORDER BY name")?;
                for r in stmt.query_map(params![u], row_to_vault)? {
                    out.push(r?);
                }
            }
            None => {
                let mut stmt = conn.prepare("SELECT id, user_id, name, created_at FROM vaults ORDER BY user_id, name")?;
                for r in stmt.query_map([], row_to_vault)? {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    pub fn delete_vault(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM vaults WHERE id = ?1", params![id])? > 0)
    }

    /// Re-insert a vault row (used by restore when the registry lacks a vault found on disk).
    pub fn upsert_vault(&self, v: &VaultInfo) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO vaults (id, user_id, name, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![v.id, v.user_id, v.name, v.created_at],
        )?;
        Ok(())
    }

    pub fn upsert_user(&self, u: &User) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO users (id, name, created_at) VALUES (?1, ?2, ?3)",
            params![u.id, u.name, u.created_at],
        )?;
        Ok(())
    }

    /// Consistent copy of the registry for backups (SQLite online backup API).
    pub fn backup_to(&self, dest: &Path) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        let mut dst = Connection::open(dest)?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
        backup.run_to_completion(256, std::time::Duration::from_millis(5), None)?;
        Ok(())
    }
}

impl Registry {
    // ----------------------------------------------------------------- shares

    pub fn create_share(&self, user_id: &str, vault_id: &str, name: &str, prefix: &str, only_file: Option<&str>) -> anyhow::Result<ShareInfo> {
        let s = ShareInfo {
            id: new_id(),
            user_id: user_id.into(),
            vault_id: vault_id.into(),
            name: name.into(),
            prefix: prefix.into(),
            only_file: only_file.map(|s| s.to_string()),
            created_at: Self::now(),
        };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO shares (id, user_id, vault_id, name, prefix, only_file, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![s.id, s.user_id, s.vault_id, s.name, s.prefix, s.only_file, s.created_at],
        )?;
        Ok(s)
    }

    pub fn share_by_id(&self, id: &str) -> anyhow::Result<Option<ShareInfo>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT id, user_id, vault_id, name, prefix, only_file, created_at FROM shares WHERE id = ?1", params![id], row_to_share)
            .optional()?)
    }

    pub fn shares_for_vault(&self, vault_id: &str) -> anyhow::Result<Vec<ShareInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, user_id, vault_id, name, prefix, only_file, created_at FROM shares WHERE vault_id = ?1 ORDER BY prefix")?;
        let rows = stmt.query_map(params![vault_id], row_to_share)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Move a share to another folder or note in the same vault (the share itself is unchanged).
    pub fn update_share_path(&self, id: &str, prefix: &str, only_file: Option<&str>) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE shares SET prefix = ?1, only_file = ?2 WHERE id = ?3", params![prefix, only_file, id])? > 0)
    }

    pub fn delete_share(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM shares WHERE id = ?1", params![id])? > 0)
    }

    pub fn create_grant(&self, share_id: &str, label: &str, access: &str, rw_paths: &[String]) -> anyhow::Result<String> {
        let secret = hex::encode({
            let mut b = [0u8; 32];
            rand::Rng::fill_bytes(&mut rand::rng(), &mut b);
            b
        });
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO share_grants (share_id, secret_hash, label, access, rw_paths, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![share_id, token_hash(&secret), label, access, join_paths(rw_paths), Self::now()],
        )
        .with_context(|| format!("creating ticket '{label}' (label already used?)"))?;
        Ok(secret)
    }

    pub fn grants_for_share(&self, share_id: &str) -> anyhow::Result<Vec<GrantInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT label, access, created_at, revoked_at, last_seen_at, rw_paths FROM share_grants WHERE share_id = ?1 ORDER BY created_at")?;
        let rows = stmt.query_map(params![share_id], |r| {
            Ok(GrantInfo {
                label: r.get(0)?,
                access: r.get(1)?,
                created_at: r.get(2)?,
                revoked_at: r.get(3)?,
                last_seen_at: r.get(4)?,
                rw_paths: split_paths(&r.get::<_, String>(5)?),
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Validate a presented share secret. Returns the grant if valid and not revoked.
    pub fn check_grant(&self, share_id: &str, secret: &str) -> anyhow::Result<Option<GrantAccess>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row(
                "SELECT label, access, rw_paths FROM share_grants WHERE share_id = ?1 AND secret_hash = ?2 AND revoked_at IS NULL",
                params![share_id, token_hash(secret)],
                |r| Ok(GrantAccess { label: r.get(0)?, access: r.get(1)?, rw_paths: split_paths(&r.get::<_, String>(2)?) }),
            )
            .optional()?)
    }

    /// Record that a guest just authenticated with this secret.
    pub fn touch_grant(&self, share_id: &str, secret: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE share_grants SET last_seen_at = ?1 WHERE share_id = ?2 AND secret_hash = ?3",
            params![Self::now(), share_id, token_hash(secret)],
        )?;
        Ok(())
    }

    pub fn grant_secret_hash(&self, share_id: &str, label: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock();
        Ok(conn
            .query_row("SELECT secret_hash FROM share_grants WHERE share_id = ?1 AND label = ?2", params![share_id, label], |r| r.get(0))
            .optional()?)
    }

    /// Make a revoked ticket valid again. Returns its secret hash so mounts can be restored too.
    pub fn restore_grant(&self, share_id: &str, label: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock();
        let hash: Option<String> = conn
            .query_row(
                "SELECT secret_hash FROM share_grants WHERE share_id = ?1 AND label = ?2 AND revoked_at IS NOT NULL",
                params![share_id, label],
                |r| r.get(0),
            )
            .optional()?;
        if hash.is_some() {
            conn.execute("UPDATE share_grants SET revoked_at = NULL WHERE share_id = ?1 AND label = ?2", params![share_id, label])?;
        }
        Ok(hash)
    }

    /// Un-revoke the local mounts made with a secret. Returns their ids so bridges can be re-attached.
    pub fn restore_mounts_with_secret(&self, share_id: &str, secret_hash: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, secret FROM mounts WHERE share_id = ?1 AND revoked_at IS NOT NULL")?;
        let ids: Vec<String> = stmt
            .query_map(params![share_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter(|(_, s)| token_hash(&self.reveal_secret(s)) == secret_hash)
            .map(|(id, _)| id)
            .collect();
        for id in &ids {
            conn.execute("UPDATE mounts SET revoked_at = NULL WHERE id = ?1", params![id])?;
        }
        Ok(ids)
    }

    /// Remove a revoked ticket for good (it was dead already; this only tidies the list).
    pub fn delete_revoked_grant(&self, share_id: &str, label: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "DELETE FROM share_grants WHERE share_id = ?1 AND label = ?2 AND revoked_at IS NOT NULL",
            params![share_id, label],
        )?;
        Ok(n > 0)
    }

    pub fn revoke_grant(&self, share_id: &str, label: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE share_grants SET revoked_at = ?1 WHERE share_id = ?2 AND label = ?3 AND revoked_at IS NULL",
            params![Self::now(), share_id, label],
        )?;
        Ok(n > 0)
    }

    // ----------------------------------------------------------------- mounts

    #[allow(clippy::too_many_arguments)]
    pub fn create_mount(
        &self,
        user_id: &str,
        vault_id: &str,
        share_id: &str,
        share_name: &str,
        mount_prefix: &str,
        access: &str,
        rw_paths: &[String],
        home_node: &str,
        home_addrs: &[String],
        only_file: Option<&str>,
        secret: &str,
        forward_shares: &[String],
    ) -> anyhow::Result<MountInfo> {
        let m = MountInfo {
            id: new_id(),
            user_id: user_id.into(),
            vault_id: vault_id.into(),
            share_id: share_id.into(),
            share_name: share_name.into(),
            mount_prefix: mount_prefix.into(),
            access: access.into(),
            rw_paths: rw_paths.to_vec(),
            home_node: home_node.into(),
            home_addrs: home_addrs.to_vec(),
            only_file: only_file.map(str::to_string),
            forward_shares: forward_shares.to_vec(),
            secret: secret.into(),
            created_at: Self::now(),
            revoked_at: None,
        };
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO mounts (id, user_id, vault_id, share_id, share_name, mount_prefix, access, home_node, home_addrs, only_file, secret, created_at, rw_paths, forward_shares) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![m.id, m.user_id, m.vault_id, m.share_id, m.share_name, m.mount_prefix, m.access, m.home_node, m.home_addrs.join(" "), m.only_file, self.protect_secret(&m.secret), m.created_at, join_paths(&m.rw_paths), m.forward_shares.join(" ")],
        )?;
        Ok(m)
    }

    /// Replace the set of enclosing shares a mount's content is forwarded to.
    pub fn update_mount_forward(&self, id: &str, forward_shares: &[String]) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE mounts SET forward_shares = ?1 WHERE id = ?2", params![forward_shares.join(" "), id])? > 0)
    }

    /// Mark one mount revoked (used when the owner server denies our secret).
    pub fn revoke_mount(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE mounts SET revoked_at = ?1 WHERE id = ?2 AND revoked_at IS NULL", params![Self::now(), id])? > 0)
    }

    pub fn all_mounts(&self) -> anyhow::Result<Vec<MountInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{MOUNT_COLS} ORDER BY created_at"))?;
        let rows = stmt.query_map([], row_to_mount)?;
        let v: Vec<MountInfo> = rows.collect::<Result<_, _>>()?;
        drop(stmt);
        drop(conn);
        Ok(v.into_iter().map(|m| self.revealed(m)).collect())
    }

    pub fn all_shares(&self) -> anyhow::Result<Vec<ShareInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{SHARE_COLS} ORDER BY created_at"))?;
        let rows = stmt.query_map([], row_to_share)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn mount_by_id(&self, id: &str) -> anyhow::Result<Option<MountInfo>> {
        let conn = self.conn.lock();
        let m = conn.query_row(&format!("{MOUNT_COLS} WHERE id = ?1"), params![id], row_to_mount).optional()?;
        drop(conn);
        Ok(m.map(|m| self.revealed(m)))
    }

    fn revealed(&self, mut m: MountInfo) -> MountInfo {
        m.secret = self.reveal_secret(&m.secret);
        m
    }

    pub fn mounts_for_vault(&self, vault_id: &str) -> anyhow::Result<Vec<MountInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{MOUNT_COLS} WHERE vault_id = ?1 ORDER BY mount_prefix"))?;
        let rows = stmt.query_map(params![vault_id], row_to_mount)?;
        let v: Vec<MountInfo> = rows.collect::<Result<_, _>>()?;
        drop(stmt);
        drop(conn);
        Ok(v.into_iter().map(|m| self.revealed(m)).collect())
    }

    pub fn mounts_for_share(&self, share_id: &str) -> anyhow::Result<Vec<MountInfo>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!("{MOUNT_COLS} WHERE share_id = ?1"))?;
        let rows = stmt.query_map(params![share_id], row_to_mount)?;
        let v: Vec<MountInfo> = rows.collect::<Result<_, _>>()?;
        drop(stmt);
        drop(conn);
        Ok(v.into_iter().map(|m| self.revealed(m)).collect())
    }

    pub fn update_mount_prefix(&self, id: &str, prefix: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("UPDATE mounts SET mount_prefix = ?1 WHERE id = ?2", params![prefix, id])? > 0)
    }

    pub fn delete_mount(&self, id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM mounts WHERE id = ?1", params![id])? > 0)
    }

    pub fn revoke_mounts_with_secret(&self, share_id: &str, secret_hash: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare("SELECT id, secret FROM mounts WHERE share_id = ?1 AND revoked_at IS NULL")?;
        let ids: Vec<String> = stmt
            .query_map(params![share_id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .filter_map(|r| r.ok())
            .filter(|(_, s)| token_hash(&self.reveal_secret(s)) == secret_hash)
            .map(|(id, _)| id)
            .collect();
        for id in &ids {
            conn.execute("UPDATE mounts SET revoked_at = ?1 WHERE id = ?2", params![Self::now(), id])?;
        }
        Ok(ids)
    }
}

// ---------------------------------------------------------------------- schema migrations
//
// The CREATE TABLE IF NOT EXISTS block above describes the *current* shape and only ever
// creates missing tables. Anything that changes an existing table lives here as a numbered
// step, tracked in SQLite's `PRAGMA user_version`. Rules: steps are additive and idempotent,
// never drop a table that could hold user data (rename it `<name>_legacy_vN` instead), and
// bump REGISTRY_SCHEMA_VERSION together with the new step. See docs/STORAGE.md.

/// Current registry schema version (`PRAGMA user_version` after all steps ran).
pub const REGISTRY_SCHEMA_VERSION: u32 = 5;

fn migrate(conn: &Connection) -> anyhow::Result<()> {
    let mut version: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    let start = version;

    if version < 1 {
        // v1: the sharing model. Databases from the first scaffold carry an empty, reserved
        // `mounts` table with a different shape (vault_id, mount_path, share_id, remote_server).
        if !has_column(conn, "mounts", "user_id")? {
            let rows: i64 = conn.query_row("SELECT count(*) FROM mounts", [], |r| r.get(0))?;
            if rows > 0 {
                tracing::warn!("registry: keeping {rows} rows of the pre-sharing mounts table as mounts_legacy_v0");
                conn.execute_batch("ALTER TABLE mounts RENAME TO mounts_legacy_v0")?;
            } else {
                conn.execute_batch("DROP TABLE mounts")?;
            }
            conn.execute_batch(
                r#"
                CREATE TABLE mounts (
                    id           TEXT PRIMARY KEY,
                    user_id      TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
                    vault_id     TEXT NOT NULL REFERENCES vaults(id) ON DELETE CASCADE,
                    share_id     TEXT NOT NULL,
                    share_name   TEXT NOT NULL,
                    mount_prefix TEXT NOT NULL,
                    access       TEXT NOT NULL,
                    home_node    TEXT NOT NULL,
                    secret       TEXT NOT NULL,
                    created_at   TEXT NOT NULL,
                    revoked_at   TEXT
                );
                CREATE INDEX IF NOT EXISTS mounts_vault ON mounts(vault_id);
                CREATE INDEX IF NOT EXISTS mounts_share ON mounts(share_id);
                "#,
            )?;
        }
        version = 1;
    }

    if version < 2 {
        // v2: remote mounts (server-to-server) need the owner's addresses and the single-note flag.
        if !has_column(conn, "mounts", "home_addrs")? {
            conn.execute_batch("ALTER TABLE mounts ADD COLUMN home_addrs TEXT NOT NULL DEFAULT ''")?;
        }
        if !has_column(conn, "mounts", "only_file")? {
            conn.execute_batch("ALTER TABLE mounts ADD COLUMN only_file TEXT")?;
        }
        version = 2;
    }

    if version < 3 {
        // v3: guest mode shows the owner when a ticket was last used.
        if !has_column(conn, "share_grants", "last_seen_at")? {
            conn.execute_batch("ALTER TABLE share_grants ADD COLUMN last_seen_at TEXT")?;
        }
        version = 3;
    }

    if version < 4 {
        // v4: read-only tickets may name share-relative paths that stay writable.
        if !has_column(conn, "share_grants", "rw_paths")? {
            conn.execute_batch("ALTER TABLE share_grants ADD COLUMN rw_paths TEXT NOT NULL DEFAULT ''")?;
        }
        if !has_column(conn, "mounts", "rw_paths")? {
            conn.execute_batch("ALTER TABLE mounts ADD COLUMN rw_paths TEXT NOT NULL DEFAULT ''")?;
        }
        version = 4;
    }

    if version < 5 {
        // v5: a mount may sit inside one of the vault's own shares; this lists the shares that
        // carry it onward (empty = the enclosing shares skip it).
        if !has_column(conn, "mounts", "forward_shares")? {
            conn.execute_batch("ALTER TABLE mounts ADD COLUMN forward_shares TEXT NOT NULL DEFAULT ''")?;
        }
        version = 5;
    }

    if version > REGISTRY_SCHEMA_VERSION {
        anyhow::bail!(
            "registry.db schema is v{version} but this server only knows v{REGISTRY_SCHEMA_VERSION}; upgrade the server"
        );
    }
    if version != start {
        conn.pragma_update(None, "user_version", version)?;
        tracing::info!("registry: migrated schema v{start} -> v{version}");
    } else if start == 0 {
        conn.pragma_update(None, "user_version", REGISTRY_SCHEMA_VERSION)?;
    }
    Ok(())
}

fn has_column(conn: &Connection, table: &str, column: &str) -> anyhow::Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let cols: Vec<String> = stmt.query_map([], |r| r.get::<_, String>(1))?.collect::<Result<_, _>>()?;
    Ok(cols.iter().any(|c| c == column))
}

const SHARE_COLS: &str = "SELECT id, user_id, vault_id, name, prefix, only_file, created_at FROM shares";
const MOUNT_COLS: &str = "SELECT id, user_id, vault_id, share_id, share_name, mount_prefix, access, home_node, secret, created_at, revoked_at, home_addrs, only_file, rw_paths, forward_shares FROM mounts";

/// `rw_paths` are stored newline-separated: keys pass `valid_key`, so they never contain one.
fn join_paths(paths: &[String]) -> String {
    paths.join("\n")
}

fn split_paths(s: &str) -> Vec<String> {
    s.split('\n').filter(|p| !p.is_empty()).map(str::to_string).collect()
}

fn row_to_share(r: &rusqlite::Row) -> rusqlite::Result<ShareInfo> {
    Ok(ShareInfo {
        id: r.get(0)?,
        user_id: r.get(1)?,
        vault_id: r.get(2)?,
        name: r.get(3)?,
        prefix: r.get(4)?,
        only_file: r.get(5)?,
        created_at: r.get(6)?,
    })
}

fn row_to_mount(r: &rusqlite::Row) -> rusqlite::Result<MountInfo> {
    Ok(MountInfo {
        id: r.get(0)?,
        user_id: r.get(1)?,
        vault_id: r.get(2)?,
        share_id: r.get(3)?,
        share_name: r.get(4)?,
        mount_prefix: r.get(5)?,
        access: r.get(6)?,
        home_node: r.get(7)?,
        secret: r.get(8)?,
        created_at: r.get(9)?,
        revoked_at: r.get(10)?,
        home_addrs: r.get::<_, String>(11)?.split_whitespace().map(str::to_string).collect(),
        only_file: r.get(12)?,
        rw_paths: split_paths(&r.get::<_, String>(13)?),
        forward_shares: r.get::<_, String>(14)?.split_whitespace().map(str::to_string).collect(),
    })
}

fn row_to_vault(r: &rusqlite::Row) -> rusqlite::Result<VaultInfo> {
    Ok(VaultInfo { id: r.get(0)?, user_id: r.get(1)?, name: r.get(2)?, created_at: r.get(3)? })
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    fn open_with(sql: &str) -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.db");
        {
            let c = Connection::open(&path).unwrap();
            c.execute_batch(sql).unwrap();
        }
        let reg = Registry::open(&path).unwrap();
        (dir, reg)
    }

    fn user_version(reg: &Registry) -> u32 {
        reg.conn.lock().query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn fresh_db_is_current() {
        let (_d, reg) = open_with("");
        assert_eq!(user_version(&reg), REGISTRY_SCHEMA_VERSION);
        assert!(has_column(&reg.conn.lock(), "mounts", "home_addrs").unwrap());
    }

    #[test]
    fn scaffold_v0_reserved_mounts_table_is_replaced() {
        let (_d, reg) = open_with(
            "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL);
             CREATE TABLE vaults (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE mounts (id TEXT PRIMARY KEY, vault_id TEXT NOT NULL, mount_path TEXT NOT NULL, share_id TEXT NOT NULL, remote_server TEXT, created_at TEXT NOT NULL);",
        );
        assert_eq!(user_version(&reg), REGISTRY_SCHEMA_VERSION);
        let c = reg.conn.lock();
        assert!(has_column(&c, "mounts", "user_id").unwrap());
        assert!(has_column(&c, "mounts", "only_file").unwrap());
        drop(c);
        // The query that failed in the field now works.
        assert!(reg.mounts_for_vault("nope").unwrap().is_empty());
    }

    #[test]
    fn phase1_db_gets_v2_columns() {
        let (_d, reg) = open_with(
            "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL);
             CREATE TABLE vaults (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE mounts (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, vault_id TEXT NOT NULL, share_id TEXT NOT NULL, share_name TEXT NOT NULL, mount_prefix TEXT NOT NULL, access TEXT NOT NULL, home_node TEXT NOT NULL, secret TEXT NOT NULL, created_at TEXT NOT NULL, revoked_at TEXT);
             INSERT INTO mounts VALUES ('m1','u','v','s','S','Shared/S/','rw','node','sec','t',NULL);",
        );
        assert_eq!(user_version(&reg), REGISTRY_SCHEMA_VERSION);
        let m = reg.mount_by_id("m1").unwrap().unwrap();
        assert!(m.home_addrs.is_empty());
        assert_eq!(m.only_file, None);
        assert!(m.rw_paths.is_empty());
    }

    #[test]
    fn v3_db_gets_rw_paths_columns() {
        let (_d, reg) = open_with(
            "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL);
             CREATE TABLE vaults (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE shares (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, vault_id TEXT NOT NULL, name TEXT NOT NULL, prefix TEXT NOT NULL, only_file TEXT, created_at TEXT NOT NULL);
             CREATE TABLE share_grants (share_id TEXT NOT NULL, secret_hash TEXT NOT NULL, label TEXT NOT NULL, access TEXT NOT NULL, created_at TEXT NOT NULL, revoked_at TEXT, last_seen_at TEXT, PRIMARY KEY (share_id, label));
             CREATE TABLE mounts (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, vault_id TEXT NOT NULL, share_id TEXT NOT NULL, share_name TEXT NOT NULL, mount_prefix TEXT NOT NULL, access TEXT NOT NULL, home_node TEXT NOT NULL, secret TEXT NOT NULL, created_at TEXT NOT NULL, revoked_at TEXT, home_addrs TEXT NOT NULL DEFAULT '', only_file TEXT);
             INSERT INTO shares VALUES ('s1','u','v','S','Projects/',NULL,'t');
             INSERT INTO share_grants VALUES ('s1','h','ticket-1','ro','t',NULL,NULL);
             PRAGMA user_version = 3;",
        );
        assert_eq!(user_version(&reg), REGISTRY_SCHEMA_VERSION);
        let g = &reg.grants_for_share("s1").unwrap()[0];
        assert_eq!(g.access, "ro");
        assert!(g.rw_paths.is_empty());
        let secret = reg.create_grant("s1", "mixed", "ro", &["Inbox/".to_string(), "todo.md".to_string()]).unwrap();
        let ga = reg.check_grant("s1", &secret).unwrap().unwrap();
        assert_eq!(ga.rw_paths, vec!["Inbox/", "todo.md"]);
    }

    #[test]
    fn v4_db_gets_forward_shares_column() {
        let (_d, reg) = open_with(
            "CREATE TABLE users (id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE, created_at TEXT NOT NULL);
             CREATE TABLE vaults (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, name TEXT NOT NULL, created_at TEXT NOT NULL);
             CREATE TABLE shares (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, vault_id TEXT NOT NULL, name TEXT NOT NULL, prefix TEXT NOT NULL, only_file TEXT, created_at TEXT NOT NULL);
             CREATE TABLE share_grants (share_id TEXT NOT NULL, secret_hash TEXT NOT NULL, label TEXT NOT NULL, access TEXT NOT NULL, rw_paths TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL, revoked_at TEXT, last_seen_at TEXT, PRIMARY KEY (share_id, label));
             CREATE TABLE mounts (id TEXT PRIMARY KEY, user_id TEXT NOT NULL, vault_id TEXT NOT NULL, share_id TEXT NOT NULL, share_name TEXT NOT NULL, mount_prefix TEXT NOT NULL, access TEXT NOT NULL, home_node TEXT NOT NULL, secret TEXT NOT NULL, created_at TEXT NOT NULL, revoked_at TEXT, home_addrs TEXT NOT NULL DEFAULT '', only_file TEXT, rw_paths TEXT NOT NULL DEFAULT '');
             INSERT INTO mounts VALUES ('m1','u','v','s','S','Shared/S/','rw','node','sec','t',NULL,'',NULL,'');
             PRAGMA user_version = 4;",
        );
        assert_eq!(user_version(&reg), REGISTRY_SCHEMA_VERSION);
        let m = reg.mount_by_id("m1").unwrap().unwrap();
        assert!(m.forward_shares.is_empty(), "old mounts forward nothing");
        assert!(reg.update_mount_forward("m1", &["s1".to_string(), "s2".to_string()]).unwrap());
        assert_eq!(reg.mount_by_id("m1").unwrap().unwrap().forward_shares, vec!["s1", "s2"]);
    }

    #[test]
    fn newer_schema_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("registry.db");
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", 99u32).unwrap();
        }
        assert!(Registry::open(&path).is_err());
    }
}
