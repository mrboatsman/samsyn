//! HTTP API for creating shares, issuing tickets and mounting them.

use std::collections::HashMap;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use crate::access::{clean_rw_paths, describe};
use crate::ids::valid_name;
use crate::storage::{MountInfo, ShareInfo, User};
use crate::ticket::Ticket;
use crate::vault::overlaps;
use crate::ws::authenticate;
use crate::SharedState;

fn err(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(serde_json::json!({ "error": msg.into() }))).into_response()
}

fn internal(e: anyhow::Error) -> Response {
    // Full context goes to the log only; error strings can carry paths and ids.
    tracing::error!("sharing api: {e:#}");
    err(StatusCode::INTERNAL_SERVER_ERROR, "internal error; see the server log")
}

/// Normalise a user-typed path to a folder prefix ("a/b/") or a file key ("a/b.md").
fn clean_path(p: &str) -> String {
    p.trim().trim_matches('/').replace('\\', "/")
}

/// Share names are shown in UIs and logs and used in default folder names: printable, bounded.
fn clean_name(n: &str) -> String {
    let s: String = n.chars().filter(|c| !c.is_control()).collect();
    let s = s.trim();
    let s: String = s.chars().take(128).collect();
    if s.is_empty() { "share".to_string() } else { s }
}

fn user_vault(state: &SharedState, user: &User, vault: &str) -> Result<(crate::storage::VaultInfo, std::sync::Arc<crate::vault::LiveDoc>), Response> {
    if !valid_name(vault) {
        return Err(err(StatusCode::BAD_REQUEST, "invalid vault name"));
    }
    let doc = state.docs.get_or_load(user, vault).map_err(internal)?;
    let info = doc.vault_info().cloned().ok_or_else(|| err(StatusCode::INTERNAL_SERVER_ERROR, "not a vault"))?;
    Ok((info, doc))
}

/// A vault's shares and active mounts, for the overlap rules. Shares may nest inside each
/// other (the vault document is the hub, every bridge sees each change once), but no two
/// shares may have the same path. A mount is content other people share with us: it may sit
/// strictly inside a folder share (the owner then decides per share whether that share carries
/// it onward, see `MountInfo::forward_shares`), but nothing may be shared from inside a mount,
/// and mounts never overlap each other.
struct Taken {
    shares: Vec<ShareInfo>,
    /// Active mounts only. Revoked rows keep their folder out of the enclosing shares (see
    /// `vault::owner_excludes`) but do not block anything.
    mounts: Vec<MountInfo>,
}

fn taken_prefixes(state: &SharedState, vault_id: &str) -> anyhow::Result<Taken> {
    let reg = state.storage.registry();
    let shares = reg.shares_for_vault(vault_id)?;
    let mounts = reg.mounts_for_vault(vault_id)?.into_iter().filter(|m| m.revoked_at.is_none()).collect();
    Ok(Taken { shares, mounts })
}

fn share_full(s: &ShareInfo) -> String {
    share_full_path(&s.prefix, s.only_file.as_deref())
}

impl Taken {
    /// A new share at `full` (`Projects/X/`, `Notes/a.md`, `""` for the whole vault): refused
    /// with a reason, or allowed with the mounts strictly inside it, which the owner may or may
    /// not carry onward with the new share.
    fn refuses_share(&self, full: &str) -> Result<Vec<&MountInfo>, String> {
        let is_folder = full.is_empty() || full.ends_with('/');
        let mut inner = Vec::new();
        let mut blocking = Vec::new();
        for m in &self.mounts {
            if !overlaps(&m.mount_prefix, full) {
                continue;
            }
            if is_folder && m.mount_prefix != full && m.mount_prefix.starts_with(full) {
                inner.push(m);
            } else {
                blocking.push(show_path(m.mount_prefix.trim_end_matches('/')));
            }
        }
        if !blocking.is_empty() {
            return Err(format!("this path is inside a folder other people share with you ({}); their content cannot be shared onward", blocking.join(", ")));
        }
        if let Some(s) = self.shares.iter().find(|s| share_full(s) == full) {
            return Err(format!("{} is already shared as \"{}\"; issue another ticket for that share instead", show_path(full.trim_end_matches('/')), s.name));
        }
        Ok(inner)
    }

    /// A mount at the folder `prefix` (trailing slash): refused with a reason, or allowed with
    /// the folder shares that enclose it. `skip_mount` is the mount being moved, if any.
    fn refuses_mount(&self, prefix: &str, skip_mount: Option<&str>) -> Result<Vec<&ShareInfo>, String> {
        let mut enclosing = Vec::new();
        for s in &self.shares {
            let full = share_full(s);
            if !overlaps(&full, prefix) {
                continue;
            }
            if s.only_file.is_none() && prefix != full && prefix.starts_with(&full) {
                enclosing.push(s);
            } else {
                return Err(format!("mount path overlaps what you share as \"{}\" ({})", s.name, show_path(full.trim_end_matches('/'))));
            }
        }
        for m in &self.mounts {
            if Some(m.id.as_str()) == skip_mount {
                continue;
            }
            if overlaps(&m.mount_prefix, prefix) {
                return Err(format!("mount path overlaps a folder other people share with you ({})", show_path(m.mount_prefix.trim_end_matches('/'))));
            }
        }
        Ok(enclosing)
    }
}

/// The ids a request wants forwarding for, checked against the ids that are eligible.
/// Duplicates collapse; an id outside the eligible set is a 400 with `what` in the message.
fn chosen_ids(requested: Option<Vec<String>>, eligible: &[&str], what: &str) -> Result<Vec<String>, Response> {
    let mut out: Vec<String> = Vec::new();
    for id in requested.unwrap_or_default() {
        if !eligible.contains(&id.as_str()) {
            return Err(err(StatusCode::BAD_REQUEST, format!("{id} is not {what}")));
        }
        if !out.contains(&id) {
            out.push(id);
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ GET /api/vaults/{vault}/sharing

pub async fn get_sharing(
    Path(vault): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (info, _) = match user_vault(&state, &user, &vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let shares = match reg.shares_for_vault(&info.id) {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let mut shares_json = Vec::new();
    for s in shares {
        let grants = reg.grants_for_share(&s.id).unwrap_or_default();
        let all_mounts = reg.mounts_for_share(&s.id).unwrap_or_default();
        let mounts = all_mounts.iter().filter(|m| m.revoked_at.is_none()).count();
        // Local mounts carry the ticket secret, so they can be attributed to a ticket.
        let grants: Vec<serde_json::Value> = grants
            .iter()
            .map(|g| {
                let hash = reg.grant_secret_hash(&s.id, &g.label).ok().flatten();
                let mounted_by = all_mounts
                    .iter()
                    .filter(|m| m.revoked_at.is_none() && hash.as_deref() == Some(crate::ids::token_hash(&m.secret).as_str()))
                    .count();
                serde_json::json!({ "label": g.label, "access": g.access, "rw_paths": g.rw_paths, "created_at": g.created_at, "revoked_at": g.revoked_at, "mounted_by": mounted_by, "last_seen_at": g.last_seen_at })
            })
            .collect();
        shares_json.push(serde_json::json!({
            "id": s.id, "name": s.name, "prefix": s.prefix, "only_file": s.only_file,
            "path": match &s.only_file { Some(f) => format!("{}{}", s.prefix, f), None => s.prefix.trim_end_matches('/').to_string() },
            "created_at": s.created_at, "grants": grants, "mounted_by": mounts,
        }));
    }
    let mounts = reg.mounts_for_vault(&info.id).unwrap_or_default();
    let mounts_json: Vec<serde_json::Value> = mounts
        .iter()
        .map(|m| {
            let status = if m.revoked_at.is_some() {
                "revoked".to_string()
            } else if m.home_node != state.docs.node_id {
                format!("remote: {}", state.docs.link_status(&m.id).unwrap_or_else(|| "starting".into()))
            } else {
                "active".to_string()
            };
            serde_json::json!({
                "id": m.id, "share_id": m.share_id, "name": m.share_name, "mount_path": m.mount_prefix.trim_end_matches('/'),
                "access": m.access, "rw_paths": m.rw_paths, "home_node": m.home_node, "status": status, "created_at": m.created_at,
                "forward_shares": m.forward_shares,
            })
        })
        .collect();
    Json(serde_json::json!({ "node_id": state.docs.node_id, "public_url": state.serve.public_url, "shares": shares_json, "mounts": mounts_json })).into_response()
}

// ------------------------------------------------------------------ POST /api/vaults/{vault}/shares

#[derive(Deserialize)]
pub struct CreateShare {
    /// Folder, note, or empty for the whole vault.
    pub path: String,
    pub name: Option<String>,
    pub access: Option<String>,
    /// Share-relative paths the first ticket may write although it is read-only.
    pub rw_paths: Option<Vec<String>>,
    /// Ids of mounts inside the shared folder whose content the new share carries onward to
    /// its holders. Mounts inside the folder that are not listed are skipped by the share.
    pub forward_mounts: Option<Vec<String>>,
}

fn make_ticket(state: &SharedState, share: &ShareInfo, secret: String, access: &str, rw_paths: &[String]) -> anyhow::Result<String> {
    Ticket {
        version: crate::ticket::CURRENT_VERSION,
        share_id: share.id.clone(),
        name: share.name.clone(),
        node_id: state.docs.node_id.clone(),
        secret,
        access: access.to_string(),
        addrs: state.docs.endpoint.as_ref().map(crate::remote::addr_strings).unwrap_or_default(),
        url: state.serve.public_url.clone().unwrap_or_default().trim_end_matches('/').to_string(),
        rw_paths: rw_paths.to_vec(),
    }
    .encode()
}

/// `rw` or `ro`, with the writable paths of a read-only ticket validated against the share.
fn ticket_access(access: Option<String>, rw_paths: Option<Vec<String>>, prefix: &str, only_file: bool, keys: &[String]) -> Result<(String, Vec<String>), Response> {
    let access = access.unwrap_or_else(|| "rw".into());
    if access != "rw" && access != "ro" {
        return Err(err(StatusCode::BAD_REQUEST, "access must be rw or ro"));
    }
    let raw = rw_paths.unwrap_or_default();
    if access == "rw" && raw.iter().any(|p| !p.trim().is_empty()) {
        return Err(err(StatusCode::BAD_REQUEST, "writable paths only apply to read-only tickets"));
    }
    let rw_paths = clean_rw_paths(&raw, only_file, |k| keys.iter().any(|x| x == &format!("{prefix}{k}"))).map_err(|m| err(StatusCode::BAD_REQUEST, m))?;
    Ok((access, rw_paths))
}

fn share_full_path(prefix: &str, only_file: Option<&str>) -> String {
    match only_file {
        Some(f) => format!("{prefix}{f}"),
        None => prefix.to_string(),
    }
}

fn show_path(full: &str) -> String {
    if full.is_empty() { "(whole vault)".to_string() } else { full.to_string() }
}

pub async fn create_share(
    Path(vault): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<CreateShare>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (info, doc) = match user_vault(&state, &user, &vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let path = clean_path(&body.path);
    if !path.is_empty() && !crate::ids::valid_key(&path) {
        return err(StatusCode::BAD_REQUEST, "path contains '..', empty or control-character segments");
    }

    // Whole vault, folder or note? An empty path is the vault root; a note is an existing key;
    // a folder has keys below it.
    let keys = doc.keys();
    let (prefix, only_file) = if path.is_empty() {
        (String::new(), None)
    } else if keys.iter().any(|k| k == &path) {
        match path.rsplit_once('/') {
            Some((dir, file)) => (format!("{dir}/"), Some(file.to_string())),
            None => (String::new(), Some(path.clone())),
        }
    } else {
        let p = format!("{path}/");
        if !keys.iter().any(|k| k.starts_with(&p)) {
            return err(StatusCode::NOT_FOUND, format!("'{path}' is neither a synced note nor a folder with synced files"));
        }
        (p, None)
    };
    let (access, rw_paths) = match ticket_access(body.access, body.rw_paths, &prefix, only_file.is_some(), &keys) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let full = share_full_path(&prefix, only_file.as_deref());
    // Check + insert + attach under one lock, otherwise two concurrent creates could both pass.
    let _admin = state.docs.share_admin.lock();
    let taken = match taken_prefixes(&state, &info.id) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let inner = match taken.refuses_share(&full) {
        Ok(inner) => inner,
        Err(msg) => return err(StatusCode::CONFLICT, msg),
    };
    let inner_ids: Vec<&str> = inner.iter().map(|m| m.id.as_str()).collect();
    let forward_mounts = match chosen_ids(body.forward_mounts, &inner_ids, "a folder shared with you inside this path") {
        Ok(ids) => ids,
        Err(r) => return r,
    };
    let name = clean_name(&body.name.filter(|n| !n.trim().is_empty()).unwrap_or_else(|| {
        if path.is_empty() { info.name.clone() } else { path.rsplit('/').next().unwrap_or(&path).to_string() }
    }));

    let reg = state.storage.registry();
    let share = match reg.create_share(&user.id, &info.id, &name, &prefix, only_file.as_deref()) {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    // Record the forwarding before the bridge is built: it reads the mounts' lists.
    for m in inner.iter().filter(|m| forward_mounts.contains(&m.id)) {
        let mut ids = m.forward_shares.clone();
        ids.push(share.id.clone());
        if let Err(e) = reg.update_mount_forward(&m.id, &ids) {
            let _ = reg.delete_share(&share.id);
            return internal(e);
        }
    }
    let secret = match reg.create_grant(&share.id, "ticket-1", &access, &rw_paths) {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    if let Err(e) = state.docs.add_owner_bridge(&share) {
        let _ = reg.delete_share(&share.id);
        return internal(e);
    }
    let ticket = match make_ticket(&state, &share, secret, &access, &rw_paths) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let skipped = inner.len() - forward_mounts.len();
    tracing::info!(user = %user.name, share = %share.name, "share created for {} ({}); {} mount(s) inside carried onward, {skipped} skipped", show_path(&full), describe(&access, &rw_paths), forward_mounts.len());
    Json(serde_json::json!({ "share": share, "path": full.trim_end_matches('/'), "ticket": ticket, "label": "ticket-1", "access": access, "rw_paths": rw_paths, "forward_mounts": forward_mounts })).into_response()
}

// ------------------------------------------------------------------ POST /api/shares/{id}/tickets

#[derive(Deserialize, Default)]
pub struct NewTicket {
    pub label: Option<String>,
    pub access: Option<String>,
    /// Share-relative paths a read-only ticket may still write.
    pub rw_paths: Option<Vec<String>>,
}

fn owned_share(state: &SharedState, user: &User, id: &str) -> Result<ShareInfo, Response> {
    match state.storage.registry().share_by_id(id) {
        Ok(Some(s)) if s.user_id == user.id => Ok(s),
        Ok(_) => Err(err(StatusCode::NOT_FOUND, "no such share")),
        Err(e) => Err(internal(e)),
    }
}

pub async fn new_ticket(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    body: Option<Json<NewTicket>>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let share = match owned_share(&state, &user, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let body = body.map(|b| b.0).unwrap_or_default();
    let keys = match state.docs.load_vault_by_id(&share.vault_id) {
        Ok(d) => d.keys(),
        Err(e) => return internal(e),
    };
    let (access, rw_paths) = match ticket_access(body.access, body.rw_paths, &share.prefix, share.only_file.is_some(), &keys) {
        Ok(a) => a,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let label = body.label.filter(|l| !l.trim().is_empty()).unwrap_or_else(|| {
        let n = reg.grants_for_share(&share.id).map(|g| g.len()).unwrap_or(0) + 1;
        format!("ticket-{n}")
    });
    let secret = match reg.create_grant(&share.id, &label, &access, &rw_paths) {
        Ok(s) => s,
        Err(e) => return err(StatusCode::CONFLICT, e.to_string()),
    };
    match make_ticket(&state, &share, secret, &access, &rw_paths) {
        Ok(t) => Json(serde_json::json!({ "ticket": t, "label": label, "access": access, "rw_paths": rw_paths })).into_response(),
        Err(e) => internal(e),
    }
}

// ------------------------------------------------------------------ DELETE /api/shares/{id}/grants/{label}

pub async fn revoke_ticket(
    Path((id, label)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let share = match owned_share(&state, &user, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let hash = match reg.grant_secret_hash(&share.id, &label) {
        Ok(Some(h)) => h,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such ticket"),
        Err(e) => return internal(e),
    };
    // Already revoked: a second DELETE removes the row entirely (tidy the list).
    let already = reg.grants_for_share(&share.id).unwrap_or_default().iter().any(|g| g.label == label && g.revoked_at.is_some());
    if already {
        return match reg.delete_revoked_grant(&share.id, &label) {
            Ok(_) => StatusCode::NO_CONTENT.into_response(),
            Err(e) => internal(e),
        };
    }
    if let Err(e) = reg.revoke_grant(&share.id, &label) {
        return internal(e);
    }
    match reg.revoke_mounts_with_secret(&share.id, &hash) {
        Ok(ids) => state.docs.remove_bridges_for_mounts(&ids),
        Err(e) => return internal(e),
    }
    StatusCode::NO_CONTENT.into_response()
}

// ------------------------------------------------------------------ POST /api/shares/{id}/grants/{label}/restore

/// Un-revoke a ticket: the same ticket works again. Guests reconnect by themselves; mounts on
/// this server are re-attached; mounts on other servers have to be mounted again.
pub async fn restore_ticket(
    Path((id, label)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let share = match owned_share(&state, &user, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let hash = match reg.restore_grant(&share.id, &label) {
        Ok(Some(h)) => h,
        Ok(None) => return err(StatusCode::NOT_FOUND, "no such revoked ticket"),
        Err(e) => return internal(e),
    };
    let mut restored = 0;
    match reg.restore_mounts_with_secret(&share.id, &hash) {
        Ok(ids) => {
            for mid in ids {
                if let Ok(Some(m)) = reg.mount_by_id(&mid) {
                    if m.home_node == state.docs.node_id {
                        match state.docs.add_mount_bridge(&m) {
                            Ok(()) => restored += 1,
                            Err(e) => tracing::warn!(mount = %mid, "re-attaching restored mount failed: {e}"),
                        }
                    }
                }
            }
        }
        Err(e) => return internal(e),
    }
    tracing::info!(user = %user.name, share = %share.name, "ticket {label} restored, {restored} local mount(s) re-attached");
    Json(serde_json::json!({ "restored_mounts": restored })).into_response()
}

// ------------------------------------------------------------------ DELETE /api/shares/{id}

pub async fn delete_share(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let share = match owned_share(&state, &user, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let mount_ids: Vec<String> = reg.mounts_for_share(&share.id).unwrap_or_default().into_iter().map(|m| m.id).collect();
    state.docs.remove_bridges_for_mounts(&mount_ids);
    state.docs.remove_bridges_for_share(&share.id);
    for m in &mount_ids {
        let _ = reg.delete_mount(m);
    }
    if let Err(e) = reg.delete_share(&share.id) {
        return internal(e);
    }
    let dir = state.storage.share_dir(&share.user_id, &share.id);
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        tracing::warn!("removing share dir {} failed: {e}", dir.display());
    }
    tracing::info!(user = %user.name, share = %share.name, "share deleted");
    StatusCode::NO_CONTENT.into_response()
}

// ------------------------------------------------------------------ POST /api/vaults/{vault}/mounts

#[derive(Deserialize)]
pub struct CreateMount {
    pub ticket: String,
    pub mount_path: Option<String>,
    /// Ids of this vault's own shares that enclose `mount_path` and should carry the mounted
    /// content onward to their holders. Enclosing shares not listed skip the folder.
    pub forward_shares: Option<Vec<String>>,
}

pub async fn create_mount(
    Path(vault): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<CreateMount>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let (info, doc) = match user_vault(&state, &user, &vault) {
        Ok(v) => v,
        Err(r) => return r,
    };
    let ticket = match Ticket::decode(&body.ticket) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("bad ticket: {e}")),
    };
    let reg = state.storage.registry();
    let remote = ticket.node_id != state.docs.node_id;
    // What the owner granted: (label for the log, access, writable paths, share name, single-note flag).
    let (label, grant_access, rw_paths, share_name, only_file) = if remote {
        let Some(ep) = state.docs.endpoint.as_ref() else {
            return err(StatusCode::NOT_IMPLEMENTED, "this ticket is for a share on another server and p2p is disabled on this one (--no-p2p)");
        };
        match crate::remote::probe(ep, &ticket).await {
            Ok(g) => ("p2p".to_string(), g.access, g.rw_paths, g.name, g.only_file),
            Err(e) => return err(StatusCode::BAD_GATEWAY, e.to_string()),
        }
    } else {
        let share = match reg.share_by_id(&ticket.share_id) {
            Ok(Some(s)) => s,
            Ok(None) => return err(StatusCode::NOT_FOUND, "the share in this ticket no longer exists"),
            Err(e) => return internal(e),
        };
        if share.vault_id == info.id {
            return err(StatusCode::BAD_REQUEST, "you cannot mount your own share into the same vault");
        }
        match reg.check_grant(&share.id, &ticket.secret) {
            Ok(Some(g)) => (g.label, g.access, g.rw_paths, share.name.clone(), share.only_file.clone()),
            Ok(None) => return err(StatusCode::FORBIDDEN, "ticket is invalid or has been revoked"),
            Err(e) => return internal(e),
        }
    };
    // Anything but an explicit "rw" from both the grant and the ticket means read-only; a
    // peer server could answer with any string. The writable paths are the grant's (the
    // registry here, the owner's hello for remote shares), never the ticket's.
    let access = if grant_access == "rw" && ticket.access == "rw" { "rw" } else { "ro" };
    let rw_paths = if access == "rw" { Vec::new() } else { rw_paths };
    // A single-note share's file name comes from the peer too: one path segment or nothing.
    let only_file = only_file.filter(|f| crate::ids::valid_key(f) && !f.contains('/') && !f.starts_with('.'));

    let share_name = clean_name(&share_name);
    let mount_path = clean_path(&body.mount_path.unwrap_or_default());
    let mount_path = if mount_path.is_empty() {
        let candidate = format!("Shared/{}", share_name.replace('/', "-"));
        if crate::ids::valid_key(&candidate) && !candidate.split('/').any(|s| s.starts_with('.')) { candidate } else { format!("Shared/{}", ticket.share_id) }
    } else {
        mount_path
    };
    if !crate::ids::valid_key(&mount_path) || mount_path.split('/').any(|s| s.starts_with('.')) {
        return err(StatusCode::BAD_REQUEST, "mount path contains '..', hidden, empty or control-character segments");
    }
    let prefix = format!("{mount_path}/");
    let _admin = state.docs.share_admin.lock();
    let taken = match taken_prefixes(&state, &info.id) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let enclosing = match taken.refuses_mount(&prefix, None) {
        Ok(e) => e,
        Err(msg) => return err(StatusCode::CONFLICT, msg),
    };
    let enclosing_ids: Vec<&str> = enclosing.iter().map(|s| s.id.as_str()).collect();
    let forward_shares = match chosen_ids(body.forward_shares, &enclosing_ids, "a share of yours that contains this folder") {
        Ok(ids) => ids,
        Err(r) => return r,
    };
    if doc.keys().iter().any(|k| k.starts_with(&prefix)) {
        return err(StatusCode::CONFLICT, format!("'{mount_path}' already contains files; choose an empty or new folder"));
    }
    let mount = match reg.create_mount(
        &user.id,
        &info.id,
        &ticket.share_id,
        &share_name,
        &prefix,
        access,
        &rw_paths,
        &ticket.node_id,
        &ticket.addrs,
        only_file.as_deref(),
        &ticket.secret,
        &forward_shares,
    ) {
        Ok(m) => m,
        Err(e) => return internal(e),
    };
    if let Err(e) = state.docs.add_mount_bridge(&mount) {
        let _ = reg.delete_mount(&mount.id);
        return internal(e);
    }
    let skipped = enclosing.len() - forward_shares.len();
    tracing::info!(user = %user.name, share = %share_name, remote, "mounted at {prefix} via {label} ({}); carried onward by {} enclosing share(s), skipped by {skipped}", describe(access, &rw_paths), forward_shares.len());
    Json(serde_json::json!({ "mount": mount, "mount_path": mount_path, "access": access, "rw_paths": rw_paths, "forward_shares": forward_shares })).into_response()
}

// ------------------------------------------------------------------ PUT /api/mounts/{id}/forward

#[derive(Deserialize)]
pub struct ForwardShares {
    /// The complete new set: enclosing shares that carry this mount onward.
    pub share_ids: Vec<String>,
}

/// Change which of the enclosing shares carry a mount's content onward. A share that stops
/// carrying it drops the folder from its holders; one that starts gets the folder copied in.
pub async fn set_mount_forward(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<ForwardShares>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let mount = match reg.mount_by_id(&id) {
        Ok(Some(m)) if m.user_id == user.id => m,
        Ok(_) => return err(StatusCode::NOT_FOUND, "no such mount"),
        Err(e) => return internal(e),
    };
    if mount.revoked_at.is_some() {
        return err(StatusCode::CONFLICT, "this mount was revoked; its folder is never carried onward");
    }
    let _admin = state.docs.share_admin.lock();
    let taken = match taken_prefixes(&state, &mount.vault_id) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let enclosing = match taken.refuses_mount(&mount.mount_prefix, Some(&mount.id)) {
        Ok(e) => e,
        Err(msg) => return err(StatusCode::CONFLICT, msg),
    };
    let enclosing_ids: Vec<&str> = enclosing.iter().map(|s| s.id.as_str()).collect();
    let share_ids = match chosen_ids(Some(body.share_ids), &enclosing_ids, "a share of yours that contains this folder") {
        Ok(ids) => ids,
        Err(r) => return r,
    };
    if let Err(e) = reg.update_mount_forward(&mount.id, &share_ids) {
        return internal(e);
    }
    if let Err(e) = state.docs.refresh_owner_bridges(&mount.vault_id, std::slice::from_ref(&mount.mount_prefix)) {
        return internal(e);
    }
    tracing::info!(user = %user.name, "mount {} now carried onward by {} of {} enclosing share(s)", mount.mount_prefix, share_ids.len(), enclosing.len());
    Json(serde_json::json!({ "forward_shares": share_ids })).into_response()
}

// ------------------------------------------------------------------ PATCH /api/mounts/{id}, PATCH /api/shares/{id}

#[derive(Deserialize)]
pub struct MovePath {
    /// Mounts: the new folder. Shares: the new folder or note path.
    #[serde(alias = "mount_path")]
    pub path: String,
}

/// Move a mount to another folder. Used by the plugin when the user moves the folder in
/// Obsidian (so the move never looks like a deletion to the owner) and by the Move… dialog.
pub async fn move_mount(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<MovePath>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let mount = match reg.mount_by_id(&id) {
        Ok(Some(m)) if m.user_id == user.id => m,
        Ok(_) => return err(StatusCode::NOT_FOUND, "no such mount"),
        Err(e) => return internal(e),
    };
    if mount.revoked_at.is_some() {
        return err(StatusCode::CONFLICT, "this mount was revoked; remove it instead");
    }
    let new_path = clean_path(&body.path);
    if new_path.is_empty() {
        return err(StatusCode::BAD_REQUEST, "path is required");
    }
    if !crate::ids::valid_key(&new_path) || new_path.split('/').any(|s| s.starts_with('.')) {
        return err(StatusCode::BAD_REQUEST, "path contains '..', hidden, empty or control-character segments");
    }
    let new_prefix = format!("{new_path}/");
    if new_prefix == mount.mount_prefix {
        return Json(serde_json::json!({ "mount_path": new_path })).into_response();
    }
    let _admin = state.docs.share_admin.lock();
    let taken = match taken_prefixes(&state, &mount.vault_id) {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    let enclosing = match taken.refuses_mount(&new_prefix, Some(&mount.id)) {
        Ok(e) => e,
        Err(msg) => return err(StatusCode::CONFLICT, format!("the new folder is not allowed: {msg}")),
    };
    // Forwarding is a decision about a specific enclosing share. It follows the mount while it
    // stays inside that share and lapses when it leaves; a share it moves into starts out
    // skipping it, the safe default (the UI offers the choice again).
    let kept: Vec<String> = mount.forward_shares.iter().filter(|id| enclosing.iter().any(|s| &s.id == *id)).cloned().collect();
    let doc = match state.docs.load_vault_by_id(&mount.vault_id) {
        Ok(d) => d,
        Err(e) => return internal(e),
    };
    // The target may already hold exactly the moved files (Obsidian moved them before telling
    // us), but never unrelated ones.
    let old_keys: Vec<String> = doc.keys().into_iter().filter(|k| k.starts_with(&mount.mount_prefix)).collect();
    let stray = doc.keys().into_iter().any(|k| {
        k.starts_with(&new_prefix) && !old_keys.iter().any(|o| format!("{new_prefix}{}", &o[mount.mount_prefix.len()..]) == k)
    });
    if stray {
        return err(StatusCode::CONFLICT, format!("'{new_path}' already contains other files; choose an empty or new folder"));
    }
    if kept != mount.forward_shares {
        if let Err(e) = reg.update_mount_forward(&mount.id, &kept) {
            return internal(e);
        }
    }
    if let Err(e) = state.docs.move_prefix(&mount.vault_id, &mount.share_id, Some(&mount.id), &mount.mount_prefix, &new_prefix, None, None) {
        return internal(e);
    }
    tracing::info!(user = %user.name, "moved mount {} -> {}", mount.mount_prefix, new_prefix);
    Json(serde_json::json!({ "mount_path": new_path })).into_response()
}

/// Move a share's folder or note within the owner's vault. What is shared does not change.
pub async fn move_share(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<MovePath>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let share = match owned_share(&state, &user, &id) {
        Ok(s) => s,
        Err(r) => return r,
    };
    if share.prefix.is_empty() && share.only_file.is_none() {
        return err(StatusCode::BAD_REQUEST, "the whole vault is shared; there is nothing to move");
    }
    let new_path = clean_path(&body.path);
    if new_path.is_empty() {
        return err(StatusCode::BAD_REQUEST, "path is required");
    }
    if !crate::ids::valid_key(&new_path) {
        return err(StatusCode::BAD_REQUEST, "path contains '..', empty or control-character segments");
    }
    let (new_prefix, new_file) = if share.only_file.is_some() {
        match new_path.rsplit_once('/') {
            Some((dir, file)) => (format!("{dir}/"), Some(file.to_string())),
            None => (String::new(), Some(new_path.clone())),
        }
    } else {
        (format!("{new_path}/"), None)
    };
    let full = match &new_file { Some(f) => format!("{new_prefix}{f}"), None => new_prefix.clone() };
    let old_full = match &share.only_file { Some(f) => format!("{}{}", share.prefix, f), None => share.prefix.clone() };
    if full == old_full {
        return Json(serde_json::json!({ "path": new_path })).into_response();
    }
    let _admin = state.docs.share_admin.lock();
    match taken_prefixes(&state, &share.vault_id) {
        Ok(taken) => {
            // Other shares nested inside this one would be carried along without their
            // registry rows following; refuse rather than guess.
            let inner: Vec<String> = taken.shares.iter().map(share_full).filter(|p| *p != old_full && p.starts_with(&old_full)).collect();
            if !inner.is_empty() {
                return err(StatusCode::CONFLICT, format!("{} contains other shares ({}); move or unshare those first", show_path(old_full.trim_end_matches('/')), inner.iter().map(|p| p.trim_end_matches('/')).collect::<Vec<_>>().join(", ")));
            }
            // Mounts inside the share would be carried along without their rows following.
            let inner_mounts: Vec<&str> = taken.mounts.iter().filter(|m| share.only_file.is_none() && m.mount_prefix.starts_with(&old_full)).map(|m| m.mount_prefix.trim_end_matches('/')).collect();
            if !inner_mounts.is_empty() {
                return err(StatusCode::CONFLICT, format!("{} contains folders other people share with you ({}); move or unmount those first", show_path(old_full.trim_end_matches('/')), inner_mounts.join(", ")));
            }
            let others = Taken { shares: taken.shares.iter().filter(|s| share_full(s) != old_full).cloned().collect(), mounts: taken.mounts.clone() };
            match others.refuses_share(&full) {
                Err(msg) => return err(StatusCode::CONFLICT, msg),
                Ok(mounts_inside) if !mounts_inside.is_empty() => {
                    return err(StatusCode::CONFLICT, "the new path would contain folders other people share with you; share it anew there instead");
                }
                Ok(_) => {}
            }
            if others.shares.iter().any(|s| share_full(s).starts_with(&full)) {
                return err(StatusCode::CONFLICT, "the new path would contain another share");
            }
        }
        Err(e) => return internal(e),
    }
    let doc = match state.docs.load_vault_by_id(&share.vault_id) {
        Ok(d) => d,
        Err(e) => return internal(e),
    };
    let keys = doc.keys();
    let stray = keys.iter().any(|k| {
        let under_new = if new_file.is_some() { *k == full } else { k.starts_with(&new_prefix) };
        under_new && !keys.iter().any(|o| {
            let under_old = if share.only_file.is_some() { *o == old_full } else { o.starts_with(&share.prefix) };
            under_old && format!("{full}{}", &o[old_full.len()..]) == *k
        })
    });
    if stray {
        return err(StatusCode::CONFLICT, format!("'{new_path}' already contains other files"));
    }
    if let Err(e) = state.docs.move_prefix(&share.vault_id, &share.id, None, &share.prefix, &new_prefix, share.only_file.as_deref(), new_file.as_deref()) {
        return internal(e);
    }
    tracing::info!(user = %user.name, share = %share.name, "moved share {old_full} -> {full}");
    Json(serde_json::json!({ "path": new_path })).into_response()
}

// ------------------------------------------------------------------ POST /api/tickets/inspect

#[derive(Deserialize)]
pub struct InspectTicket {
    pub ticket: String,
}

/// Tell the UI what a pasted ticket is before mounting it: name, access, which server it
/// belongs to and, for shares on this server, whether it is still valid.
pub async fn inspect_ticket(
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
    Json(body): Json<InspectTicket>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let ticket = match Ticket::decode(&body.ticket) {
        Ok(t) => t,
        Err(e) => return err(StatusCode::BAD_REQUEST, format!("bad ticket: {e}")),
    };
    let same_server = ticket.node_id == state.docs.node_id;
    let (valid, access, rw_paths, own) = if same_server {
        let reg = state.storage.registry();
        match reg.share_by_id(&ticket.share_id) {
            Ok(Some(s)) => match reg.check_grant(&s.id, &ticket.secret) {
                Ok(Some(g)) => {
                    let a = if g.access == "ro" || ticket.access == "ro" { "ro" } else { "rw" };
                    let paths = if a == "rw" { Vec::new() } else { g.rw_paths };
                    (Some(true), a.to_string(), paths, Some(s.vault_id))
                }
                _ => (Some(false), ticket.access.clone(), ticket.rw_paths.clone(), None),
            },
            _ => (Some(false), ticket.access.clone(), ticket.rw_paths.clone(), None),
        }
    } else {
        (None, ticket.access.clone(), ticket.rw_paths.clone(), None)
    };
    // Only tell the caller about the share's vault when it is their own vault.
    let own_vault = own
        .and_then(|vid| state.storage.registry().vault_by_id(&vid).ok().flatten())
        .filter(|v| v.user_id == user.id)
        .map(|v| v.name);
    Json(serde_json::json!({
        "share_id": ticket.share_id,
        "name": ticket.name,
        "access": access,
        "rw_paths": rw_paths,
        "node_id": ticket.node_id,
        "same_server": same_server,
        "p2p_available": state.docs.endpoint.is_some(),
        "url": ticket.url,
        "valid": valid,
        "from_own_vault": own_vault,
    }))
    .into_response()
}

// ------------------------------------------------------------------ DELETE /api/mounts/{id}

pub async fn delete_mount(
    Path(id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let reg = state.storage.registry();
    let mount = match reg.mount_by_id(&id) {
        Ok(Some(m)) if m.user_id == user.id => m,
        Ok(_) => return err(StatusCode::NOT_FOUND, "no such mount"),
        Err(e) => return internal(e),
    };
    state.docs.remove_bridges_for_mounts(std::slice::from_ref(&mount.id));
    if let Err(e) = reg.delete_mount(&mount.id) {
        return internal(e);
    }
    // The folder is ordinary notes now; an enclosing share that skipped it carries it like
    // the rest of the vault (the UI says so before unmounting).
    if let Err(e) = state.docs.refresh_owner_bridges(&mount.vault_id, std::slice::from_ref(&mount.mount_prefix)) {
        tracing::warn!(user = %user.name, "refreshing shares around {} failed: {e}", mount.mount_prefix);
    }
    if mount.home_node != state.docs.node_id {
        let dir = state.storage.mount_dir(&mount.user_id, &mount.id);
        if dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&dir) {
                tracing::warn!("removing replica dir {} failed: {e}", dir.display());
            }
        }
    }
    tracing::info!(user = %user.name, "unmounted {}", mount.mount_prefix);
    StatusCode::NO_CONTENT.into_response()
}
