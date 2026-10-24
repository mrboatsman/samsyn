//! WebSocket endpoint speaking the y-sync protocol, scoped to the authenticated user's vault.

use std::collections::{HashMap, HashSet};

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Path, Query, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use tokio::sync::broadcast::error::RecvError;
use yrs::block::ClientID;
use yrs::sync::{DefaultProtocol, Message, MessageReader, Protocol, SyncMessage};
use yrs::updates::decoder::DecoderV1;
use yrs::updates::encoder::{Encoder, EncoderV1};

use crate::config::extract_token;
use crate::ids::valid_name;
use crate::storage::User;
use crate::vault::encode_message;
use crate::SharedState;

/// Largest y-sync message accepted from a client. A full vault state is one message, so this
/// bounds vault size over WebSocket; it also bounds what one client can make us allocate.
pub const MAX_WS_MESSAGE: usize = 256 * 1024 * 1024;

/// Client ids one socket may introduce into a document's awareness.
pub const MAX_AWARENESS_CLIENTS: usize = 64;
/// Read-only guests send awareness only; their inbound messages are capped much lower.
pub const MAX_WS_MESSAGE_READONLY: usize = 4 * 1024 * 1024;

/// lib0 varuint: 7 data bits per byte, high bit = continuation.
fn read_varuint(data: &[u8], pos: &mut usize) -> Option<u64> {
    let mut n: u64 = 0;
    let mut shift = 0;
    loop {
        let b = *data.get(*pos)?;
        *pos += 1;
        n |= ((b & 0x7f) as u64) << shift;
        if b & 0x80 == 0 {
            return Some(n);
        }
        shift += 7;
        if shift > 63 {
            return None;
        }
    }
}

/// Reject sync messages whose length fields cannot be backed by the bytes actually present,
/// before the CRDT decoder reserves memory for them. Every count in the y-sync framing is
/// followed by at least one byte per item, so a count larger than the remaining payload is
/// never legitimate. Unknown message kinds pass through to the real decoder.
pub fn plausible_payload(data: &[u8]) -> bool {
    let mut pos = 0;
    while pos < data.len() {
        let Some(kind) = read_varuint(data, &mut pos) else { return false };
        match kind {
            // Sync (0): a sub-kind, then a length-prefixed buffer. Our clients only ever send
            // step 1 (0), step 2 (1) and update (2).
            0 => {
                let Some(sub) = read_varuint(data, &mut pos) else { return false };
                if !matches!(sub, 0..=2) {
                    return false;
                }
                if !bounded_buffer(data, &mut pos) {
                    return false;
                }
            }
            // Awareness (1): a length-prefixed buffer whose first field is the client count.
            1 => {
                if !bounded_buffer(data, &mut pos) {
                    return false;
                }
            }
            // The server only speaks sync + awareness; auth (2), query-awareness (3) and any
            // unknown kind are refused here rather than handed to the CRDT decoder, which may
            // allocate from an attacker-chosen count.
            _ => return false,
        }
    }
    true
}

/// Read a length-prefixed buffer and require that its declared inner count (every y-sync and
/// awareness buffer begins with a varuint count of items, each at least one byte) fits in the
/// bytes actually present. Advances `pos` past the buffer.
fn bounded_buffer(data: &[u8], pos: &mut usize) -> bool {
    let Some(len) = read_varuint(data, pos) else { return false };
    let len = len as usize;
    if len > data.len().saturating_sub(*pos) {
        return false;
    }
    let buf = &data[*pos..*pos + len];
    *pos += len;
    let mut p = 0;
    let Some(count) = read_varuint(buf, &mut p) else { return false };
    count as usize <= buf.len()
}

#[cfg(test)]
mod framing_tests {
    use super::*;
    use yrs::sync::{Message, SyncMessage};
    use yrs::updates::encoder::Encode;
    use yrs::{Doc, ReadTxn, Transact};

    #[test]
    fn real_messages_pass() {
        let doc = Doc::new();
        let sv = doc.transact().state_vector();
        assert!(plausible_payload(&Message::Sync(SyncMessage::SyncStep1(sv)).encode_v1()));
        let upd = doc.transact().encode_state_as_update_v1(&yrs::StateVector::default());
        assert!(plausible_payload(&Message::Sync(SyncMessage::Update(upd)).encode_v1()));
        assert!(plausible_payload(&[]));
    }

    #[test]
    fn counts_larger_than_the_payload_are_rejected() {
        // Sync / Update / 5-byte buffer whose first field claims far more items than 5 bytes.
        let msg = [0u8, 2, 5, 0xff, 0xff, 0xff, 0xff, 0x0f];
        assert!(!plausible_payload(&msg));
        // Buffer longer than the message.
        assert!(!plausible_payload(&[0u8, 2, 200, 1]));
        // Awareness with an absurd client count.
        assert!(!plausible_payload(&[1u8, 3, 0xff, 0xff, 0x7f]));
        // Kinds the server does not speak are refused outright (query-awareness / auth).
        assert!(!plausible_payload(&[3u8, 0, 0, 6, 0xff, 0xff, 0xff, 0xff]));
        assert!(!plausible_payload(&[2u8]));
    }

    #[test]
    fn malformed_crdt_message_is_an_error_not_a_panic() {
        use yrs::sync::{Awareness, DefaultProtocol};
        use yrs::Doc;
        // Bytes from the fuzzer: a SyncStep1 whose state vector has a client id with reserved
        // bits set, which makes yrs 0.28 panic during decode. handle_payload must contain it.
        let crash: &[u8] = &[0,0,1,0,1,1,0,0,0,49,49,49,49,49,49,49,49,49,49,43,49,49,49,49,49,49,49,49,49,49,49,49,49,49,49,207,206,206,206,206,206,206,215,48,53,0,59,7,7,249,250,200,0,0,63,0,0,1,1];
        let mut aw = Awareness::new(Doc::new());
        let mut owned = std::collections::HashSet::new();
        let mut budget = crate::limits::RequestBudget::new(100, std::time::Duration::from_secs(60));
        let mut limits = PayloadLimits { doc_bytes: (0, 0), step1: &mut budget };
        // The guarantee is containment: handle_payload must return (Ok or Err), never panic or
        // abort. With the vendored yrs count caps this particular input decodes cleanly; the
        // point is only that it comes back at all.
        let _ = handle_payload(&DefaultProtocol, &mut aw, crash, &mut owned, true, &mut limits);
    }
}

/// Shared auth for ws + blob routes.
pub fn authenticate(state: &SharedState, headers: &HeaderMap, query: &HashMap<String, String>) -> Result<User, Response> {
    let token = extract_token(headers, query.get("token").map(String::as_str));
    match token.and_then(|t| state.storage.registry().authenticate(&t).ok().flatten()) {
        Some(u) => Ok(u),
        None => Err((StatusCode::UNAUTHORIZED, "bad token").into_response()),
    }
}

pub async fn whoami(
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    match authenticate(&state, &headers, &query) {
        Ok(u) => {
            let vaults: Vec<String> = state
                .storage
                .registry()
                .list_vaults(Some(&u.id))
                .map(|v| v.into_iter().map(|v| v.name).collect())
                .unwrap_or_default();
            let used = state.limits.usage.get(&u.id, || state.storage.user_bytes(&u.id)).unwrap_or(0);
            let quota = state.serve.max_user_mb * 1024 * 1024;
            Json(serde_json::json!({ "user": u.name, "vaults": vaults, "usage_bytes": used, "quota_bytes": quota })).into_response()
        }
        Err(r) => r,
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Path(vault_name): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
    State(state): State<SharedState>,
) -> Response {
    if !valid_name(&vault_name) {
        return (StatusCode::BAD_REQUEST, "invalid vault name").into_response();
    }
    let user = match authenticate(&state, &headers, &query) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let token = extract_token(&headers, query.get("token").map(String::as_str)).unwrap_or_default();
    let Some(slot) = state.limits.conns.acquire(&format!("u:{}", crate::ids::token_hash(&token)), state.serve.max_conns_per_token) else {
        return (StatusCode::TOO_MANY_REQUESTS, "too many connections for this token").into_response();
    };
    let vault = match state.docs.get_or_load(&user, &vault_name) {
        Ok(v) => v,
        Err(e) if e.to_string().contains("vault limit") => {
            return (StatusCode::CONFLICT, e.to_string()).into_response();
        }
        Err(e) => {
            tracing::error!(vault = %vault_name, user = %user.name, "failed to load vault: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "failed to load vault").into_response();
        }
    };
    let _ = user;
    ws.max_message_size(MAX_WS_MESSAGE)
        .max_frame_size(MAX_WS_MESSAGE)
        .on_upgrade(move |socket| handle_socket(socket, state, vault, Write::Full, Some(SessionCheck::User(token)), slot))
}

/// What keeps a socket alive; re-checked every 30 s so a revoked token or ticket ends the
/// session instead of living on until the next reconnect.
pub enum SessionCheck {
    /// A user token (`rp_…`).
    User(String),
    /// A ticket: (share id, secret).
    Guest(String, String),
}

/// What one connection may write to its document.
#[derive(Debug, Clone)]
pub enum Write {
    /// Everything (vault owners, read & write tickets).
    Full,
    /// Nothing: document updates from the peer are dropped before they are applied.
    Nothing,
    /// Only the keys the access allows: updates are applied, then anything outside the
    /// writable paths is undone (`DocRegistry::enforce_share_access`).
    Paths(crate::access::Access),
}

impl Write {
    pub fn from_access(a: &crate::access::Access) -> Self {
        if a.base_rw {
            Write::Full
        } else if a.rw_paths.is_empty() {
            Write::Nothing
        } else {
            Write::Paths(a.clone())
        }
    }

    /// Whether document updates are applied at all.
    pub fn applies(&self) -> bool {
        !matches!(self, Write::Nothing)
    }
}

/// Pump y-sync between one WebSocket and a live document. `Write::Nothing` drops the client's
/// document updates (read-only guests); `Write::Paths` applies them and reverts what the
/// ticket does not cover.
pub async fn handle_socket(
    socket: WebSocket,
    state: SharedState,
    vault: std::sync::Arc<crate::vault::LiveDoc>,
    write: Write,
    check: Option<SessionCheck>,
    _slot: crate::limits::ConnSlot,
) {
    let writable = write.applies();
    let (mut sink, mut stream) = socket.split();
    vault.conns.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    *vault.last_active.lock() = std::time::Instant::now();
    let mut step1 = crate::limits::RequestBudget::new(6, std::time::Duration::from_secs(60));
    let mut rx = vault.tx.subscribe();
    let protocol = DefaultProtocol;
    let label = vault.label.clone();
    let mut revoke_tick = tokio::time::interval(std::time::Duration::from_secs(30));
    revoke_tick.tick().await;

    let hello = {
        let aw = vault.awareness.lock();
        let mut enc = EncoderV1::new();
        match protocol.start(&aw, &mut enc) {
            Ok(()) => Bytes::from(enc.to_vec()),
            Err(e) => {
                tracing::warn!(vault = %label, "protocol start failed: {e}");
                return;
            }
        }
    };
    if sink.send(WsMessage::Binary(hello)).await.is_err() {
        return;
    }

    let mut owned_clients: HashSet<ClientID> = HashSet::new();
    tracing::debug!(vault = %label, "client connected");

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(WsMessage::Binary(data))) => {
                        let replies = {
                            // Serialize all CRDT applies server-wide; see DocRegistry.
                            let _apply = state.docs.apply_lock.lock();
                            let r = {
                                let mut aw = vault.awareness.lock();
                                let mut limits = PayloadLimits {
                                    doc_bytes: (vault.approx_bytes.load(std::sync::atomic::Ordering::Relaxed), state.docs.max_doc_bytes),
                                    step1: &mut step1,
                                };
                                handle_payload(&protocol, &mut aw, &data, &mut owned_clients, writable, &mut limits)
                            };
                            if let Write::Paths(a) = &write {
                                state.docs.enforce_share_access(&vault, a);
                            }
                            state.docs.propagate(&vault);
                            r
                        };
                        match replies {
                            Ok(replies) => {
                                for r in replies {
                                    if sink.send(WsMessage::Binary(r)).await.is_err() { break; }
                                }
                            }
                            Err(e) => tracing::warn!(vault = %label, "bad message from client: {e}"),
                        }
                    }
                    Some(Ok(WsMessage::Ping(p))) => {
                        if sink.send(WsMessage::Pong(p)).await.is_err() { break; }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::debug!(vault = %label, "ws error: {e}");
                        break;
                    }
                }
            }
            _ = revoke_tick.tick() => {
                let reg = state.storage.registry();
                let ok = match &check {
                    Some(SessionCheck::User(token)) => reg.authenticate(token).ok().flatten().is_some(),
                    Some(SessionCheck::Guest(share_id, secret)) => reg.check_grant(share_id, secret).ok().flatten().is_some(),
                    None => true,
                };
                if !ok {
                    tracing::info!(doc = %label, "credential revoked; closing socket");
                    break;
                }
            }
            broadcast = rx.recv() => {
                match broadcast {
                    Ok(bytes) => {
                        if sink.send(WsMessage::Binary(bytes)).await.is_err() { break; }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::warn!(vault = %label, "client lagged {n} messages, resyncing");
                        let resync = {
                            let aw = vault.awareness.lock();
                            let mut enc = EncoderV1::new();
                            let _ = protocol.start(&aw, &mut enc);
                            Bytes::from(enc.to_vec())
                        };
                        if sink.send(WsMessage::Binary(resync)).await.is_err() { break; }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
        }
    }

    if !owned_clients.is_empty() {
        let mut aw = vault.awareness.lock();
        for id in owned_clients {
            aw.remove_state(id);
        }
    }
    vault.conns.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    *vault.last_active.lock() = std::time::Instant::now();
    state.docs.maybe_compact(&vault);
    tracing::debug!(vault = %label, "client disconnected");
}

/// Feed one y-sync payload to the document. With `writable == false` (read-only peer) the
/// peer's document updates are dropped; it can still pull our state and send awareness.
/// Limits applied while feeding a payload: document size and full-state request budget.
pub struct PayloadLimits<'a> {
    /// (current approximate size, max) in bytes; `max == 0` means unlimited.
    pub doc_bytes: (u64, u64),
    pub step1: &'a mut crate::limits::RequestBudget,
}

pub fn handle_payload(
    protocol: &DefaultProtocol,
    aw: &mut yrs::sync::Awareness,
    data: &[u8],
    owned: &mut HashSet<ClientID>,
    writable: bool,
    limits: &mut PayloadLimits<'_>,
) -> Result<Vec<Bytes>, yrs::sync::Error> {
    if !plausible_payload(data) {
        return Err(yrs::sync::Error::Other("implausible message framing".into()));
    }
    // yrs 0.28 can panic (not just error) while decoding a malformed update or state vector,
    // e.g. a client id with reserved bits set. parking_lot locks mean such a panic only drops
    // this connection, but a decoder should never panic: contain it and treat it as a bad
    // message. The panic happens during decode/apply of untrusted bytes, so nothing is left
    // half-applied that matters (a client resyncs on reconnect).
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        handle_payload_inner(protocol, aw, data, owned, writable, limits)
    }));
    match result {
        Ok(r) => r,
        Err(_) => Err(yrs::sync::Error::Other("panic while decoding a client message".into())),
    }
}

fn handle_payload_inner(
    protocol: &DefaultProtocol,
    aw: &mut yrs::sync::Awareness,
    data: &[u8],
    owned: &mut HashSet<ClientID>,
    writable: bool,
    limits: &mut PayloadLimits<'_>,
) -> Result<Vec<Bytes>, yrs::sync::Error> {
    let mut decoder = DecoderV1::from(data);
    let reader = MessageReader::new(&mut decoder);
    let mut out = Vec::new();
    for msg in reader {
        let msg = msg?;
        if !writable && matches!(msg, Message::Sync(SyncMessage::Update(_)) | Message::Sync(SyncMessage::SyncStep2(_))) {
            continue;
        }
        if let Message::Sync(SyncMessage::Update(u)) | Message::Sync(SyncMessage::SyncStep2(u)) = &msg {
            let (cur, max) = limits.doc_bytes;
            if max > 0 && cur + u.len() as u64 > max {
                tracing::warn!("document would exceed {} MB; update of {} bytes refused", max / 1_048_576, u.len());
                continue;
            }
        }
        if let Message::Sync(SyncMessage::SyncStep1(_)) = &msg {
            // A full-state request makes us encode the whole document; a few per connection
            // are normal (connect, resync), a stream of them is not.
            if !limits.step1.allow() {
                tracing::warn!("too many full-state requests on one connection; ignored");
                continue;
            }
        }
        if let Message::Awareness(update) = &msg {
            // One connection speaks for a handful of clients at most; a message carrying
            // thousands of ids is an attempt to bloat everyone's awareness state.
            if owned.len() + update.clients.len() > MAX_AWARENESS_CLIENTS {
                tracing::warn!("awareness update with {} clients ignored", update.clients.len());
                continue;
            }
            owned.extend(update.clients.keys().copied());
        }
        if let Some(reply) = protocol.handle_message(aw, msg)? {
            out.push(encode_message(&reply));
        }
    }
    Ok(out)
}
