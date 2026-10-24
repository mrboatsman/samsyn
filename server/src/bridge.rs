//! A bridge keeps the subtree `vault_prefix/…` of a vault document equal to the whole of a
//! share document. Changes are replayed as plain edits with a `bridge:<source-doc-id>` origin
//! so observers can tell where they came from and never echo them back.

use std::sync::Arc;

use yrs::{Any, GetString, Map, Origin, Out, ReadTxn, Text, TextPrelim, Transact, WriteTxn};

use crate::access::Access;
use crate::client::diff::diff_text;
use crate::ids::valid_key;
use crate::vault::{LiveDoc, BRIDGE_ORIGIN_PREFIX};

#[derive(Debug, Clone)]
pub enum OwnedDelta {
    Retain(u32),
    Delete(u32),
    Insert(String),
}

#[derive(Debug, Clone)]
pub enum ChangeKind {
    /// Delta plus the full text after it, so a replay that does not fit (embeds, races) can
    /// fall back to setting the whole text instead of panicking inside yrs.
    TextDelta(Vec<OwnedDelta>, String),
    TextSet(String),
    BlobSet(Any),
    Delete,
}

#[derive(Debug, Clone)]
pub struct Change {
    pub key: String,
    pub kind: ChangeKind,
    /// Doc id this change was bridged from, if it did not come from a client.
    pub from: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Vault,
    Share,
}

pub struct Bridge {
    pub share_id: String,
    pub mount_id: Option<String>,
    pub vault: Arc<LiveDoc>,
    pub share: Arc<LiveDoc>,
    pub vault_prefix: String,
    pub only_file: Option<String>,
    /// What the vault side may write into the share. Owner bridges are always full access;
    /// a mount's comes from its ticket (read & write, read-only, or read-only with writable paths).
    pub access: Access,
    pub truth: Side,
    /// Vault folders (full prefixes with trailing slash, inside `vault_prefix`) this bridge
    /// leaves alone in both directions: mounts of other people's shares that the owner chose
    /// not to carry onward with this share. Owner bridges only; always empty for mounts.
    pub exclude: Vec<String>,
}

fn bridge_origin(src: &str) -> Origin {
    let mut v = BRIDGE_ORIGIN_PREFIX.to_vec();
    v.extend_from_slice(src.as_bytes());
    Origin::from(v.as_slice())
}

impl Bridge {
    fn excluded(&self, vault_key: &str) -> bool {
        self.exclude.iter().any(|p| vault_key.starts_with(p.as_str()))
    }

    pub fn vault_to_share_key(&self, key: &str) -> Option<String> {
        if !valid_key(key) {
            tracing::warn!(share = %self.share_id, "refusing to bridge invalid key {key:?}");
            return None;
        }
        let rest = key.strip_prefix(self.vault_prefix.as_str())?;
        if rest.is_empty() || self.excluded(key) {
            return None;
        }
        match &self.only_file {
            Some(f) if f != rest => None,
            _ => Some(rest.to_string()),
        }
    }

    pub fn share_to_vault_key(&self, key: &str) -> Option<String> {
        if !valid_key(key) {
            tracing::warn!(share = %self.share_id, "refusing to bridge invalid key {key:?}");
            return None;
        }
        let vk = match &self.only_file {
            Some(f) if f != key => return None,
            _ => format!("{}{}", self.vault_prefix, key),
        };
        if self.excluded(&vk) {
            return None;
        }
        Some(vk)
    }

    /// Remove everything under the vault folder `prefix` from the share document: the owner
    /// stopped carrying a mount onward with this share, so its holders lose their copy. Only
    /// meaningful on a bridge that does not (yet) exclude `prefix`.
    pub fn drop_from_share(&self, prefix: &str) {
        let Some(rel) = prefix.strip_prefix(self.vault_prefix.as_str()) else { return };
        let aw = self.share.awareness.lock();
        let doc = aw.doc().clone();
        let mut txn = doc.transact_mut_with(bridge_origin(&self.vault.id));
        let texts = txn.get_or_insert_map("texts");
        let blobs = txn.get_or_insert_map("blobs");
        let keys: Vec<String> = texts.keys(&txn).chain(blobs.keys(&txn)).filter(|k| k.starts_with(rel)).map(str::to_string).collect();
        for k in &keys {
            texts.remove(&mut txn, k);
            blobs.remove(&mut txn, k);
        }
        if !keys.is_empty() {
            tracing::info!(share = %self.share.label, "dropped {} key(s) under {prefix} no longer carried onward", keys.len());
        }
    }

    /// Replay `changes` recorded on `source` into the other side. Returns every doc that was
    /// written (so the caller continues propagation from them): the target, and/or the vault
    /// when edits on read-only keys were undone there.
    pub fn forward(&self, source: &Arc<LiveDoc>, changes: &[Change]) -> Vec<Arc<LiveDoc>> {
        let (target, to_share) = if source.id == self.vault.id {
            (&self.share, true)
        } else if source.id == self.share.id {
            (&self.vault, false)
        } else {
            return Vec::new();
        };
        let mut mapped: Vec<(String, &Change)> = changes
            .iter()
            .filter(|c| c.from.as_deref() != Some(target.id.as_str()))
            .filter_map(|c| {
                let k = if to_share { self.vault_to_share_key(&c.key) } else { self.share_to_vault_key(&c.key) };
                k.map(|k| (k, c))
            })
            .collect();
        if mapped.is_empty() {
            return Vec::new();
        }
        let mut written = Vec::new();

        if to_share && !self.access.base_rw {
            // Read-only mount (possibly with writable paths): undo the vault-side edits on keys
            // the ticket does not cover by re-asserting the share's content; the rest flows.
            let (allowed, blocked): (Vec<_>, Vec<_>) = mapped.into_iter().partition(|(k, _)| self.access.writable(k));
            if !blocked.is_empty() {
                tracing::info!(mount = ?self.mount_id, "reverting {} edit(s) on the read-only part of a mount", blocked.len());
                let keys: Vec<String> = blocked.iter().map(|(k, _)| k.clone()).collect();
                self.rescue_unshared(&keys);
                self.assert_keys(&self.share, &self.vault, &keys, false);
                written.push(self.vault.clone());
            }
            mapped = allowed;
            if mapped.is_empty() {
                return written;
            }
        }

        // Blob bytes go first: the metadata insert below is broadcast to clients at once and
        // they fetch immediately, so the target store must already hold the bytes.
        for (k, c) in &mapped {
            if let ChangeKind::BlobSet(a) = &c.kind {
                copy_blob(source, target, a, k);
            }
        }
        apply_changes(target, &source.id, &mapped);
        written.push(target.clone());
        written
    }

    /// On a read-only mount, a key (in share terms) that the share does not have but the vault
    /// does is content the friend created locally. Reverting would silently delete it, so copy
    /// it first to a sibling folder outside the mount: `<mount> (read-only copy)/<rest>`.
    fn rescue_unshared(&self, share_keys: &[String]) {
        let missing: Vec<String> = {
            let aw = self.share.awareness.lock();
            let txn = aw.doc().transact();
            let texts = txn.get_map("texts");
            let blobs = txn.get_map("blobs");
            share_keys
                .iter()
                .filter(|k| {
                    let in_texts = texts.as_ref().map(|m| m.contains_key(&txn, k)).unwrap_or(false);
                    let in_blobs = blobs.as_ref().map(|m| m.contains_key(&txn, k)).unwrap_or(false);
                    !in_texts && !in_blobs
                })
                .cloned()
                .collect()
        };
        if missing.is_empty() {
            return;
        }
        let rescue_root = format!("{} (read-only copy)/", self.vault_prefix.trim_end_matches('/'));
        let mut changes: Vec<(String, Change)> = Vec::new();
        {
            let aw = self.vault.awareness.lock();
            let txn = aw.doc().transact();
            let texts = txn.get_map("texts");
            let blobs = txn.get_map("blobs");
            for k in &missing {
                let Some(vk) = self.share_to_vault_key(k) else { continue };
                let text = texts.as_ref().and_then(|m| match m.get(&txn, &vk) {
                    Some(Out::YText(t)) => Some(t.get_string(&txn)),
                    _ => None,
                });
                let blob = blobs.as_ref().and_then(|m| match m.get(&txn, &vk) {
                    Some(Out::Any(a)) => Some(a),
                    _ => None,
                });
                let kind = match (text, blob) {
                    (Some(t), _) => ChangeKind::TextSet(t),
                    (None, Some(b)) => ChangeKind::BlobSet(b),
                    (None, None) => continue,
                };
                let rk = format!("{rescue_root}{k}");
                tracing::warn!(mount = ?self.mount_id, "read-only mount: keeping new file {vk} as {rk}");
                changes.push((rk.clone(), Change { key: rk, kind, from: None }));
            }
        }
        if changes.is_empty() {
            return;
        }
        let refs: Vec<(String, &Change)> = changes.iter().map(|(k, c)| (k.clone(), c)).collect();
        apply_changes(&self.vault, &self.share.id, &refs);
    }

    /// Make the non-truth side equal to the truth side for the whole bridged subtree.
    pub fn reconcile(&self) {
        let (truth, other, to_share) = match self.truth {
            Side::Vault => (&self.vault, &self.share, true),
            Side::Share => (&self.share, &self.vault, false),
        };
        let truth_keys: Vec<(String, String)> = truth
            .keys()
            .into_iter()
            .filter_map(|k| {
                let mapped = if to_share { self.vault_to_share_key(&k) } else { self.share_to_vault_key(&k) };
                mapped.map(|m| (k, m))
            })
            .collect();
        let other_keys: Vec<String> = other
            .keys()
            .into_iter()
            .filter(|k| if to_share { self.share_to_vault_key(k).is_some() } else { self.vault_to_share_key(k).is_some() })
            .collect();

        // Keys only on the other side: delete.
        let truth_mapped: Vec<&String> = truth_keys.iter().map(|(_, m)| m).collect();
        let stale: Vec<String> = other_keys.into_iter().filter(|k| !truth_mapped.contains(&k)).collect();
        if !stale.is_empty() {
            let aw = other.awareness.lock();
            let doc = aw.doc().clone();
            let mut txn = doc.transact_mut_with(bridge_origin(&truth.id));
            let texts = txn.get_or_insert_map("texts");
            let blobs = txn.get_or_insert_map("blobs");
            for k in &stale {
                texts.remove(&mut txn, k);
                blobs.remove(&mut txn, k);
            }
        }
        let keys: Vec<String> = truth_keys.iter().map(|(k, _)| k.clone()).collect();
        self.assert_keys(truth, other, &keys, to_share);
        tracing::info!(
            share = %self.share.label,
            "bridge reconciled: {} keys, {} stale removed",
            truth_keys.len(),
            stale.len()
        );
    }

    /// Copy the given keys (named in `src` terms) from `src` to `dst`, making them equal.
    pub(crate) fn assert_keys(&self, src: &Arc<LiveDoc>, dst: &Arc<LiveDoc>, keys: &[String], src_is_vault: bool) {
        // Read everything from src first so we never hold two awareness locks at once.
        let mut items: Vec<(String, Option<String>, Option<Any>)> = Vec::new();
        {
            let aw = src.awareness.lock();
            let txn = aw.doc().transact();
            let texts = txn.get_map("texts");
            let blobs = txn.get_map("blobs");
            for k in keys {
                let mapped = if src_is_vault { self.vault_to_share_key(k) } else { self.share_to_vault_key(k) };
                let Some(mapped) = mapped else { continue };
                let text = texts.as_ref().and_then(|m| match m.get(&txn, k) {
                    Some(Out::YText(t)) => Some(t.get_string(&txn)),
                    _ => None,
                });
                let blob = blobs.as_ref().and_then(|m| match m.get(&txn, k) {
                    Some(Out::Any(a)) => Some(a),
                    _ => None,
                });
                items.push((mapped, text, blob));
            }
        }
        let changes: Vec<(String, Change)> = items
            .into_iter()
            .map(|(k, text, blob)| {
                let kind = match (text, blob) {
                    (Some(t), _) => ChangeKind::TextSet(t),
                    (None, Some(b)) => ChangeKind::BlobSet(b),
                    (None, None) => ChangeKind::Delete,
                };
                (k.clone(), Change { key: k, kind, from: None })
            })
            .collect();
        let refs: Vec<(String, &Change)> = changes.iter().map(|(k, c)| (k.clone(), c)).collect();
        for (k, c) in &refs {
            if let ChangeKind::BlobSet(a) = &c.kind {
                copy_blob(src, dst, a, k);
            }
        }
        apply_changes(dst, &src.id, &refs);
    }
}

/// Apply mapped changes to `target` in one transaction with origin `bridge:<src>`.
fn apply_changes(target: &Arc<LiveDoc>, src_id: &str, changes: &[(String, &Change)]) {
    let aw = target.awareness.lock();
    let doc = aw.doc().clone();
    let mut txn = doc.transact_mut_with(bridge_origin(src_id));
    let texts = txn.get_or_insert_map("texts");
    let blobs = txn.get_or_insert_map("blobs");
    for (key, change) in changes {
        match &change.kind {
            ChangeKind::TextDelta(delta, full) => {
                let t = match texts.get(&txn, key) {
                    Some(Out::YText(t)) => t,
                    _ => texts.insert(&mut txn, key.as_str(), TextPrelim::new("")),
                };
                if !replay_text_delta(&mut txn, &t, delta, full) {
                    tracing::warn!("delta for {key} does not fit the target text; set the whole text instead");
                }
            }
            ChangeKind::TextSet(content) => match texts.get(&txn, key) {
                Some(Out::YText(t)) => {
                    let cur = t.get_string(&txn);
                    if let Some(d) = diff_text(&cur, content) {
                        if d.remove > 0 {
                            t.remove_range(&mut txn, d.index as u32, d.remove as u32);
                        }
                        if !d.insert.is_empty() {
                            t.insert(&mut txn, d.index as u32, &d.insert);
                        }
                    }
                }
                _ => {
                    texts.insert(&mut txn, key.as_str(), TextPrelim::new(content.as_str()));
                }
            },
            ChangeKind::BlobSet(a) => {
                let same = matches!(blobs.get(&txn, key), Some(Out::Any(cur)) if &cur == a);
                if !same {
                    blobs.insert(&mut txn, key.as_str(), a.clone());
                }
            }
            ChangeKind::Delete => {
                texts.remove(&mut txn, key);
                blobs.remove(&mut txn, key);
            }
        }
    }
}

fn copy_blob(src: &LiveDoc, dst: &LiveDoc, meta: &Any, key: &str) {
    let Any::Map(m) = meta else { return };
    let Some(Any::String(hash)) = m.get("hash") else { return };
    if dst.store.has_blob(hash) {
        return;
    }
    match src.store.get_blob(hash) {
        Ok(Some(bytes)) => {
            if let Err(e) = dst.store.put_blob(hash, &bytes) {
                tracing::warn!("copying blob for {key} failed: {e}");
            }
        }
        Ok(None) => tracing::debug!("blob for {key} not present in source yet"),
        Err(e) => tracing::warn!("reading blob for {key} failed: {e}"),
    }
}

/// Replay a recorded text delta onto `t`. The delta is first simulated on the current string to
/// confirm every position lands in range and on a char boundary; only then is it applied to yrs
/// (whose `insert`/`remove_range` panic on an out-of-range or mid-codepoint position). When it
/// does not fit (embeds, races, or a hostile source document) the text is set to `full` with a
/// diff instead. Returns whether the delta fit. Pure over yrs types, so it can be fuzzed.
pub fn replay_text_delta(txn: &mut yrs::TransactionMut, t: &yrs::TextRef, delta: &[OwnedDelta], full: &str) -> bool {
    let cur = t.get_string(txn);
    if delta_fits(&cur, delta) {
        let mut pos: u32 = 0;
        for d in delta {
            match d {
                OwnedDelta::Retain(n) => pos += n,
                OwnedDelta::Delete(n) => t.remove_range(txn, pos, *n),
                OwnedDelta::Insert(s) => {
                    t.insert(txn, pos, s);
                    pos += s.len() as u32;
                }
            }
        }
        return true;
    }
    if let Some(d) = diff_text(&cur, full) {
        if d.remove > 0 {
            t.remove_range(txn, d.index as u32, d.remove as u32);
        }
        if !d.insert.is_empty() {
            t.insert(txn, d.index as u32, &d.insert);
        }
    }
    false
}

/// Simulate `delta` on a copy of `cur` (yrs here uses byte offsets). Every position must stay
/// within the evolving string and on a char boundary, or the delta is rejected.
fn delta_fits(cur: &str, delta: &[OwnedDelta]) -> bool {
    let mut buf = cur.to_string();
    let mut pos: usize = 0;
    for d in delta {
        match d {
            OwnedDelta::Retain(n) => {
                pos = match pos.checked_add(*n as usize) {
                    Some(p) => p,
                    None => return false,
                };
                if pos > buf.len() || !buf.is_char_boundary(pos) {
                    return false;
                }
            }
            OwnedDelta::Delete(n) => {
                let end = match pos.checked_add(*n as usize) {
                    Some(e) => e,
                    None => return false,
                };
                if end > buf.len() || !buf.is_char_boundary(pos) || !buf.is_char_boundary(end) {
                    return false;
                }
                buf.replace_range(pos..end, "");
            }
            OwnedDelta::Insert(s) => {
                if pos > buf.len() || !buf.is_char_boundary(pos) {
                    return false;
                }
                buf.insert_str(pos, s);
                pos += s.len();
            }
        }
    }
    true
}

#[cfg(test)]
mod replay_tests {
    use super::*;
    use yrs::{Doc, GetString, Text, Transact};

    fn run(start: &str, delta: &[OwnedDelta], full: &str) -> (bool, String) {
        let doc = Doc::new();
        let t = doc.get_or_insert_text("t");
        let mut txn = doc.transact_mut();
        t.insert(&mut txn, 0, start);
        let fit = replay_text_delta(&mut txn, &t, delta, full);
        (fit, t.get_string(&txn))
    }

    #[test]
    fn multibyte_text_replays_in_byte_offsets() {
        let (fit, s) = run("h\u{e9}llo", &[OwnedDelta::Retain(3), OwnedDelta::Insert("X".into())], "h\u{e9}Xllo");
        assert!(fit);
        assert_eq!(s, "h\u{e9}Xllo");
        let (fit, s) = run("a\u{1f600}b", &[OwnedDelta::Retain(5), OwnedDelta::Delete(1)], "a\u{1f600}");
        assert!(fit);
        assert_eq!(s, "a\u{1f600}");
    }

    #[test]
    fn out_of_range_delta_falls_back_to_full_text() {
        let (fit, s) = run("short", &[OwnedDelta::Retain(40), OwnedDelta::Insert("x".into())], "the full text \u{1f600}");
        assert!(!fit);
        assert_eq!(s, "the full text \u{1f600}");
        let (fit, s) = run("", &[OwnedDelta::Delete(3)], "\u{fc}n\u{ef}code");
        assert!(!fit);
        assert_eq!(s, "\u{fc}n\u{ef}code");
    }

    #[test]
    fn mid_codepoint_position_falls_back_without_panicking() {
        let (fit, s) = run("\u{e9}", &[OwnedDelta::Retain(1), OwnedDelta::Insert("x".into())], "fallback");
        assert!(!fit);
        assert_eq!(s, "fallback");
        let (fit, s) = run("a\u{e9}b", &[OwnedDelta::Retain(2), OwnedDelta::Delete(1)], "trimmed");
        assert!(!fit);
        assert_eq!(s, "trimmed");
    }
}
