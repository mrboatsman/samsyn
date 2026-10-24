//! The sync engine: owns the document, reconciles with disk, pushes local edits, writes remote ones.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::Context;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use yrs::sync::{Awareness, DefaultProtocol, Message, MessageReader, Protocol, SyncMessage};
use yrs::types::{Event, PathSegment};
use yrs::updates::decoder::DecoderV1;
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Any, DeepObservable, Doc, GetString, Map, MapRef, Observable, Origin, Out, Text, TextPrelim, Transact};

use super::diff::diff_text;
use super::state::{hash_bytes, write_atomic, LocalState, StateStore, META_DIR};
use super::SyncArgs;

const LOCAL: &str = "local";
const MASS_DELETE_MIN: usize = 5;
const MASS_DELETE_RATIO: f64 = 0.5;

pub struct Engine {
    args: SyncArgs,
    root: PathBuf,
    awareness: Awareness,
    texts: MapRef,
    blobs: MapRef,
    store: StateStore,
    state: LocalState,
    device: String,
    text_exts: HashSet<String>,
    excludes: Vec<String>,
    http: reqwest::Client,
    conn: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>>,
    /// Paths changed by remote transactions, drained after every incoming message.
    dirty_remote: Arc<Mutex<HashSet<String>>>,
    doc_dirty: Arc<Mutex<bool>>,
    /// Text content each file had when disk and document last agreed. Used as the common
    /// ancestor for a three-way merge when a remote change lands while a local edit is pending.
    base: HashMap<String, String>,
    state_dirty: bool,
    root_missing: bool,
    /// (when, how many) shared files we removed recently; feeds the mass-delete guard.
    recent_deletes: VecDeque<(Instant, usize)>,
    /// Blobs the document references but the server did not have yet: (meta, retry at, attempts).
    pending_blobs: HashMap<String, (BlobMeta, Instant, u32)>,
    /// Mounts ("Shared/X/") that are read-only for us, each with the vault-relative paths the
    /// ticket still lets us write; local edits on the read-only part are not pushed.
    ro_areas: Vec<RoArea>,
    /// Everything shared with other people (our shares and our mounts): moving these, or moving
    /// files across their boundary, is refused like the plugin does.
    shared_roots: Vec<SharedRoot>,
    pub synced: bool,
    pub had_cached_doc: bool,
    pub pending_deletions: Vec<String>,
}

impl Engine {
    pub fn new(args: &SyncArgs) -> anyhow::Result<Self> {
        let root = std::fs::canonicalize(&args.dir)?;
        let store = StateStore::new(&root)?;
        let install_id = StateStore::install_id(&root);
        let state = store.load_state(&install_id);

        let doc = Doc::new();
        let had_cached_doc = store.load_doc(&doc);
        let texts = doc.get_or_insert_map("texts");
        let blobs = doc.get_or_insert_map("blobs");

        let conn: Arc<Mutex<Option<mpsc::UnboundedSender<Vec<u8>>>>> = Arc::new(Mutex::new(None));
        let dirty_remote: Arc<Mutex<HashSet<String>>> = Arc::new(Mutex::new(HashSet::new()));
        let doc_dirty = Arc::new(Mutex::new(false));

        // Local commits go to the server; every commit marks the cache dirty.
        {
            let conn = conn.clone();
            let doc_dirty = doc_dirty.clone();
            doc.observe_update_v1("samsyn-client", move |txn, ev| {
                *doc_dirty.lock() = true;
                if is_local(txn.origin()) {
                    if let Some(tx) = conn.lock().as_ref() {
                        let _ = tx.send(encode(&Message::Sync(SyncMessage::Update(ev.update.clone()))));
                    }
                }
            })?;
        }
        // Remote changes are collected by path and written to disk after the message is handled.
        {
            let dirty = dirty_remote.clone();
            texts.observe_deep("samsyn-client", move |txn, events| {
                if is_local(txn.origin()) {
                    return;
                }
                let mut d = dirty.lock();
                for ev in events.iter() {
                    match ev {
                        Event::Map(m) if ev.path().is_empty() => {
                            for k in m.keys(txn).keys() {
                                d.insert(k.to_string());
                            }
                        }
                        _ => {
                            if let Some(PathSegment::Key(k)) = ev.path().front() {
                                d.insert(k.to_string());
                            }
                        }
                    }
                }
            });
        }
        {
            let dirty = dirty_remote.clone();
            blobs.observe("samsyn-client", move |txn, ev| {
                if is_local(txn.origin()) {
                    return;
                }
                let mut d = dirty.lock();
                for k in ev.keys(txn).keys() {
                    d.insert(k.to_string());
                }
            });
        }

        let device = args
            .device
            .clone()
            .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().to_string());
        let mut awareness = Awareness::new(doc);
        let _ = awareness.set_local_state(serde_json::json!({ "user": { "name": device } }));

        let text_exts = args
            .text_ext
            .split(',')
            .map(|s| s.trim().trim_start_matches('.').to_ascii_lowercase())
            .filter(|s| !s.is_empty())
            .collect();

        Ok(Self {
            args: args.clone(),
            root,
            awareness,
            texts,
            blobs,
            store,
            state,
            device,
            text_exts,
            excludes: args.exclude.clone(),
            http: reqwest::Client::new(),
            conn,
            dirty_remote,
            doc_dirty,
            base: HashMap::new(),
            state_dirty: false,
            root_missing: false,
            recent_deletes: VecDeque::new(),
            pending_blobs: HashMap::new(),
            ro_areas: Vec::new(),
            shared_roots: Vec::new(),
            synced: false,
            had_cached_doc,
            pending_deletions: Vec::new(),
        })
    }

    // ------------------------------------------------------------------ helpers

    fn doc(&self) -> &Doc {
        self.awareness.doc()
    }

    fn rel(&self, abs: &Path) -> Option<String> {
        let r = abs.strip_prefix(&self.root).ok()?;
        let s = r.to_string_lossy().replace('\\', "/");
        if s.is_empty() {
            None
        } else {
            Some(s)
        }
    }

    /// Keys that become files pass `should_sync` first (safe relative keys). Prefixes ("" for a
    /// guest's whole directory, "Shared/X") also come through here, so no assertion: an
    /// absolute key is simply refused.
    fn abs(&self, rel: &str) -> PathBuf {
        if rel.starts_with('/') {
            tracing::error!("refusing absolute key {rel:?}");
            return self.root.join(META_DIR).join("refused");
        }
        self.root.join(rel)
    }

    fn is_text(&self, rel: &str) -> bool {
        rel.rsplit_once('.')
            .map(|(_, ext)| self.text_exts.contains(&ext.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    fn should_sync(&self, rel: &str) -> bool {
        // Keys come from other people's documents: anything that could leave the directory
        // (absolute, "..", empty segments, control characters) is ignored outright.
        if !crate::ids::valid_key(rel) {
            return false;
        }
        if rel.split('/').any(|seg| seg.starts_with('.')) {
            return false;
        }
        // A guest's whole directory is the share; files moved aside from a read-only share must
        // not be pushed back into it (they would be reverted again) and are left alone.
        if self.args.is_guest() && rel.starts_with("read-only copy/") {
            return false;
        }
        if rel.starts_with(META_DIR) {
            return false;
        }
        if self.excludes.iter().any(|e| rel == e.trim_end_matches('/') || rel.starts_with(&format!("{}/", e.trim_end_matches('/')))) {
            return false;
        }
        self.is_text(rel) || !self.args.no_binary
    }

    fn mark_synced(&mut self, rel: &str, hash: String) {
        if self.state.synced.get(rel) != Some(&hash) {
            self.state.synced.insert(rel.to_string(), hash);
            self.state_dirty = true;
        }
    }

    fn forget_synced(&mut self, rel: &str) {
        if self.state.synced.remove(rel).is_some() {
            self.state_dirty = true;
        }
    }

    fn text_of(&self, rel: &str) -> Option<String> {
        let txn = self.doc().transact();
        match self.texts.get(&txn, rel) {
            Some(Out::YText(t)) => Some(t.get_string(&txn)),
            _ => None,
        }
    }

    fn blob_of(&self, rel: &str) -> Option<BlobMeta> {
        let txn = self.doc().transact();
        match self.blobs.get(&txn, rel) {
            Some(Out::Any(Any::Map(m))) => BlobMeta::from_any(&m),
            _ => None,
        }
    }

    fn doc_keys(&self) -> (Vec<String>, Vec<String>) {
        let txn = self.doc().transact();
        let t: Vec<String> = self.texts.keys(&txn).map(|k| k.to_string()).collect();
        let b: Vec<String> = self.blobs.keys(&txn).map(|k| k.to_string()).collect();
        (t, b)
    }

    fn conflict_path(&self, rel: &str) -> String {
        let (base, ext) = match rel.rsplit_once('.') {
            Some((b, e)) if !b.contains('/') || b.rfind('/') < rel.rfind('.') => (b.to_string(), format!(".{e}")),
            _ => (rel.to_string(), String::new()),
        };
        let stamp = chrono::Local::now().format("%Y-%m-%d %H%M");
        format!("{base} (conflict {} {stamp}){ext}", self.device)
    }

    // ------------------------------------------------------------------ connection

    pub fn on_connected(&mut self, tx: mpsc::UnboundedSender<Vec<u8>>) {
        let mut enc = EncoderV1::new();
        if let Err(e) = DefaultProtocol.start(&self.awareness, &mut enc) {
            tracing::warn!("protocol start failed: {e}");
        }
        let _ = tx.send(enc.to_vec());
        *self.conn.lock() = Some(tx);
        tracing::info!("connected");
    }

    /// Ask the server which mounts in this vault are read-only, so edits under them are not
    /// pushed (the server would revert them anyway) and the files are marked read-only on disk.
    pub async fn refresh_sharing(&mut self) {
        let url = self.args.sharing_url();
        let res = match self.http.get(&url).bearer_auth(self.args.bearer()).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("sharing info unavailable: {e}");
                return;
            }
        };
        let Ok(body) = res.bytes().await else { return };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&body) else { return };
        let rw_list = |m: &serde_json::Value, prefix: &str| -> Vec<String> {
            m["rw_paths"].as_array().into_iter().flatten().filter_map(|p| p.as_str()).map(|p| format!("{prefix}{p}")).collect()
        };
        let mut ro: Vec<RoArea> = if self.args.is_guest() {
            // Guest: the whole directory is the share; read-only means all of it (minus the
            // ticket's writable paths).
            if v["access"].as_str() == Some("ro") { vec![RoArea { prefix: String::new(), rw: rw_list(&v, "") }] } else { vec![] }
        } else { v["mounts"]
            .as_array()
            .map(|ms| {
                ms.iter()
                    .filter(|m| m["access"].as_str() == Some("ro") && m["status"].as_str() != Some("revoked"))
                    .filter_map(|m| {
                        let prefix = format!("{}/", m["mount_path"].as_str()?.trim_end_matches('/'));
                        Some(RoArea { rw: rw_list(m, &prefix), prefix })
                    })
                    .collect()
            })
            .unwrap_or_default() };
        ro.sort();
        if !self.args.is_guest() {
            let mut roots: Vec<SharedRoot> = Vec::new();
            for s in v["shares"].as_array().into_iter().flatten() {
                if let Some(p) = s["path"].as_str() {
                    let is_file = !s["only_file"].is_null();
                    roots.push(SharedRoot {
                        path: if is_file { p.to_string() } else { format!("{}/", p.trim_end_matches('/')) },
                        is_file,
                        name: s["name"].as_str().unwrap_or(p).to_string(),
                    });
                }
            }
            for m in v["mounts"].as_array().into_iter().flatten() {
                if m["status"].as_str() == Some("revoked") {
                    continue;
                }
                if let Some(p) = m["mount_path"].as_str() {
                    roots.push(SharedRoot { path: format!("{}/", p.trim_end_matches('/')), is_file: false, name: m["name"].as_str().unwrap_or(p).to_string() });
                }
            }
            if roots.iter().map(|r| &r.path).ne(self.shared_roots.iter().map(|r| &r.path)) {
                let list: Vec<&str> = roots.iter().map(|r| if r.path.is_empty() { "(whole vault)" } else { r.path.as_str() }).collect();
                tracing::info!("shared with other people here: {list:?} (moving these on disk is undone)");
            }
            self.shared_roots = roots;
        }
        if ro == self.ro_areas {
            return;
        }
        let was = std::mem::replace(&mut self.ro_areas, ro);
        for a in &self.ro_areas {
            let except = if a.rw.is_empty() { String::new() } else { format!(" (except {})", a.rw.join(", ")) };
            if a.prefix.is_empty() {
                tracing::info!("this share is read-only{except}: local edits are not synced and are restored from the server");
            } else {
                tracing::info!("{} is a read-only share{except}: edits there are not synced and are restored from the server", a.prefix);
            }
        }
        // Flip permissions on disk for every area that was or is read-only; each file follows
        // its own effective state, so writable paths inside a read-only mount stay writable.
        let mut prefixes: Vec<String> = was.iter().chain(self.ro_areas.iter()).map(|a| a.prefix.clone()).collect();
        prefixes.sort();
        prefixes.dedup();
        for p in prefixes {
            for e in walkdir::WalkDir::new(self.abs(p.trim_end_matches('/'))).into_iter().flatten() {
                if e.file_type().is_file() {
                    if let Some(rel) = self.rel(e.path()) {
                        if self.should_sync(&rel) {
                            set_readonly(e.path(), self.is_ro(&rel));
                        }
                    }
                }
            }
        }
    }

    /// Where files created under a read-only prefix are moved aside.
    fn rescue_root(prefix: &str) -> String {
        if prefix.is_empty() {
            "read-only copy/".to_string()
        } else {
            format!("{} (read-only copy)/", prefix.trim_end_matches('/'))
        }
    }

    /// The innermost shared root containing `rel` (shares may nest; a whole-vault share is "").
    fn shared_root_of(&self, rel: &str) -> Option<&SharedRoot> {
        self.shared_roots
            .iter()
            .filter(|r| if r.is_file { rel == r.path } else { rel.starts_with(r.path.as_str()) })
            .max_by_key(|r| r.path.len())
    }

    fn keys_under(&self, root: &SharedRoot) -> Vec<String> {
        let (t, b) = self.doc_keys();
        t.into_iter().chain(b).filter(|k| if root.is_file { *k == root.path } else { k.starts_with(root.path.as_str()) }).collect()
    }

    /// Does the file at `abs` hold what the document has under `key`?
    fn matches_doc(&self, key: &str, abs: &Path) -> bool {
        if let Some(t) = self.text_of(key) {
            return std::fs::read_to_string(abs).map(|s| s == t).unwrap_or(false);
        }
        if let Some(b) = self.blob_of(key) {
            return std::fs::read(abs).map(|d| hash_bytes(&d) == b.hash).unwrap_or(false);
        }
        false
    }

    /// Shared folders must not be moved on disk (the other side would see a deletion), and files
    /// must not cross a share boundary. Both are undone here. Returns the paths this batch must
    /// skip, plus `true` when the whole batch is to be ignored (a root was moved or vanished).
    fn guard_shared_moves(&mut self, rels: &[(String, PathBuf)]) -> (bool, HashSet<String>) {
        let mut skip: HashSet<String> = HashSet::new();
        if self.shared_roots.is_empty() {
            return (false, skip);
        }
        // A. A shared root (folder or note) is gone from where it belongs.
        for root in self.shared_roots.clone() {
            let root_abs = self.abs(root.path.trim_end_matches('/'));
            let gone = if root.is_file { !root_abs.is_file() } else { !root_abs.is_dir() };
            if !gone {
                continue;
            }
            let keys = self.keys_under(&root);
            if keys.is_empty() {
                continue;
            }
            let probe = if root.is_file { String::new() } else { keys[0][root.path.len()..].to_string() };
            let dest = rels.iter().find(|(_, abs)| {
                abs.exists()
                    && if root.is_file { self.matches_doc(&root.path, abs) } else { abs.is_dir() && abs.join(&probe).exists() }
            });
            match dest {
                Some((drel, dabs)) => {
                    if let Some(parent) = root_abs.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    match std::fs::rename(dabs, &root_abs) {
                        Ok(()) => tracing::warn!(
                            "\"{}\" ({}) is shared with other people and must not be moved; moved it back from {drel}. Use the plugin's Sharing settings (Move…) to relocate a mount.",
                            root.name,
                            root.path.trim_end_matches('/')
                        ),
                        Err(e) => tracing::error!("\"{}\" was moved to {drel} and moving it back failed: {e}. Put it back by hand.", root.name),
                    }
                    return (true, skip);
                }
                None if !self.args.allow_mass_delete => {
                    tracing::warn!(
                        "\"{}\" ({}) is shared with other people and disappeared from disk; not deleting it for everyone. Put it back (it is restored on the next reconcile), or restart with --allow-mass-delete if you really want it removed for everyone.",
                        root.name,
                        root.path.trim_end_matches('/')
                    );
                    for k in keys {
                        if !self.pending_deletions.contains(&k) {
                            self.pending_deletions.push(k);
                        }
                    }
                    return (true, skip);
                }
                None => {}
            }
        }
        // B. A file left or entered a shared area: the same content vanished on one side of the
        //    boundary and appeared on the other.
        let removed: Vec<&(String, PathBuf)> = rels.iter().filter(|(r, abs)| !abs.exists() && self.should_sync(r) && (self.text_of(r).is_some() || self.blob_of(r).is_some())).collect();
        let created: Vec<&(String, PathBuf)> = rels.iter().filter(|(r, abs)| abs.is_file() && self.should_sync(r) && !self.is_rescue_path(r)).collect();
        for (r, r_abs) in removed {
            let from = self.shared_root_of(r).map(|x| x.path.clone());
            for (c, c_abs) in &created {
                let to = self.shared_root_of(c).map(|x| x.path.clone());
                if from == to || !self.matches_doc(r, c_abs) {
                    continue;
                }
                if let Some(parent) = r_abs.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let why = match (&from, &to) {
                    (Some(f), None) => format!("out of the shared folder {f}; it would disappear for the people you share with"),
                    (None, Some(t)) => format!("into the shared folder {t}; it would be shared with other people"),
                    (Some(f), Some(t)) => format!("from the shared folder {f} to {t}; both are shared with other people"),
                    (None, None) => continue,
                };
                match std::fs::rename(c_abs, r_abs) {
                    Ok(()) => tracing::warn!("{r} was moved {why}. Moved it back to {r} (copy it instead)."),
                    Err(e) => tracing::error!("{r} was moved {why} and moving it back failed: {e}"),
                }
                skip.insert(r.clone());
                skip.insert(c.clone());
                break;
            }
        }
        (false, skip)
    }

    fn is_rescue_path(&self, rel: &str) -> bool {
        self.ro_areas.iter().any(|a| rel.starts_with(Self::rescue_root(&a.prefix).as_str()))
    }

    /// Inside a read-only mount, outside its rescue folder and outside the ticket's writable paths.
    fn is_ro(&self, rel: &str) -> bool {
        self.ro_areas.iter().any(|a| {
            rel.starts_with(a.prefix.as_str())
                && !rel.starts_with(Self::rescue_root(&a.prefix).as_str())
                && !a.rw.iter().any(|p| if p.ends_with('/') { rel.starts_with(p.as_str()) } else { rel == p })
        })
    }

    fn protect_if_ro(&self, rel: &str) {
        if self.is_ro(rel) {
            set_readonly(&self.abs(rel), true);
        }
    }

    /// A local change under a read-only mount: put the server's version back. A file the
    /// share does not have is moved to `<mount> (read-only copy)/…`, like the server does for
    /// plugin clients, so nothing the user wrote is lost.
    async fn undo_ro_edit(&mut self, rel: &str) {
        let prefix = self.ro_areas.iter().find(|a| rel.starts_with(a.prefix.as_str())).map(|a| a.prefix.clone()).unwrap_or_default();
        let in_doc = {
            let txn = self.doc().transact();
            self.texts.contains_key(&txn, rel) || self.blobs.contains_key(&txn, rel)
        };
        let abs = self.abs(rel);
        if in_doc {
            // Plain overwrite, no conflict copy: the edit was never allowed in the first place.
            tracing::warn!("{rel} is in a read-only share; restoring the server's version");
            let res = if let Some(content) = self.text_of(rel) {
                self.write_text(rel, &content)
            } else if let Some(meta) = self.blob_of(rel) {
                self.state.synced.remove(rel);
                self.download_blob(rel, &meta).await
            } else {
                Ok(())
            };
            if let Err(e) = res {
                tracing::warn!("restoring {rel} failed: {e}");
            }
        } else if abs.is_file() {
            let rest = rel.strip_prefix(prefix.as_str()).unwrap_or(rel);
            let rescue = format!("{}{rest}", Self::rescue_root(&prefix));
            let dst = self.abs(&rescue);
            tracing::warn!("{rel} is in a read-only share; keeping it as {rescue}");
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::rename(&abs, &dst) {
                tracing::warn!("moving {rel} aside failed: {e}");
            }
        }
    }

    pub fn on_disconnected(&mut self) {
        *self.conn.lock() = None;
        self.synced = false;
    }

    pub async fn on_message(&mut self, data: &[u8]) {
        let mut decoder = DecoderV1::from(data);
        let reader = MessageReader::new(&mut decoder);
        let mut replies = Vec::new();
        let mut got_step2 = false;
        for msg in reader {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("bad message from server: {e}");
                    break;
                }
            };
            if matches!(msg, Message::Sync(SyncMessage::SyncStep2(_))) {
                got_step2 = true;
            }
            match DefaultProtocol.handle_message(&mut self.awareness, msg) {
                Ok(Some(reply)) => replies.push(encode(&reply)),
                Ok(None) => {}
                Err(e) => tracing::warn!("protocol error: {e}"),
            }
        }
        if let Some(tx) = self.conn.lock().as_ref() {
            for r in replies {
                let _ = tx.send(r);
            }
        }
        if got_step2 && !self.synced {
            self.synced = true;
            tracing::info!("initial sync received");
        }
        self.flush_remote().await;
    }

    // ------------------------------------------------------------------ remote -> disk

    async fn flush_remote(&mut self) {
        let paths: Vec<String> = {
            let mut d = self.dirty_remote.lock();
            d.drain().collect()
        };
        for rel in paths {
            if !self.should_sync(&rel) {
                continue;
            }
            if let Err(e) = self.apply_remote(&rel).await {
                tracing::warn!("applying remote change to {rel} failed: {e}");
            }
        }
    }

    async fn apply_remote(&mut self, rel: &str) -> anyhow::Result<()> {
        if let Some(content) = self.text_of(rel) {
            let abs = self.abs(rel);
            if let Ok(local) = std::fs::read_to_string(&abs) {
                if local == content {
                    self.mark_synced(rel, hash_bytes(content.as_bytes()));
                    self.base.insert(rel.to_string(), content);
                    return Ok(());
                }
                match self.base.get(rel).cloned() {
                    Some(base) if base != local => {
                        // Local has an unpushed edit. Merge it against the remote change.
                        return self.merge_local_edit(rel, &base, &local, &content);
                    }
                    Some(_) => {}
                    None => {
                        let local_hash = hash_bytes(local.as_bytes());
                        if self.state.synced.get(rel).map(|h| h != &local_hash).unwrap_or(true) {
                            self.write_conflict_copy(rel, local.as_bytes())?;
                        }
                    }
                }
            }
            self.write_text(rel, &content)?;
            return Ok(());
        }
        if let Some(meta) = self.blob_of(rel) {
            return self.download_blob(rel, &meta).await;
        }
        // Gone from the document: move the local file to .samsyn/trash.
        self.trash_local(rel)?;
        Ok(())
    }

    /// Three-way merge of a pending local edit (base -> local) with a remote change
    /// (base -> remote) that has already been applied to the document. Non-overlapping spans
    /// are merged by replaying the local span into the CRDT; overlapping ones keep a copy.
    fn merge_local_edit(&mut self, rel: &str, base: &str, local: &str, remote: &str) -> anyhow::Result<()> {
        let Some(dl) = diff_text(base, local) else {
            return self.write_text(rel, remote);
        };
        let Some(dr) = diff_text(base, remote) else {
            // Remote did not change the text after all; just push ours.
            self.push_text(rel, local);
            return Ok(());
        };
        let local_end = dl.index + dl.remove;
        let remote_end = dr.index + dr.remove;
        let disjoint = local_end <= dr.index || remote_end <= dl.index;
        // Pure insertions at the same point are not a conflict either; order remote first.
        let same_point_inserts = dl.remove == 0 && dr.remove == 0 && dl.index == dr.index;
        if !disjoint && !same_point_inserts {
            tracing::warn!("overlapping concurrent edits on {rel}; keeping local copy");
            self.write_conflict_copy(rel, local.as_bytes())?;
            return self.write_text(rel, remote);
        }
        let shift: isize = if dr.index <= dl.index {
            dr.insert.len() as isize - dr.remove as isize
        } else {
            0
        };
        let index = (dl.index as isize + shift) as usize;
        let merged = {
            let doc = self.doc().clone();
            let mut txn = doc.transact_mut_with(LOCAL);
            if let Some(Out::YText(t)) = self.texts.get(&txn, rel) {
                if dl.remove > 0 {
                    t.remove_range(&mut txn, index as u32, dl.remove as u32);
                }
                if !dl.insert.is_empty() {
                    t.insert(&mut txn, index as u32, &dl.insert);
                }
                t.get_string(&txn)
            } else {
                remote.to_string()
            }
        };
        tracing::info!("merged concurrent edits on {rel}");
        self.write_text(rel, &merged)
    }

    fn write_text(&mut self, rel: &str, content: &str) -> anyhow::Result<()> {
        self.mark_synced(rel, hash_bytes(content.as_bytes()));
        self.base.insert(rel.to_string(), content.to_string());
        write_atomic(&self.abs(rel), content.as_bytes())?;
        self.protect_if_ro(rel);
        tracing::info!("wrote {rel}");
        Ok(())
    }

    fn write_conflict_copy(&self, rel: &str, data: &[u8]) -> anyhow::Result<()> {
        let cp = self.conflict_path(rel);
        write_atomic(&self.abs(&cp), data)?;
        tracing::warn!("conflict on {rel}; local version kept as {cp}");
        Ok(())
    }

    fn trash_local(&mut self, rel: &str) -> anyhow::Result<()> {
        self.forget_synced(rel);
        self.base.remove(rel);
        let abs = self.abs(rel);
        if !abs.exists() {
            return Ok(());
        }
        let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
        let dest = self.store.trash_dir().join(stamp.to_string()).join(rel);
        std::fs::create_dir_all(dest.parent().unwrap())?;
        std::fs::rename(&abs, &dest)?;
        tracing::info!("removed {rel} (kept in {})", dest.display());
        if let Some(parent) = abs.parent() {
            self.prune_empty_dirs(parent.to_path_buf());
        }
        Ok(())
    }

    /// A remote delete emptied a folder (typically a folder renamed on another device, which
    /// arrives as delete + create): drop it and any parent it leaves empty, stopping at the sync
    /// root and at shared roots, which must stay where they are.
    fn prune_empty_dirs(&self, mut dir: PathBuf) {
        let keep: Vec<PathBuf> =
            self.shared_roots.iter().filter(|r| !r.is_file).map(|r| self.abs(r.path.trim_end_matches('/'))).collect();
        while dir != self.root && dir.starts_with(&self.root) {
            if keep.contains(&dir) || std::fs::remove_dir(&dir).is_err() {
                return; // a shared root, not empty, or already gone
            }
            match dir.parent() {
                Some(p) => dir = p.to_path_buf(),
                None => return,
            }
        }
    }

    async fn download_blob(&mut self, rel: &str, meta: &BlobMeta) -> anyhow::Result<()> {
        let abs = self.abs(rel);
        if let Ok(local) = std::fs::read(&abs) {
            let local_hash = hash_bytes(&local);
            if local_hash == meta.hash {
                self.mark_synced(rel, local_hash);
                self.pending_blobs.remove(rel);
                return Ok(());
            }
            if self.state.synced.get(rel).map(|h| h != &local_hash).unwrap_or(false) {
                self.write_conflict_copy(rel, &local)?;
            }
        }
        let url = self.args.blob_url(&meta.hash);
        let res = self.http.get(&url).bearer_auth(self.args.bearer()).send().await?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            // Metadata can precede the bytes (slow uploader, or a server-side share link still
            // fetching them); retry with backoff for up to about ten minutes.
            let attempts = self.pending_blobs.get(rel).map(|(_, _, n)| *n).unwrap_or(0) + 1;
            if attempts > 20 {
                tracing::warn!("blob for {rel} never appeared on server, giving up");
                self.pending_blobs.remove(rel);
                return Ok(());
            }
            let delay = Duration::from_secs(match attempts { 1 => 2, 2 => 5, 3 => 15, 4 => 30, _ => 60 });
            tracing::warn!("blob for {rel} not on server yet, retrying in {delay:?}");
            self.pending_blobs.insert(rel.to_string(), (meta.clone(), Instant::now() + delay, attempts));
            return Ok(());
        }
        self.pending_blobs.remove(rel);
        let bytes = res.error_for_status()?.bytes().await?;
        self.mark_synced(rel, meta.hash.clone());
        write_atomic(&abs, &bytes)?;
        self.protect_if_ro(rel);
        tracing::info!("downloaded {rel} ({} bytes)", bytes.len());
        Ok(())
    }

    // ------------------------------------------------------------------ disk -> doc

    /// Watch the directory and hand over batches of paths that have been quiet for
    /// `--debounce-ms`. Access events (open, read, close without write) are dropped before
    /// debouncing: on Linux inotify reports every open, including our own hash checks and
    /// other programs merely reading the files, and treating those as changes made the client
    /// re-hash the whole directory in an endless loop.
    pub fn start_watcher(&self, tx: mpsc::UnboundedSender<Vec<PathBuf>>) -> anyhow::Result<RecommendedWatcher> {
        let debounce = Duration::from_millis(self.args.debounce_ms);
        let (raw_tx, raw_rx) = std::sync::mpsc::channel::<Vec<PathBuf>>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if matches!(ev.kind, EventKind::Access(_)) || ev.paths.is_empty() {
                    return;
                }
                let _ = raw_tx.send(ev.paths);
            }
        })
        .context("starting file watcher")?;
        std::thread::Builder::new()
            .name("samsyn fs debounce".to_string())
            .spawn(move || {
                let mut pending: HashMap<PathBuf, Instant> = HashMap::new();
                loop {
                    let wait = pending
                        .values()
                        .min()
                        .map(|t| (*t + debounce).saturating_duration_since(Instant::now()))
                        .unwrap_or(Duration::from_secs(3600));
                    match raw_rx.recv_timeout(wait) {
                        Ok(paths) => {
                            let now = Instant::now();
                            for p in paths {
                                pending.insert(p, now);
                            }
                        }
                        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                    }
                    let now = Instant::now();
                    let due: Vec<PathBuf> = pending.iter().filter(|(_, t)| now.duration_since(**t) >= debounce).map(|(p, _)| p.clone()).collect();
                    if !due.is_empty() {
                        for p in &due {
                            pending.remove(p);
                        }
                        if tx.send(due).is_err() {
                            return;
                        }
                    }
                }
            })
            .context("starting debounce thread")?;
        watcher.watch(&self.root, RecursiveMode::Recursive).context("starting file watcher")?;
        Ok(watcher)
    }

    /// Handle one debounced batch of filesystem events. Removals are counted across the whole
    /// batch (and a rolling window) so deleting a folder cannot sneak past the mass-delete guard
    /// as many single-file deletions. Returns true if anything syncable was touched.
    pub async fn on_fs_batch(&mut self, paths: &[PathBuf]) -> bool {
        if !self.root.is_dir() {
            if !self.root_missing {
                self.root_missing = true;
                tracing::error!(
                    "sync directory {} disappeared; ignoring all local events until it is back",
                    self.root.display()
                );
            }
            return false;
        }
        self.root_missing = false;

        let rels: Vec<(String, PathBuf)> = paths.iter().filter_map(|p| self.rel(p).map(|r| (r, p.clone()))).collect();
        let (skip_all, skip) = self.guard_shared_moves(&rels);
        if skip_all {
            return true;
        }

        let mut touched = false;
        let mut removed: Vec<String> = Vec::new();
        let mut changed: Vec<String> = Vec::new();
        let mut ro_touched: Vec<String> = Vec::new();
        let mut seen = HashSet::new();
        for abs in paths {
            let Some(rel) = self.rel(abs) else { continue };
            if !self.should_sync(&rel) || skip.contains(&rel) || !seen.insert(rel.clone()) {
                continue;
            }
            touched = true;
            if self.is_ro(&rel) {
                // Our own restores, downloads and chmods come back as watcher events. Only a
                // real divergence from the document is an edit to undo; restoring a file that
                // already matches would fire another event and loop forever.
                if !self.matches_doc(&rel, abs) {
                    ro_touched.push(rel);
                }
                continue;
            }
            if abs.exists() {
                changed.push(rel);
            } else {
                removed.push(rel);
            }
        }

        if !removed.is_empty() {
            let (t, b) = self.doc_keys();
            let total = t.len() + b.len();
            let mut gone: Vec<String> = Vec::new();
            for k in t.into_iter().chain(b) {
                if removed.iter().any(|r| &k == r || k.starts_with(&format!("{r}/"))) {
                    gone.push(k);
                }
            }
            if !gone.is_empty() {
                let now = Instant::now();
                while self.recent_deletes.front().map(|(t, _)| now.duration_since(*t) > Duration::from_secs(30)).unwrap_or(false) {
                    self.recent_deletes.pop_front();
                }
                let recent: usize = self.recent_deletes.iter().map(|(_, n)| n).sum();
                let n = gone.len() + recent;
                let mass = n >= MASS_DELETE_MIN && (n as f64) > ((total + recent) as f64) * MASS_DELETE_RATIO;
                if mass && !self.args.allow_mass_delete {
                    tracing::warn!(
                        "{} files removed locally would delete {n} of {} shared files; held back.                          Restart with --allow-mass-delete if intended; otherwise they are restored on next reconcile.",
                        gone.len(),
                        total + recent
                    );
                    self.pending_deletions.extend(gone);
                } else {
                    self.recent_deletes.push_back((now, gone.len()));
                    for k in gone {
                        self.remove_from_doc(&k);
                    }
                }
            }
        }

        for rel in changed {
            if let Err(e) = self.push_local(&rel).await {
                tracing::warn!("pushing {rel} failed: {e}");
            }
        }
        for rel in ro_touched {
            self.undo_ro_edit(&rel).await;
        }
        touched
    }

    /// Make the document reflect what is on disk at `rel` (file changed, created or removed).
    async fn push_local(&mut self, rel: &str) -> anyhow::Result<()> {
        let abs = self.abs(rel);
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(_) => {
                // Removed: drop the key and anything below it if it was a directory.
                let (t, b) = self.doc_keys();
                let total = t.len() + b.len();
                let prefix = format!("{rel}/");
                let gone: Vec<String> = t.into_iter().chain(b).filter(|k| k == rel || k.starts_with(&prefix)).collect();
                let mass = gone.len() >= MASS_DELETE_MIN && (gone.len() as f64) > (total as f64) * MASS_DELETE_RATIO;
                if mass && !self.args.allow_mass_delete {
                    tracing::warn!(
                        "{rel} removal would delete {} of {total} shared files; held back.                          Restart with --allow-mass-delete if intended; otherwise the files are restored on next reconcile.",
                        gone.len()
                    );
                    self.pending_deletions = gone;
                    return Ok(());
                }
                for k in gone {
                    self.remove_from_doc(&k);
                }
                return Ok(());
            }
        };
        if meta.is_dir() {
            // New directory (e.g. moved in): push its contents.
            for entry in walkdir::WalkDir::new(&abs).into_iter().filter_map(|e| e.ok()) {
                if entry.file_type().is_file() {
                    if let Some(r) = self.rel(entry.path()) {
                        if self.should_sync(&r) {
                            Box::pin(self.push_local(&r)).await?;
                        }
                    }
                }
            }
            return Ok(());
        }
        if self.is_text(rel) {
            let bytes = std::fs::read(&abs)?;
            let content = match String::from_utf8(bytes) {
                Ok(s) => s,
                Err(_) => {
                    tracing::warn!("{rel} is not valid UTF-8; skipping");
                    return Ok(());
                }
            };
            self.push_text(rel, &content);
        } else {
            self.push_blob(rel).await?;
        }
        Ok(())
    }

    fn push_text(&mut self, rel: &str, content: &str) {
        let doc = self.doc().clone();
        let mut txn = doc.transact_mut_with(LOCAL);
        match self.texts.get(&txn, rel) {
            Some(Out::YText(t)) => {
                let current = t.get_string(&txn);
                if let Some(d) = diff_text(&current, content) {
                    if d.remove > 0 {
                        t.remove_range(&mut txn, d.index as u32, d.remove as u32);
                    }
                    if !d.insert.is_empty() {
                        t.insert(&mut txn, d.index as u32, &d.insert);
                    }
                    tracing::info!("pushed {rel} (-{} +{} bytes)", d.remove, d.insert.len());
                }
            }
            _ => {
                self.texts.insert(&mut txn, rel, TextPrelim::new(content));
                tracing::info!("added {rel}");
            }
        }
        drop(txn);
        self.mark_synced(rel, hash_bytes(content.as_bytes()));
        self.base.insert(rel.to_string(), content.to_string());
    }

    async fn push_blob(&mut self, rel: &str) -> anyhow::Result<()> {
        let abs = self.abs(rel);
        let data = std::fs::read(&abs)?;
        let hash = hash_bytes(&data);
        if self.blob_of(rel).map(|m| m.hash == hash).unwrap_or(false) {
            self.mark_synced(rel, hash);
            return Ok(());
        }
        let url = self.args.blob_url(&hash);
        let size = data.len();
        let res = self.http.put(&url).bearer_auth(self.args.bearer()).body(data).send().await?;
        if !res.status().is_success() {
            anyhow::bail!("upload failed: HTTP {}", res.status());
        }
        let mtime = std::fs::metadata(&abs)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as f64)
            .unwrap_or(0.0);
        let meta = BlobMeta { hash: hash.clone(), size, mtime };
        {
            let doc = self.doc().clone();
            let mut txn = doc.transact_mut_with(LOCAL);
            self.blobs.insert(&mut txn, rel, meta.to_any());
        }
        self.mark_synced(rel, hash);
        tracing::info!("uploaded {rel} ({size} bytes)");
        Ok(())
    }

    fn remove_from_doc(&mut self, rel: &str) {
        let doc = self.doc().clone();
        let mut txn = doc.transact_mut_with(LOCAL);
        let a = self.texts.remove(&mut txn, rel).is_some();
        let b = self.blobs.remove(&mut txn, rel).is_some();
        drop(txn);
        if a || b {
            tracing::info!("deleted {rel} from shared vault");
        }
        self.forget_synced(rel);
        self.base.remove(rel);
    }

    // ------------------------------------------------------------------ reconcile

    pub async fn reconcile(&mut self) -> anyhow::Result<()> {
        let mut local: HashMap<String, PathBuf> = HashMap::new();
        for entry in walkdir::WalkDir::new(&self.root)
            .into_iter()
            .filter_entry(|e| e.depth() == 0 || !e.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|e| e.ok())
        {
            if !entry.file_type().is_file() {
                continue;
            }
            if let Some(rel) = self.rel(entry.path()) {
                if self.should_sync(&rel) {
                    local.insert(rel, entry.path().to_path_buf());
                }
            }
        }

        for rel in local.keys().cloned().collect::<Vec<_>>() {
            let r = if self.is_text(&rel) { self.reconcile_text(&rel).await } else { self.reconcile_blob(&rel).await };
            if let Err(e) = r {
                tracing::error!("reconcile failed for {rel}: {e}");
            }
        }

        let (t_keys, b_keys) = self.doc_keys();
        let mut to_delete = Vec::new();
        for rel in t_keys.iter().chain(b_keys.iter()) {
            if local.contains_key(rel) || !self.should_sync(rel) {
                continue;
            }
            if self.state.synced.contains_key(rel) {
                to_delete.push(rel.clone());
            } else if let Some(c) = self.text_of(rel) {
                self.write_text(rel, &c)?;
            } else if let Some(m) = self.blob_of(rel) {
                self.download_blob(rel, &m).await?;
            }
        }

        let total = t_keys.len() + b_keys.len();
        let mass = to_delete.len() >= MASS_DELETE_MIN && (to_delete.len() as f64) > (total as f64) * MASS_DELETE_RATIO;
        if mass && !self.args.allow_mass_delete {
            tracing::warn!(
                "refusing to delete {} of {total} shared files from this directory; re-downloading them instead. \
                 Re-run with --allow-mass-delete if the removal was intended.",
                to_delete.len()
            );
            for rel in &to_delete {
                self.forget_synced(rel);
                if let Some(c) = self.text_of(rel) {
                    self.write_text(rel, &c)?;
                } else if let Some(m) = self.blob_of(rel) {
                    self.download_blob(rel, &m).await?;
                }
            }
            self.pending_deletions = to_delete;
        } else {
            for rel in to_delete {
                self.remove_from_doc(&rel);
            }
        }
        self.store.save_state(&self.state)?;
        self.state_dirty = false;
        tracing::info!("reconcile done: {} local files, {} shared", local.len(), total);
        Ok(())
    }

    async fn reconcile_text(&mut self, rel: &str) -> anyhow::Result<()> {
        let bytes = std::fs::read(self.abs(rel))?;
        let local = match String::from_utf8(bytes) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        let local_hash = hash_bytes(local.as_bytes());
        let last = self.state.synced.get(rel).cloned();
        let Some(remote) = self.text_of(rel) else {
            if last.is_some() {
                self.trash_local(rel)?;
            } else {
                self.push_text(rel, &local);
            }
            return Ok(());
        };
        if remote == local {
            self.mark_synced(rel, local_hash);
            self.base.insert(rel.to_string(), local);
        } else if last.as_deref() == Some(local_hash.as_str()) {
            self.write_text(rel, &remote)?;
        } else if last.as_deref() == Some(hash_bytes(remote.as_bytes()).as_str()) {
            self.push_text(rel, &local);
        } else {
            self.write_conflict_copy(rel, local.as_bytes())?;
            self.write_text(rel, &remote)?;
        }
        Ok(())
    }

    async fn reconcile_blob(&mut self, rel: &str) -> anyhow::Result<()> {
        let abs = self.abs(rel);
        let last = self.state.synced.get(rel).cloned();
        let Some(meta) = self.blob_of(rel) else {
            if last.is_some() {
                self.trash_local(rel)?;
            } else {
                self.push_blob(rel).await?;
            }
            return Ok(());
        };
        let data = std::fs::read(&abs)?;
        let local_hash = hash_bytes(&data);
        if local_hash == meta.hash {
            self.mark_synced(rel, local_hash);
        } else if last.as_deref() == Some(local_hash.as_str()) {
            self.download_blob(rel, &meta).await?;
        } else if last.as_deref() == Some(meta.hash.as_str()) {
            self.push_blob(rel).await?;
        } else {
            let mtime = std::fs::metadata(&abs)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as f64)
                .unwrap_or(0.0);
            if mtime > meta.mtime {
                self.push_blob(rel).await?;
            } else {
                self.write_conflict_copy(rel, &data)?;
                self.download_blob(rel, &meta).await?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ persistence

    /// Retry blobs that were missing on the server when their metadata arrived.
    pub async fn retry_pending_blobs(&mut self) {
        let now = Instant::now();
        let due: Vec<(String, BlobMeta)> = self
            .pending_blobs
            .iter()
            .filter(|(_, (_, at, _))| *at <= now)
            .map(|(rel, (meta, _, _))| (rel.clone(), meta.clone()))
            .collect();
        for (rel, meta) in due {
            let current = {
                let txn = self.awareness.doc().transact();
                match self.blobs.get(&txn, &rel) {
                    Some(Out::Any(Any::Map(m))) => BlobMeta::from_any(&m),
                    _ => None,
                }
            };
            match current {
                Some(m) if m.hash == meta.hash => {
                    if let Err(e) = self.download_blob(&rel, &meta).await {
                        tracing::warn!("retrying blob {rel} failed: {e}");
                    }
                }
                _ => {
                    // Replaced or removed meanwhile; nothing to fetch.
                    self.pending_blobs.remove(&rel);
                }
            }
        }
    }

    pub fn flush_persistence(&mut self) {
        let dirty = std::mem::replace(&mut *self.doc_dirty.lock(), false);
        if dirty {
            if let Err(e) = self.store.save_doc(self.doc()) {
                tracing::warn!("saving doc cache failed: {e}");
            }
        }
        if self.state_dirty {
            match self.store.save_state(&self.state) {
                Ok(()) => self.state_dirty = false,
                Err(e) => tracing::warn!("saving state failed: {e}"),
            }
        }
    }

    pub fn shutdown(&mut self) {
        *self.doc_dirty.lock() = true;
        self.flush_persistence();
    }
}

fn is_local(origin: Option<&Origin>) -> bool {
    origin.map(|o| o == &Origin::from(LOCAL)).unwrap_or(false)
}

fn encode(msg: &Message) -> Vec<u8> {
    let mut enc = EncoderV1::new();
    msg.encode(&mut enc);
    enc.to_vec()
}

#[derive(Debug, Clone)]
struct BlobMeta {
    hash: String,
    size: usize,
    mtime: f64,
}

impl BlobMeta {
    fn from_any(m: &HashMap<String, Any>) -> Option<Self> {
        let hash = match m.get("hash") {
            Some(Any::String(s)) => s.to_string(),
            _ => return None,
        };
        let num = |k: &str| match m.get(k) {
            Some(Any::Number(n)) => n.as_f64().unwrap_or(0.0),
            _ => 0.0,
        };
        let size = num("size") as usize;
        let mtime = num("mtime");
        Some(Self { hash, size, mtime })
    }

    fn to_any(&self) -> Any {
        let mut m = HashMap::new();
        m.insert("hash".to_string(), Any::String(self.hash.clone().into()));
        m.insert("size".to_string(), Any::Number(yrs::Number::from(self.size as f64)));
        m.insert("mtime".to_string(), Any::Number(yrs::Number::from(self.mtime)));
        Any::Map(Arc::new(m))
    }
}

fn set_readonly(path: &std::path::Path, ro: bool) {
    if let Ok(meta) = std::fs::metadata(path) {
        let mut perm = meta.permissions();
        perm.set_readonly(ro);
        let _ = std::fs::set_permissions(path, perm);
    }
}

/// A read-only mount: its prefix ("" for a guest's whole directory) and the vault-relative paths
/// inside it that the ticket still lets us write (folders end with '/').
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RoArea {
    prefix: String,
    rw: Vec<String>,
}

/// A folder (prefix with trailing slash) or single note shared with or by other people.
#[derive(Debug, Clone)]
struct SharedRoot {
    path: String,
    is_file: bool,
    name: String,
}
