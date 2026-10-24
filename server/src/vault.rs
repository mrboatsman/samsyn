//! Live documents (vaults and shares) with persistence, fan-out broadcasting, and the
//! bridges that keep shared subtrees equal between documents.

use std::collections::HashMap;
use std::sync::Arc;
use parking_lot::Mutex;

use bytes::Bytes;
use tokio::sync::broadcast;
use yrs::sync::{Awareness, Message, SyncMessage};
use yrs::types::{EntryChange, Event, PathSegment};
use yrs::updates::encoder::{Encode, Encoder, EncoderV1};
use yrs::{Any, DeepObservable, GetString, Map, Observable, Origin, Out, ReadTxn, StateVector, Transact, WriteTxn};

use crate::access::Access;
use crate::bridge::{Bridge, Change, ChangeKind, OwnedDelta, Side};
use crate::presence::{PresenceEvent, PresenceSender};
use crate::storage::{MountInfo, ShareInfo, Storage, User, VaultInfo, VaultStore};

const BROADCAST_CAPACITY: usize = 4096;
pub const BRIDGE_ORIGIN_PREFIX: &[u8] = b"bridge:";

pub enum DocKind {
    Vault(VaultInfo),
    Share(ShareInfo),
    /// Local replica of a share owned by another server (one per remote mount).
    Remote(MountInfo),
}

pub struct LiveDoc {
    pub id: String,
    pub label: String,
    pub kind: DocKind,
    pub store: VaultStore,
    pub awareness: Mutex<Awareness>,
    pub tx: broadcast::Sender<Bytes>,
    /// Changes recorded by observers, drained by `DocRegistry::propagate`.
    pub changes: Arc<Mutex<Vec<Change>>>,
    /// Serializes quota check + blob write for guests.
    pub quota_lock: Mutex<()>,
    /// Approximate encoded size: full state at load/compaction plus every update since.
    /// Shared with the update observer, hence the Arc.
    pub approx_bytes: Arc<std::sync::atomic::AtomicU64>,
    /// Sockets and links currently attached.
    pub conns: std::sync::atomic::AtomicUsize,
    pub last_active: Arc<Mutex<std::time::Instant>>,
}

impl LiveDoc {
    fn load(id: String, label: String, kind: DocKind, store: VaultStore, presence: Option<PresenceSender>) -> anyhow::Result<Arc<Self>> {
        let (doc, n) = store.load_doc()?;
        if store.needs_compaction() {
            let full = doc.transact().encode_state_as_update_v1(&StateVector::default());
            store.compact(&full)?;
            tracing::info!(doc = %label, "compacted {n} updates into one");
        }

        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let approx_bytes = Arc::new(std::sync::atomic::AtomicU64::new(
            doc.transact().encode_state_as_update_v1(&StateVector::default()).len() as u64,
        ));
        let last_active = Arc::new(Mutex::new(std::time::Instant::now()));
        {
            let store = store.clone();
            let tx = tx.clone();
            let label = label.clone();
            let approx = approx_bytes.clone();
            let active = last_active.clone();
            doc.observe_update_v1("samsyn", move |_txn, event| {
                if let Err(e) = store.append_update(&event.update) {
                    tracing::error!(doc = %label, "failed to persist update: {e}");
                }
                approx.fetch_add(event.update.len() as u64, std::sync::atomic::Ordering::Relaxed);
                *active.lock() = std::time::Instant::now();
                let _ = tx.send(encode_message(&Message::Sync(SyncMessage::Update(event.update.clone()))));
            })?;
        }

        let changes: Arc<Mutex<Vec<Change>>> = Arc::new(Mutex::new(Vec::new()));
        let texts = doc.get_or_insert_map("texts");
        let blobs = doc.get_or_insert_map("blobs");
        {
            let changes = changes.clone();
            texts.observe_deep("samsyn-bridge", move |txn, events| {
                let from = origin_source(txn.origin());
                let mut out = changes.lock();
                let mut handled: Vec<String> = Vec::new();
                for ev in events.iter() {
                    match ev {
                        Event::Map(m) if ev.path().is_empty() => {
                            for (k, ch) in m.keys(txn) {
                                let key = k.to_string();
                                let kind = match ch {
                                    EntryChange::Inserted(Out::YText(t)) | EntryChange::Updated(_, Out::YText(t)) => {
                                        ChangeKind::TextSet(t.get_string(txn))
                                    }
                                    EntryChange::Removed(_) => ChangeKind::Delete,
                                    _ => continue,
                                };
                                handled.push(key.clone());
                                out.push(Change { key, kind, from: from.clone() });
                            }
                        }
                        Event::Text(t) => {
                            let Some(PathSegment::Key(k)) = ev.path().front().cloned() else { continue };
                            let key = k.to_string();
                            if handled.contains(&key) {
                                continue;
                            }
                            // Anything but retain / delete / string insert (an embed, say) makes
                            // positions untrustworthy: send the whole text instead.
                            let mut delta: Vec<OwnedDelta> = Vec::new();
                            let mut plain = true;
                            for d in t.delta(txn).iter() {
                                match d {
                                    yrs::types::Delta::Retain(n, _) => delta.push(OwnedDelta::Retain(*n)),
                                    yrs::types::Delta::Deleted(n) => delta.push(OwnedDelta::Delete(*n)),
                                    yrs::types::Delta::Inserted(Out::Any(Any::String(s)), _) => delta.push(OwnedDelta::Insert(s.to_string())),
                                    _ => plain = false,
                                }
                            }
                            let full = t.target().get_string(txn);
                            if !plain {
                                out.push(Change { key, kind: ChangeKind::TextSet(full), from: from.clone() });
                            } else if !delta.is_empty() {
                                out.push(Change { key, kind: ChangeKind::TextDelta(delta, full), from: from.clone() });
                            }
                        }
                        _ => {}
                    }
                }
            });
        }
        {
            let changes = changes.clone();
            blobs.observe("samsyn-bridge", move |txn, ev| {
                let from = origin_source(txn.origin());
                let mut out = changes.lock();
                for (k, ch) in ev.keys(txn) {
                    let kind = match ch {
                        EntryChange::Inserted(Out::Any(a)) | EntryChange::Updated(_, Out::Any(a)) => ChangeKind::BlobSet(a.clone()),
                        EntryChange::Removed(_) => ChangeKind::Delete,
                        _ => continue,
                    };
                    out.push(Change { key: k.to_string(), kind, from: from.clone() });
                }
            });
        }

        let mut awareness = Awareness::new(doc);
        {
            let tx = tx.clone();
            let doc_id = id.clone();
            awareness.on_update("samsyn", move |aw, event, _origin| {
                if let Ok(update) = aw.update_with_clients(event.all_changes()) {
                    if let Some(p) = &presence {
                        let _ = p.send((doc_id.clone(), update.clone()));
                    }
                    let _ = tx.send(encode_message(&Message::Awareness(update)));
                }
            });
        }

        tracing::info!(doc = %label, "loaded ({n} stored updates)");
        Ok(Arc::new(LiveDoc {
            id,
            label,
            kind,
            store,
            awareness: Mutex::new(awareness),
            tx,
            changes,
            quota_lock: Mutex::new(()),
            approx_bytes,
            conns: std::sync::atomic::AtomicUsize::new(0),
            last_active,
        }))
    }

    pub fn full_state(&self) -> Vec<u8> {
        let aw = self.awareness.lock();
        let state = aw.doc().transact().encode_state_as_update_v1(&StateVector::default());
        state
    }

    pub fn snapshot(&self, keep: usize) -> anyhow::Result<()> {
        let aw = self.awareness.lock();
        let path = self.store.write_snapshot(aw.doc(), &self.id)?;
        let full = aw.doc().transact().encode_state_as_update_v1(&StateVector::default());
        drop(aw);
        self.store.compact(&full)?;
        self.approx_bytes.store(full.len() as u64, std::sync::atomic::Ordering::Relaxed);
        let pruned = self.store.prune_snapshots(keep)?;
        tracing::info!(doc = %self.label, "snapshot {} written, {pruned} pruned", path.display());
        Ok(())
    }

    pub fn vault_info(&self) -> Option<&VaultInfo> {
        match &self.kind {
            DocKind::Vault(v) => Some(v),
            _ => None,
        }
    }

    /// All keys in `texts` and `blobs`.
    pub fn keys(&self) -> Vec<String> {
        let aw = self.awareness.lock();
        let txn = aw.doc().transact();
        let mut out: Vec<String> = txn.get_map("texts").map(|m| m.keys(&txn).map(|k| k.to_string()).collect()).unwrap_or_default();
        if let Some(b) = txn.get_map("blobs") {
            out.extend(b.keys(&txn).map(|k| k.to_string()));
        }
        out
    }
}

/// Two prefixes overlap when one contains the other (or they are equal).
pub(crate) fn overlaps(a: &str, b: &str) -> bool {
    a == b || a.starts_with(b) || b.starts_with(a)
}

/// Mount folders the owner bridge of `share` must leave alone: every mount row of the vault
/// strictly inside the share whose owner did not opt into carrying it onward with this share.
/// Revoked rows count too, until they are removed: a copy that stopped syncing is still other
/// people's content and must not start flowing to the share's holders on its own.
pub(crate) fn owner_excludes(reg: &crate::storage::Registry, share: &ShareInfo) -> anyhow::Result<Vec<String>> {
    if share.only_file.is_some() {
        return Ok(Vec::new());
    }
    Ok(reg
        .mounts_for_vault(&share.vault_id)?
        .into_iter()
        .filter(|m| m.mount_prefix.starts_with(&share.prefix) && m.mount_prefix != share.prefix && !m.forward_shares.iter().any(|s| s == &share.id))
        .map(|m| m.mount_prefix)
        .collect())
}

fn origin_source(origin: Option<&yrs::Origin>) -> Option<String> {
    let o = origin?;
    let bytes: &[u8] = o.as_ref();
    bytes.strip_prefix(BRIDGE_ORIGIN_PREFIX).map(|b| String::from_utf8_lossy(b).to_string())
}

pub fn encode_message(msg: &Message) -> Bytes {
    let mut enc = EncoderV1::new();
    msg.encode(&mut enc);
    Bytes::from(enc.to_vec())
}

/// All loaded documents plus the bridges between them. `apply_lock` serializes every CRDT
/// mutation of any document on this server, which is what makes bridging safe.
pub struct DocRegistry {
    storage: Storage,
    pub node_id: String,
    vaults: Mutex<HashMap<String, Arc<LiveDoc>>>,
    shares: Mutex<HashMap<String, Arc<LiveDoc>>>,
    bridges: Mutex<Vec<Arc<Bridge>>>,
    pub apply_lock: Mutex<()>,
    /// Serializes share/mount creation (overlap check + registry insert + bridge attach) so
    /// two concurrent requests cannot both pass the overlap check. Taken before `apply_lock`.
    pub share_admin: Mutex<()>,
    /// Peer-to-peer endpoint for server-to-server shares; `None` with `--no-p2p`.
    pub endpoint: Option<iroh::Endpoint>,
    /// Largest blob we accept over a p2p link (same limit as the HTTP blob route).
    pub max_blob_bytes: usize,
    pub max_vaults_per_user: usize,
    /// Documents past this many encoded bytes refuse further updates (0 = unlimited).
    pub max_doc_bytes: u64,
    /// Awareness changes of every loaded document, consumed by the presence mirror task.
    presence_tx: PresenceSender,
    presence_rx: Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<PresenceEvent>>>,
    /// Running link tasks for remote mounts, by mount id.
    links: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    link_status: Mutex<HashMap<String, String>>,
}

impl DocRegistry {
    pub fn new(storage: Storage, node_id: String, endpoint: Option<iroh::Endpoint>, max_blob_bytes: usize, max_vaults_per_user: usize, max_doc_bytes: u64) -> Self {
        let (presence_tx, presence_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            presence_tx,
            presence_rx: Mutex::new(Some(presence_rx)),
            storage,
            node_id,
            vaults: Mutex::new(HashMap::new()),
            shares: Mutex::new(HashMap::new()),
            bridges: Mutex::new(Vec::new()),
            apply_lock: Mutex::new(()),
            share_admin: Mutex::new(()),
            endpoint,
            max_blob_bytes,
            max_vaults_per_user,
            max_doc_bytes,
            links: Mutex::new(HashMap::new()),
            link_status: Mutex::new(HashMap::new()),
        }
    }

    /// Start mirroring presence across bridges. Call once, inside the tokio runtime.
    pub fn start_presence_mirror(self: &Arc<Self>) {
        if let Some(rx) = self.presence_rx.lock().take() {
            crate::presence::spawn(self.clone(), rx);
        }
    }

    pub fn doc_by_id(&self, id: &str) -> Option<Arc<LiveDoc>> {
        if let Some(v) = self.vaults.lock().get(id).cloned() {
            return Some(v);
        }
        self.shares.lock().get(id).cloned()
    }

    pub fn set_link_status(&self, mount_id: &str, status: &str) {
        self.link_status.lock().insert(mount_id.to_string(), status.to_string());
    }

    pub fn link_status(&self, mount_id: &str) -> Option<String> {
        self.link_status.lock().get(mount_id).cloned()
    }

    /// A blob just landed in `doc`'s store (fetched over a link): copy it to the stores of
    /// every document bridged to `doc`, so clients of those vaults can download it.
    pub fn blob_arrived(&self, doc: &Arc<LiveDoc>, hash: &str) {
        let Ok(Some(bytes)) = doc.store.get_blob(hash) else { return };
        for b in self.bridges_for(&doc.id) {
            let other = if b.share.id == doc.id { &b.vault } else { &b.share };
            if !other.store.has_blob(hash) {
                if let Err(e) = other.store.put_blob(hash, &bytes) {
                    tracing::warn!(doc = %other.label, "copying fetched blob failed: {e}");
                }
            }
        }
    }

    /// The live share document for an incoming p2p link, loading the owner's vault (and with
    /// it the share and its bridge) if needed.
    pub fn share_doc_for_link(self: &Arc<Self>, share: &ShareInfo) -> anyhow::Result<Arc<LiveDoc>> {
        self.load_vault_by_id(&share.vault_id)?;
        let existing = self.shares.lock().get(&share.id).cloned();
        match existing {
            Some(d) => Ok(d),
            None => {
                let _g = self.apply_lock.lock();
                self.load_share(share)
            }
        }
    }

    pub fn storage(&self) -> &Storage {
        &self.storage
    }

    /// Live document for `user`'s vault `name`, loading it and its bridges on first use.
    pub fn get_or_load(self: &Arc<Self>, user: &User, name: &str) -> anyhow::Result<Arc<LiveDoc>> {
        let (info, store) = self.storage.open_vault_for_user(user, name, self.max_vaults_per_user)?;
        let existing = self.vaults.lock().get(&info.id).cloned();
        if let Some(v) = existing {
            return Ok(v);
        }
        let _g = self.apply_lock.lock();
        // Re-check under the apply lock: another connection may have loaded it meanwhile.
        let existing = self.vaults.lock().get(&info.id).cloned();
        if let Some(v) = existing {
            return Ok(v);
        }
        let doc = LiveDoc::load(info.id.clone(), format!("{}/{}", user.name, info.name), DocKind::Vault(info.clone()), store, Some(self.presence_tx.clone()))?;
        self.vaults.lock().insert(info.id.clone(), doc.clone());
        self.attach_vault_bridges(&doc, &info);
        Ok(doc)
    }

    pub fn loaded_vault(&self, vault_id: &str) -> Option<Arc<LiveDoc>> {
        self.vaults.lock().get(vault_id).cloned()
    }

    fn load_share(&self, info: &ShareInfo) -> anyhow::Result<Arc<LiveDoc>> {
        let existing = self.shares.lock().get(&info.id).cloned();
        if let Some(s) = existing {
            return Ok(s);
        }
        let store = self.storage.share_store(info)?;
        let doc = LiveDoc::load(info.id.clone(), format!("share:{}", info.name), DocKind::Share(info.clone()), store, Some(self.presence_tx.clone()))?;
        self.shares.lock().insert(info.id.clone(), doc.clone());
        Ok(doc)
    }

    fn attach_vault_bridges(self: &Arc<Self>, vault: &Arc<LiveDoc>, info: &VaultInfo) {
        let reg = self.storage.registry();
        match reg.shares_for_vault(&info.id) {
            Ok(shares) => {
                for s in shares {
                    if let Err(e) = self.attach_owner_bridge_locked(vault, &s) {
                        tracing::error!(share = %s.name, "attaching share bridge failed: {e}");
                    }
                }
            }
            Err(e) => tracing::error!("listing shares failed: {e}"),
        }
        match reg.mounts_for_vault(&info.id) {
            Ok(mounts) => {
                for m in mounts.into_iter().filter(|m| m.revoked_at.is_none()) {
                    if let Err(e) = self.attach_mount_bridge_locked(vault, &m) {
                        tracing::error!(mount = %m.share_name, "attaching mount bridge failed: {e}");
                    }
                }
            }
            Err(e) => tracing::error!("listing mounts failed: {e}"),
        }
    }

    fn attach_owner_bridge_locked(&self, vault: &Arc<LiveDoc>, share: &ShareInfo) -> anyhow::Result<()> {
        let sdoc = self.load_share(share)?;
        let bridge = Arc::new(Bridge {
            share_id: share.id.clone(),
            mount_id: None,
            vault: vault.clone(),
            share: sdoc,
            vault_prefix: share.prefix.clone(),
            only_file: share.only_file.clone(),
            access: Access::full(),
            truth: Side::Vault,
            exclude: owner_excludes(self.storage.registry(), share)?,
        });
        bridge.reconcile();
        self.bridges.lock().push(bridge.clone());
        self.propagate(&bridge.share);
        Ok(())
    }

    /// Re-attach the owner bridges of every folder share of `vault` that overlaps one of the
    /// `touching` prefixes, so their exclude lists match the registry again. Call after a
    /// mount was created, moved, deleted or had its forwarding changed. A share whose exclude
    /// list did not change is left alone; one that now skips a folder it used to carry first
    /// drops that folder from its share document, so the holders lose their copy. Must be
    /// called with `apply_lock` held.
    fn refresh_owner_bridges_locked(&self, vault: &Arc<LiveDoc>, touching: &[String]) -> anyhow::Result<()> {
        let reg = self.storage.registry();
        for share in reg.shares_for_vault(&vault.id)? {
            if share.only_file.is_some() || !touching.iter().any(|p| overlaps(&share.prefix, p)) {
                continue;
            }
            let mut wanted = owner_excludes(reg, &share)?;
            wanted.sort();
            let old = {
                let mut bridges = self.bridges.lock();
                let pos = bridges.iter().position(|b| b.share_id == share.id && b.mount_id.is_none());
                pos.map(|i| bridges.remove(i))
            };
            if let Some(old) = &old {
                let mut have = old.exclude.clone();
                have.sort();
                if have == wanted {
                    self.bridges.lock().push(old.clone());
                    continue;
                }
                for p in wanted.iter().filter(|p| !old.exclude.contains(p)) {
                    old.drop_from_share(p);
                }
            }
            self.attach_owner_bridge_locked(vault, &share)?;
        }
        Ok(())
    }

    /// Public form of `refresh_owner_bridges_locked`: takes the apply lock. A vault that is
    /// not loaded needs nothing, its bridges are built from the registry when it loads.
    pub fn refresh_owner_bridges(&self, vault_id: &str, touching: &[String]) -> anyhow::Result<()> {
        let _g = self.apply_lock.lock();
        match self.loaded_vault(vault_id) {
            Some(vault) => self.refresh_owner_bridges_locked(&vault, touching),
            None => Ok(()),
        }
    }

    fn attach_mount_bridge_locked(self: &Arc<Self>, vault: &Arc<LiveDoc>, mount: &MountInfo) -> anyhow::Result<()> {
        let (sdoc, only_file) = if mount.home_node == self.node_id {
            let share = self
                .storage
                .registry()
                .share_by_id(&mount.share_id)?
                .ok_or_else(|| anyhow::anyhow!("share {} no longer exists", mount.share_id))?;
            (self.load_share(&share)?, share.only_file.clone())
        } else {
            (self.load_remote_replica(mount)?, mount.only_file.clone())
        };
        let bridge = Arc::new(Bridge {
            share_id: mount.share_id.clone(),
            mount_id: Some(mount.id.clone()),
            vault: vault.clone(),
            share: sdoc.clone(),
            vault_prefix: mount.mount_prefix.clone(),
            only_file,
            access: Access::new(&mount.access, &mount.rw_paths),
            truth: Side::Share,
            exclude: Vec::new(),
        });
        bridge.reconcile();
        self.bridges.lock().push(bridge.clone());
        self.propagate(&bridge.vault);
        if mount.home_node != self.node_id {
            if self.endpoint.is_some() {
                let handle = crate::remote::spawn_link(self.clone(), mount.clone(), sdoc);
                if let Some(old) = self.links.lock().insert(mount.id.clone(), handle) {
                    old.abort();
                }
            } else {
                tracing::warn!(mount = %mount.share_name, "p2p is disabled (--no-p2p); remote mount stays offline");
                self.set_link_status(&mount.id, "offline (p2p disabled)");
            }
        }
        Ok(())
    }

    /// Samsyn document for a remote mount, keyed by mount id in the share map.
    fn load_remote_replica(&self, mount: &MountInfo) -> anyhow::Result<Arc<LiveDoc>> {
        let existing = self.shares.lock().get(&mount.id).cloned();
        if let Some(s) = existing {
            return Ok(s);
        }
        let store = self.storage.mount_store(mount)?;
        let doc = LiveDoc::load(mount.id.clone(), format!("remote:{}", mount.share_name), DocKind::Remote(mount.clone()), store, Some(self.presence_tx.clone()))?;
        self.shares.lock().insert(mount.id.clone(), doc.clone());
        Ok(doc)
    }

    /// Called by the API after creating a share. Takes the apply lock itself.
    pub fn add_owner_bridge(&self, share: &ShareInfo) -> anyhow::Result<()> {
        let _g = self.apply_lock.lock();
        let vault = self.loaded_vault(&share.vault_id).ok_or_else(|| anyhow::anyhow!("vault not loaded"))?;
        self.attach_owner_bridge_locked(&vault, share)
    }

    /// Called by the API after creating or restoring a mount. The owner bridges around the
    /// mount folder are refreshed first, so the share that should skip it does so before the
    /// first key arrives, and the one that should carry it onward sees the reconcile.
    pub fn add_mount_bridge(self: &Arc<Self>, mount: &MountInfo) -> anyhow::Result<()> {
        let _g = self.apply_lock.lock();
        let vault = self.loaded_vault(&mount.vault_id).ok_or_else(|| anyhow::anyhow!("vault not loaded"))?;
        self.refresh_owner_bridges_locked(&vault, std::slice::from_ref(&mount.mount_prefix))?;
        self.attach_mount_bridge_locked(&vault, mount)
    }

    /// Move a share or a mount to another place in its vault without anything looking like a
    /// deletion to the other side: detach the bridge, rename the keys inside the vault document,
    /// store the new prefix, attach again. Returns the updated row(s) for the API.
    /// `only_file` is `Some` for single-note shares (the note may be renamed too).
    pub fn move_prefix(
        self: &Arc<Self>,
        vault_id: &str,
        share_id: &str,
        mount_id: Option<&str>,
        old_prefix: &str,
        new_prefix: &str,
        old_file: Option<&str>,
        new_file: Option<&str>,
    ) -> anyhow::Result<()> {
        let vault = self.load_vault_by_id(vault_id)?;
        let _g = self.apply_lock.lock();
        // Detach only this bridge; the share document and any link task stay as they are.
        self.bridges.lock().retain(|b| !(b.share_id == share_id && b.mount_id.as_deref() == mount_id));
        // A moving mount must not leak through the owner bridges around it: a share that skips
        // the mount at its old folder would mirror the keys appearing at the new one (and one
        // that carries it would see a deletion before the registry knows the new folder). Park
        // every folder share touching either end for the duration; re-attaching reconciles
        // them against the vault, which by then holds the keys at the new place only.
        let parked: Vec<ShareInfo> = if mount_id.is_some() {
            let touching: Vec<ShareInfo> = self
                .storage
                .registry()
                .shares_for_vault(vault_id)?
                .into_iter()
                .filter(|s| s.only_file.is_none() && (overlaps(&s.prefix, old_prefix) || overlaps(&s.prefix, new_prefix)))
                .collect();
            let ids: Vec<&str> = touching.iter().map(|s| s.id.as_str()).collect();
            self.bridges.lock().retain(|b| !(b.mount_id.is_none() && ids.contains(&b.share_id.as_str())));
            touching
        } else {
            Vec::new()
        };

        let (old_root, new_root) = match (old_file, new_file) {
            (Some(of), Some(nf)) => (format!("{old_prefix}{of}"), format!("{new_prefix}{nf}")),
            _ => (old_prefix.to_string(), new_prefix.to_string()),
        };
        let moved = {
            let aw = vault.awareness.lock();
            let doc = aw.doc().clone();
            let mut txn = doc.transact_mut_with(Origin::from(b"bridge:move".as_slice()));
            let texts = txn.get_or_insert_map("texts");
            let blobs = txn.get_or_insert_map("blobs");
            let keys: Vec<String> = texts
                .keys(&txn)
                .chain(blobs.keys(&txn))
                .filter(|k| if old_file.is_some() { *k == old_root } else { k.starts_with(&old_root) })
                .map(str::to_string)
                .collect();
            for k in &keys {
                let nk = format!("{new_root}{}", &k[old_root.len()..]);
                if let Some(Out::YText(t)) = texts.get(&txn, k) {
                    let s = t.get_string(&txn);
                    texts.remove(&mut txn, k);
                    texts.insert(&mut txn, nk.as_str(), yrs::TextPrelim::new(s.as_str()));
                }
                if let Some(Out::Any(a)) = blobs.get(&txn, k) {
                    blobs.remove(&mut txn, k);
                    blobs.insert(&mut txn, nk.as_str(), a);
                }
            }
            keys.len()
        };
        // The moved bridge is detached, so nothing from this transaction reaches its share.
        // Other bridges on this vault do see the renames: a share that contains the moved
        // share must follow it, or its mounts go stale. For a mount, the shares around it are
        // parked, so this forwards nothing.
        self.propagate(&vault);

        let reg = self.storage.registry();
        if let Some(mid) = mount_id {
            reg.update_mount_prefix(mid, new_prefix)?;
            let mount = reg.mount_by_id(mid)?.ok_or_else(|| anyhow::anyhow!("mount vanished"))?;
            // Re-attach with the new prefix (for remote mounts this restarts the link; the
            // replica document is untouched so nothing is re-downloaded).
            self.attach_mount_bridge_locked(&vault, &mount)?;
            for s in parked {
                if let Some(s) = reg.share_by_id(&s.id)? {
                    self.attach_owner_bridge_locked(&vault, &s)?;
                }
            }
        } else {
            reg.update_share_path(share_id, new_prefix, new_file)?;
            let share = reg.share_by_id(share_id)?.ok_or_else(|| anyhow::anyhow!("share vanished"))?;
            self.attach_owner_bridge_locked(&vault, &share)?;
            // Mount bridges on this server keep working: they only reference the share document.
        }
        tracing::info!(share = %share_id, mount = ?mount_id, "moved {moved} keys from {old_root} to {new_root}");
        Ok(())
    }

    pub fn remove_bridges_for_share(&self, share_id: &str) {
        let _g = self.apply_lock.lock();
        self.bridges.lock().retain(|b| b.share_id != share_id);
        self.shares.lock().remove(share_id);
    }

    pub fn remove_bridges_for_mounts(&self, mount_ids: &[String]) {
        self.remove_mounts_inner(mount_ids, true);
    }

    /// Same as `remove_bridges_for_mounts` for one mount, called by that mount's own link task:
    /// forgets the task handle instead of aborting it, so the caller finishes its cleanup.
    pub fn detach_own_mount(&self, mount_id: &str) {
        self.remove_mounts_inner(std::slice::from_ref(&mount_id.to_string()), false);
    }

    fn remove_mounts_inner(&self, mount_ids: &[String], abort_links: bool) {
        let _g = self.apply_lock.lock();
        let mut bridges = self.bridges.lock();
        let before = bridges.len();
        bridges.retain(|b| b.mount_id.as_ref().map(|m| !mount_ids.contains(m)).unwrap_or(true));
        tracing::info!("removed {} mount bridge(s) for {:?}", before - bridges.len(), mount_ids);
        drop(bridges);
        let mut links = self.links.lock();
        for id in mount_ids {
            if let Some(h) = links.remove(id) {
                if abort_links {
                    h.abort();
                }
            }
            self.shares.lock().remove(id);
            self.link_status.lock().remove(id);
        }
    }

    /// Load every vault that takes part in a bridge, owners first, so a share can never
    /// advance while one of its ends is not loaded (which would make reconcile lossy).
    pub fn preload_bridged(self: &Arc<Self>) {
        let reg = self.storage.registry();
        let mut vault_ids: Vec<String> = Vec::new();
        for s in reg.list_vaults(None).unwrap_or_default() {
            if reg.shares_for_vault(&s.id).map(|v| !v.is_empty()).unwrap_or(false) {
                vault_ids.push(s.id);
            }
        }
        for v in reg.list_vaults(None).unwrap_or_default() {
            if reg.mounts_for_vault(&v.id).map(|m| m.iter().any(|m| m.revoked_at.is_none())).unwrap_or(false) && !vault_ids.contains(&v.id) {
                vault_ids.push(v.id);
            }
        }
        for id in vault_ids {
            if let Err(e) = self.load_vault_by_id(&id) {
                tracing::error!(vault = %id, "preloading bridged vault failed: {e}");
            }
        }
    }

    pub fn load_vault_by_id(self: &Arc<Self>, vault_id: &str) -> anyhow::Result<Arc<LiveDoc>> {
        let reg = self.storage.registry();
        let info = reg.vault_by_id(vault_id)?.ok_or_else(|| anyhow::anyhow!("no vault {vault_id}"))?;
        let user = reg.user_by_id(&info.user_id)?.ok_or_else(|| anyhow::anyhow!("no user {}", info.user_id))?;
        self.get_or_load(&user, &info.name)
    }

    pub fn bridges_for(&self, doc_id: &str) -> Vec<Arc<Bridge>> {
        self.bridges
            .lock()
            .iter()
            .filter(|b| b.vault.id == doc_id || b.share.id == doc_id)
            .cloned()
            .collect()
    }

    /// A peer with partial write access (a read-only ticket with writable paths) just had its
    /// update applied to the share document `doc`. Its changes on writable keys stay; the rest
    /// are undone by re-asserting the owner's vault content, which has not seen them yet, and
    /// dropped from the propagation queue so no mount ever sees them. Must be called with
    /// `apply_lock` held, after the apply and before `propagate`.
    pub fn enforce_share_access(&self, doc: &Arc<LiveDoc>, access: &Access) {
        if access.base_rw {
            return;
        }
        let blocked: Vec<String> = {
            let mut changes = doc.changes.lock();
            let (keep, blocked): (Vec<Change>, Vec<Change>) =
                std::mem::take(&mut *changes).into_iter().partition(|c| c.from.is_some() || access.writable(&c.key));
            *changes = keep;
            let mut keys: Vec<String> = blocked.into_iter().map(|c| c.key).collect();
            keys.sort();
            keys.dedup();
            keys
        };
        if blocked.is_empty() {
            return;
        }
        tracing::warn!(doc = %doc.label, "peer wrote outside its writable paths; reverting {} key(s)", blocked.len());
        let owner = self.bridges_for(&doc.id).into_iter().find(|b| b.share.id == doc.id && b.mount_id.is_none());
        match owner {
            Some(b) => {
                let vault_keys: Vec<String> = blocked.iter().filter_map(|k| b.share_to_vault_key(k)).collect();
                b.assert_keys(&b.vault, &b.share, &vault_keys, true);
            }
            None => tracing::error!(doc = %doc.label, "no owner bridge loaded; cannot revert {blocked:?}"),
        }
    }

    /// Drain recorded changes of `start` and replay them across bridges until nothing is left.
    /// Must be called with `apply_lock` held.
    pub fn propagate(&self, start: &Arc<LiveDoc>) {
        let mut work: Vec<Arc<LiveDoc>> = vec![start.clone()];
        let mut rounds = 0;
        while let Some(doc) = work.pop() {
            let changes: Vec<Change> = std::mem::take(&mut *doc.changes.lock());
            if changes.is_empty() {
                continue;
            }
            for bridge in self.bridges_for(&doc.id) {
                work.extend(bridge.forward(&doc, &changes));
            }
            rounds += 1;
            if rounds > 10_000 {
                tracing::error!("bridge propagation did not settle; aborting this round");
                break;
            }
        }
        // Drop anything recorded on docs without bridges so it does not pile up.
        for d in self.vaults.lock().values().chain(self.shares.lock().values()) {
            if self.bridges_for(&d.id).is_empty() {
                d.changes.lock().clear();
            }
        }
    }

    pub fn loaded_count(&self) -> usize {
        self.vaults.lock().len()
    }

    pub fn maybe_compact(&self, doc: &LiveDoc) {
        if doc.store.needs_compaction() {
            let full = doc.full_state();
            doc.approx_bytes.store(full.len() as u64, std::sync::atomic::Ordering::Relaxed);
            match doc.store.compact(&full) {
                Ok(()) => tracing::info!(doc = %doc.label, "compacted update log"),
                Err(e) => tracing::error!(doc = %doc.label, "compaction failed: {e}"),
            }
        }
    }

    /// Drop vaults from memory that have had no connection or activity for `idle` and take
    /// part in no bridge. They come back from disk on the next connection.
    pub fn unload_idle(&self, idle: std::time::Duration, keep_snapshots: usize) {
        let candidates: Vec<Arc<LiveDoc>> = self
            .vaults
            .lock()
            .values()
            .filter(|d| d.conns.load(std::sync::atomic::Ordering::Relaxed) == 0 && d.last_active.lock().elapsed() > idle)
            .cloned()
            .collect();
        for d in candidates {
            let _g = self.apply_lock.lock();
            if d.conns.load(std::sync::atomic::Ordering::Relaxed) != 0 || !self.bridges_for(&d.id).is_empty() {
                continue;
            }
            if let Err(e) = d.snapshot(keep_snapshots) {
                tracing::warn!(doc = %d.label, "snapshot before unload failed: {e}; keeping it loaded");
                continue;
            }
            self.vaults.lock().remove(&d.id);
            tracing::info!(doc = %d.label, "unloaded after {} min idle", idle.as_secs() / 60);
        }
    }

    pub fn snapshot_all(&self, keep: usize) {
        let docs: Vec<Arc<LiveDoc>> = {
            let v = self.vaults.lock();
            let s = self.shares.lock();
            v.values().chain(s.values()).cloned().collect()
        };
        for d in docs {
            if let Err(e) = d.snapshot(keep) {
                tracing::error!(doc = %d.label, "snapshot failed: {e}");
            }
        }
    }
}
