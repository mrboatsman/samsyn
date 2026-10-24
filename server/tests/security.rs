//! Security regression tests. Each test pins one property an attacker would try to break;
//! if a change makes one fail, it is a security regression, not a flaky test. Structure:
//! anonymous → authenticated-but-wrong-user → ticket holder → malicious collaborator.
//! The scenarios in sharing_*.rs cover functionality; this file covers what must *not* work.

mod common;

use common::*;
use serde_json::json;
use std::time::Duration;

/// Connect a WebSocket, retrying briefly: under `cargo test`'s parallelism each test spins its
/// own runtime, so a connect can transiently fail even though the server is up. Not a product
/// concern; keeps the suite from flaking.
async fn ws_connect(url: &str) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    for attempt in 0..30 {
        match tokio_tungstenite::connect_async(url).await {
            Ok((ws, _)) => return ws,
            Err(_) if attempt < 29 => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => panic!("ws connect to {url} failed: {e}"),
        }
    }
    unreachable!()
}


fn hex64(c: char) -> String {
    std::iter::repeat_n(c, 64).collect()
}

// ------------------------------------------------------------------ anonymous attacker

#[test]
fn every_authenticated_route_rejects_anonymous_and_bad_tokens() {
    let dir = tmp("sec-anon");
    let server = Server::start(&dir, "s", &[]);
    let routes: &[(&str, &str, Option<serde_json::Value>)] = &[
        ("GET", "/me", None),
        ("GET", "/api/vaults/va/sharing", None),
        ("POST", "/api/vaults/va/shares", Some(json!({"path": "x"}))),
        ("POST", "/api/vaults/va/mounts", Some(json!({"ticket": "x"}))),
        ("DELETE", "/api/shares/abc", None),
        ("PATCH", "/api/shares/abc", Some(json!({"path": "x"}))),
        ("POST", "/api/shares/abc/tickets", None),
        ("DELETE", "/api/shares/abc/grants/x", None),
        ("POST", "/api/shares/abc/grants/x/restore", None),
        ("DELETE", "/api/mounts/abc", None),
        ("PATCH", "/api/mounts/abc", Some(json!({"path": "x"}))),
        ("POST", "/api/tickets/inspect", Some(json!({"ticket": "x"}))),
        ("GET", "/api/me/tokens", None),
        ("POST", "/api/me/tokens", None),
        ("DELETE", "/api/me/tokens/x", None),
        ("GET", &format!("/blobs/va/{}", hex64('a')), None),
        ("PUT", &format!("/blobs/va/{}", hex64('a')), None),
    ];
    // After 30 failures from one address the budget answers 429 instead; both mean "no".
    let mut attempts = 0;
    for (method, path, body) in routes {
        for token in ["", "rp_00000000000000000000000000000000", "garbage"] {
            let (code, _) = api_raw(&server, token, method, path, body.clone());
            let expected = if attempts < 30 { 401 } else { 429 };
            assert_eq!(code, expected, "{method} {path} with token {token:?} (attempt {attempts})");
            attempts += 1;
        }
    }
    // The WebSocket upgrade is refused before the upgrade when the token is wrong (budget
    // already exhausted above, so 429 here).
    let (code, _) = api_raw(&server, "", "GET", "/ws/va?token=rp_00000000000000000000000000000000", None);
    assert!(code == 401 || code == 429);
    // Public routes stay public but reveal nothing sensitive.
    let (code, body) = api_raw(&server, "", "GET", "/health", None);
    assert_eq!(code, 200);
    assert!(!body.contains("rp_"));
    assert!(server.errors().is_empty(), "{:?}", server.errors());
}

#[test]
fn tokens_and_secrets_never_appear_in_server_logs() {
    let dir = tmp("sec-logs");
    let server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    // Use the token in every way a client would: header, query string, WebSocket query.
    api_raw(&server, &owner, "GET", "/me", None);
    api_raw(&server, "", "GET", &format!("/me?token={owner}"), None);
    api_raw(&server, "", "GET", &format!("/ws/va?token={owner}"), None);
    let c = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    c.write("a.md", "x\n");
    eventually!(10, c.proc.log_contains("reconcile done"), "client up", c.proc);
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "a.md", "name": "A"})));
    let ticket = r["ticket"].as_str().unwrap().to_string();
    let secret = ticket_secret(&ticket);
    api_raw(&server, "", "GET", &format!("/guest/shares/{}?secret={secret}", r["share"]["id"].as_str().unwrap()), None);
    sleep(0.5);
    let log = server.proc.log_text();
    // The first-start token is printed exactly once, deliberately; nothing else may leak it.
    assert_eq!(log.matches(&owner).count(), 1, "owner token appears more than once in the log");
    assert!(!log.contains(&secret), "ticket secret in the log");
    assert!(!log.contains("token="), "query string logged");
    assert!(!log.contains("secret="), "query string logged");
}

#[test]
fn malformed_input_is_rejected_not_crashing() {
    let dir = tmp("sec-malformed");
    let server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    for name in ["../x", ".hidden", "a/b", "", &"a".repeat(65), "a b", "x\u{0}"] {
        let (code, _) = api_raw(&server, &owner, "GET", &format!("/api/vaults/{}/sharing", urlenc(name)), None);
        assert!(code == 400 || code == 404, "vault name {name:?} -> {code}");
    }
    for hash in ["abc", &hex64('g'), &"a".repeat(63), "../../etc/passwd"] {
        let (code, _) = api_raw(&server, &owner, "GET", &format!("/blobs/va/{}", urlenc(hash)), None);
        assert!(code == 400 || code == 404, "hash {hash:?} -> {code}");
    }
    // Hash mismatch on upload.
    let (code, _) = api_raw_bytes(&server, &owner, "PUT", &format!("/blobs/va/{}", hex64('a')), b"not matching");
    assert_eq!(code, 400);
    // Share paths that could escape a vault folder on a client.
    let c = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    c.write("Notes/a.md", "x\n");
    eventually!(10, c.proc.log_contains("reconcile done"), "client up", c.proc);
    // (A leading slash is merely normalised away; it cannot escape anything.)
    for path in ["../Notes", "Notes/../..", "Notes//a.md", "Notes\u{0}"] {
        let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": path})));
        assert!(code == 400 || code == 404, "share path {path:?} -> {code} {body}");
    }
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Notes", "name": "N"})));
    let ticket = r["ticket"].as_str().unwrap().to_string();
    let bob = server.add_user("bob");
    Client::start(&server, &bob, "vb", &dir.join("B"), "B");
    sleep(2.0);
    for mp in ["../x", ".obsidian/plugins/evil", "a/../../x", "a//b"] {
        let (code, body) = api_raw(&server, &bob, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": ticket, "mount_path": mp})));
        assert_eq!(code, 400, "mount path {mp:?} -> {code} {body}");
    }
    // Garbage tickets.
    for t in ["", "samsyn", "samsyn!!!", &format!("samsyn{}", "a".repeat(5000)), "x"] {
        let (code, _) = api_raw(&server, &bob, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": t})));
        assert_eq!(code, 400, "ticket {t:?}");
    }
    // Oversized JSON body is refused by the framework, not by running out of memory.
    let big = "x".repeat(3 * 1024 * 1024);
    let (code, _) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": big})));
    assert!(code == 413 || code == 400, "3 MB json -> {code}");
    assert!(server.errors().is_empty(), "{:?}", server.errors());
}

#[test]
fn websocket_survives_garbage_and_enforces_size_limit() {
    let dir = tmp("sec-ws");
    let server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    let url = format!("ws://127.0.0.1:{}/ws/va?token={owner}", server.port);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        let mut ws = ws_connect(&url).await;
        // Random bytes, truncated varints, a huge declared length.
        for payload in [vec![0xffu8; 10], vec![0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0x0f], vec![1, 2, 3], vec![]] {
            ws.send(Message::Binary(payload.into())).await.expect("send");
        }
        // Still alive: the server answers a proper sync step 1.
        let hello = {
            use yrs::sync::{Message as M, SyncMessage};
            use yrs::updates::encoder::Encode;
            use yrs::{ReadTxn, Transact};
            let doc = yrs::Doc::new();
            let sv = doc.transact().state_vector();
            M::Sync(SyncMessage::SyncStep1(sv)).encode_v1()
        };
        ws.send(Message::Binary(hello.into())).await.expect("send");
        let got = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(m)) = ws.next().await {
                if let Message::Binary(_) = m {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(got, "server stopped answering after garbage");
        // A frame above the limit closes the connection instead of being buffered.
        let mut ws2 = ws_connect(&url).await;
        let huge = vec![0u8; 300 * 1024 * 1024];
        let _ = ws2.send(Message::Binary(huge.into())).await;
        let closed = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match ws2.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(closed, "oversized message was accepted");
    });
    let (code, _) = api_raw(&server, "", "GET", "/health", None);
    assert_eq!(code, 200, "server died");
    assert!(server.errors().is_empty(), "{:?}", server.errors());
}

// ------------------------------------------------------------------ wrong user

#[test]
fn users_cannot_reach_each_others_vaults_shares_or_mounts() {
    let dir = tmp("sec-scope");
    let server = Server::start(&dir, "s", &[]);
    let alice = server.owner_token.clone();
    let bob = server.add_user("bob");
    let a = Client::start(&server, &alice, "notes", &dir.join("A"), "A");
    a.write("Projects/X/a.md", "alice's\n");
    a.write_bytes("Projects/X/p.png", b"\x89PNG alice");
    eventually!(10, a.proc.log_contains("reconcile done"), "alice up", a.proc);
    let meta = api_json(&server, &alice, "GET", "/api/vaults/notes/sharing", None);
    assert!(meta["shares"].as_array().unwrap().is_empty());

    // Same vault name as bob resolves to bob's own (empty) vault, never alice's.
    let (code, body) = api_raw(&server, &bob, "POST", "/api/vaults/notes/shares", Some(json!({"path": "Projects/X"})));
    assert_eq!(code, 404, "bob shared alice's folder: {body}");
    // Alice's blob by hash is not readable by bob even though the hash is known.
    let hash = hex::encode(sha2::Sha256::digest(b"\x89PNG alice"));
    let (code, _) = api_raw(&server, &alice, "GET", &format!("/blobs/notes/{hash}"), None);
    assert_eq!(code, 200);
    let (code, _) = api_raw(&server, &bob, "GET", &format!("/blobs/notes/{hash}"), None);
    assert_eq!(code, 404, "bob read alice's blob");

    // Alice's share and ticket; bob cannot manage them.
    let r = api_json(&server, &alice, "POST", "/api/vaults/notes/shares", Some(json!({"path": "Projects/X", "name": "X"})));
    let share_id = r["share"]["id"].as_str().unwrap().to_string();
    for (method, path, body) in [
        ("DELETE", format!("/api/shares/{share_id}"), None),
        ("PATCH", format!("/api/shares/{share_id}"), Some(json!({"path": "Y"}))),
        ("POST", format!("/api/shares/{share_id}/tickets"), None),
        ("DELETE", format!("/api/shares/{share_id}/grants/ticket-1"), None),
        ("POST", format!("/api/shares/{share_id}/grants/ticket-1/restore"), None),
    ] {
        let (code, _) = api_raw(&server, &bob, method, &path, body);
        assert_eq!(code, 404, "bob could {method} {path}");
    }
    // Bob mounts with the ticket (legitimate), alice cannot touch bob's mount.
    let bc = Client::start(&server, &bob, "notes", &dir.join("B"), "B");
    eventually!(10, bc.proc.log_contains("reconcile done"), "bob up", bc.proc);
    let m = api_json(&server, &bob, "POST", "/api/vaults/notes/mounts", Some(json!({"ticket": r["ticket"]})));
    assert_eq!(m["access"], "rw", "{m}");
    let mid = api_json(&server, &bob, "GET", "/api/vaults/notes/sharing", None)["mounts"][0]["id"].as_str().unwrap().to_string();
    for (method, body) in [("DELETE", None), ("PATCH", Some(json!({"path": "Z"})))] {
        let (code, _) = api_raw(&server, &alice, method, &format!("/api/mounts/{mid}"), body);
        assert_eq!(code, 404, "alice could {method} bob's mount");
    }
    // A ticket mounted into a vault only exposes the shared subtree, never alice's other files.
    a.write("private.md", "secret\n");
    eventually!(10, bc.exists("Shared/X/a.md"), "bob mounted", bc.proc);
    sleep(2.0);
    assert!(!bc.exists("private.md") && !bc.exists("Shared/X/private.md"));
    // Tokens are per user: revoking a label only touches the caller's own tokens. Both users
    // have a token labelled "initial". Bob is using his, so revoking it is refused until he
    // insists; then it kills bob's, not alice's.
    let (code, body) = api_raw(&server, &bob, "DELETE", "/api/me/tokens/initial", None);
    assert_eq!(code, 409, "{body}");
    let (code, _) = api_raw(&server, &bob, "GET", "/me", None);
    assert_eq!(code, 200, "bob's token revoked despite the guard");
    let (code, _) = api_raw(&server, &bob, "DELETE", "/api/me/tokens/initial?force=true", None);
    assert_eq!(code, 204);
    let (code, _) = api_raw(&server, &alice, "GET", "/me", None);
    assert_eq!(code, 200, "alice's token revoked by bob");
    let (code, _) = api_raw(&server, &bob, "GET", "/me", None);
    assert_eq!(code, 401, "bob's own token should be gone");
    let (code, _) = api_raw(&server, &alice, "DELETE", "/api/me/tokens/does-not-exist", None);
    assert_eq!(code, 404);
    assert!(server.errors().is_empty(), "{:?}", server.errors());
}

// ------------------------------------------------------------------ ticket holder

#[test]
fn guest_routes_do_not_leak_and_rate_limit_without_trusting_forged_headers() {
    let dir = tmp("sec-guest");
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let server = Server::start_at(dir.join("data"), dir.join("server.log"), port, &["--no-p2p", "--public-url", &url]);
    let owner = server.owner_token.clone();
    let c = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    c.write("S/a.md", "x\n");
    eventually!(10, c.proc.log_contains("reconcile done"), "client up", c.proc);
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "S", "name": "S"})));
    let share_id = r["share"]["id"].as_str().unwrap().to_string();
    let ro = api_json(&server, &owner, "POST", &format!("/api/shares/{share_id}/tickets"), Some(json!({"label": "ro", "access": "ro"})));
    let ro_secret = ticket_secret(ro["ticket"].as_str().unwrap());

    // Wrong secret, unknown share, wrong length: identical answers.
    let bad = hex64('b');
    let mut bodies = std::collections::HashSet::new();
    for path in [format!("/guest/shares/{share_id}?secret={bad}"), format!("/guest/shares/zzz?secret={bad}"), format!("/guest/shares/{share_id}?secret=short")] {
        let (code, body) = api_raw(&server, "", "GET", &path, None);
        assert_eq!(code, 403, "{path}");
        bodies.insert(body);
    }
    assert_eq!(bodies.len(), 1, "guest failures are distinguishable: {bodies:?}");
    // Forged X-Forwarded-For does not reset the limiter (server runs without --trust-proxy).
    let client = reqwest::blocking::Client::new();
    for i in 0..7 {
        let _ = client.get(format!("{url}/guest/shares/{share_id}?secret={bad}")).header("x-forwarded-for", format!("10.0.0.{i}")).send();
    }
    let res = client.get(format!("{url}/guest/shares/{share_id}?secret={bad}")).header("x-forwarded-for", "10.9.9.9").send().unwrap();
    assert_eq!(res.status().as_u16(), 429, "limiter bypassed with a forged X-Forwarded-For");
    // A *valid* secret from the blocked address still works: the budget slows guessing, and
    // behind a proxy one address is many guests. Wrong secrets stay blocked.
    let (code, _) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?secret={ro_secret}"), None);
    assert_eq!(code, 200, "valid secret refused because of the address's failures");
    let (code, _) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?secret={bad}"), None);
    assert_eq!(code, 429);
    assert!(!server.proc.log_contains(&bad) && !server.proc.log_contains(&ro_secret), "secret in log");
    drop(server);

    // Fresh server: read-only ticket cannot write, session is bound to its share.
    let server = Server::start_at(dir.join("data"), dir.join("server2.log"), port, &["--no-p2p", "--public-url", &url]);
    let blob = hex::encode(sha2::Sha256::digest(b"payload"));
    let (code, _) = api_raw_bytes(&server, &ro_secret, "PUT", &format!("/guest/blobs/{share_id}/{blob}"), b"payload");
    assert_eq!(code, 403, "read-only ticket uploaded a blob");
    let (code, body) = api_raw(&server, &ro_secret, "POST", &format!("/guest/session/{share_id}"), None);
    assert_eq!(code, 200, "{body}");
    let session = serde_json::from_str::<serde_json::Value>(&body).unwrap()["session"].as_str().unwrap().to_string();
    let (code, _) = api_raw(&server, &session, "GET", "/me", None);
    assert_eq!(code, 401, "a guest session is not a user token");
    let (code, _) = api_raw(&server, &session, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(code, 401);
    // Revoking the ticket kills the session immediately.
    let (code, _) = api_raw(&server, &owner, "DELETE", &format!("/api/shares/{share_id}/grants/ro"), None);
    assert_eq!(code, 204);
    let (code, _) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?session={session}"), None);
    assert_eq!(code, 403, "revoked ticket's session still valid");
    assert!(server.errors().is_empty(), "{:?}", server.errors());
}

// ------------------------------------------------------------------ malicious collaborator

/// A collaborator controls the keys in a shared document. Keys that could escape the sync
/// directory must never reach disk on the headless client, and the server must not even
/// forward them across a share.
#[test]
fn hostile_document_keys_never_escape_the_sync_directory() {
    let dir = tmp("sec-keys");
    let server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    let bob = server.add_user("bob");
    // Owner shares a folder; bob mounts it and runs a headless client on his vault.
    let a = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    a.write("Projects/X/ok.md", "fine\n");
    eventually!(10, a.proc.log_contains("reconcile done"), "owner up", a.proc);
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Projects/X", "name": "X"})));
    let ticket = r["ticket"].as_str().unwrap().to_string();
    let b = Client::start(&server, &bob, "vb", &dir.join("B"), "B");
    eventually!(10, b.proc.log_contains("reconcile done"), "bob up", b.proc);
    api_json(&server, &bob, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": ticket, "mount_path": "Shared/X"})));
    eventually!(15, b.exists("Shared/X/ok.md"), "mount works", b.proc);

    // The owner (as a hostile collaborator would) injects evil keys straight into the vault
    // document over the WebSocket, bypassing any client-side checks.
    let evil_keys = [
        "/tmp/samsyn-evil-abs.md",
        "Projects/X/../../../samsyn-evil-dotdot.md",
        "Projects/X/..\\\\samsyn-evil-backslash.md",
        "Projects/X/sub//samsyn-evil-empty.md",
        "Projects/X/\u{0}samsyn-evil-nul.md",
    ];
    let canary = "/tmp/samsyn-evil-abs.md";
    let _ = std::fs::remove_file(canary);
    let url = format!("ws://127.0.0.1:{}/ws/va?token={owner}", server.port);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        use yrs::sync::{Message as M, SyncMessage};
        use yrs::updates::encoder::Encode;
        use yrs::{Map, ReadTxn, Text, Transact};
        let mut ws = ws_connect(&url).await;
        let doc = yrs::Doc::new();
        let texts = doc.get_or_insert_map("texts");
        {
            let mut txn = doc.transact_mut();
            for k in evil_keys {
                let t = texts.insert(&mut txn, k, yrs::TextPrelim::new(""));
                t.insert(&mut txn, 0, "evil\n");
            }
            // Also a legitimate key, to prove the update itself was applied.
            let t = texts.insert(&mut txn, "Projects/X/from-ws.md", yrs::TextPrelim::new(""));
            t.insert(&mut txn, 0, "legit\n");
        }
        let update = doc.transact().encode_state_as_update_v1(&yrs::StateVector::default());
        ws.send(Message::Binary(M::Sync(SyncMessage::Update(update)).encode_v1().into())).await.expect("send");
        tokio::time::sleep(Duration::from_secs(1)).await;
    });
    eventually!(15, b.exists("Shared/X/from-ws.md"), "legitimate key arrives at bob", b.proc, server.proc);
    sleep(3.0);
    // Nothing escaped, on either client.
    assert!(!std::path::Path::new(canary).exists(), "absolute key written outside the sync dir");
    for root in [dir.join("A"), dir.join("B")] {
        for e in walkdir::WalkDir::new(&root).into_iter().flatten() {
            let p = e.path().to_string_lossy().to_string();
            assert!(!p.contains("samsyn-evil"), "evil key reached disk: {p}");
        }
    }
    for p in ["samsyn-evil-dotdot.md", "samsyn-evil-backslash.md"] {
        assert!(!dir.join(p).exists() && !dir.parent().unwrap().join(p).exists(), "{p} escaped");
    }
    // The server refused to forward the evil keys into bob's document at all.
    assert!(server.proc.log_contains("refusing to bridge invalid key"), "server forwarded invalid keys");
    assert!(server.errors().is_empty(), "{:?}", server.errors());
}


// ------------------------------------------------------------------ second-pass findings

/// Blob metadata and text deltas are collaborator-controlled. A malformed hash or an embed used
/// to panic inside the bridge while the server-wide apply lock was held, which poisoned it and
/// stopped all sync until restart.
#[test]
fn hostile_blob_metadata_and_deltas_do_not_stop_the_server() {
    let dir = tmp("sec-poison");
    let server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    let bob = server.add_user("bob");
    let a = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    a.write("Projects/X/ok.md", "fine\n");
    eventually!(10, a.proc.log_contains("reconcile done"), "owner up", a.proc);
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Projects/X", "name": "X"})));
    let ticket = r["ticket"].as_str().unwrap().to_string();
    let b = Client::start(&server, &bob, "vb", &dir.join("B"), "B");
    eventually!(10, b.proc.log_contains("reconcile done"), "bob up", b.proc);
    api_json(&server, &bob, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": ticket, "mount_path": "Shared/X"})));
    eventually!(15, b.exists("Shared/X/ok.md"), "mount works", b.proc);

    let url = format!("ws://127.0.0.1:{}/ws/va?token={owner}", server.port);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        use futures::SinkExt;
        use tokio_tungstenite::tungstenite::Message;
        use yrs::sync::{Message as M, SyncMessage};
        use yrs::updates::encoder::Encode;
        use yrs::{Map, ReadTxn, Text, Transact};
        let mut ws = ws_connect(&url).await;
        let doc = yrs::Doc::new();
        let texts = doc.get_or_insert_map("texts");
        let blobs = doc.get_or_insert_map("blobs");
        {
            let mut txn = doc.transact_mut();
            // Hashes that must never reach a path: too short, empty, absurdly long, non-hex.
            for (i, h) in ["zz", "", &"f".repeat(200), &"g".repeat(64)].iter().enumerate() {
                let mut m = std::collections::HashMap::new();
                m.insert("hash".to_string(), yrs::Any::String((*h).into()));
                m.insert("size".to_string(), yrs::Any::Number(yrs::Number::from(1.0)));
                m.insert("mtime".to_string(), yrs::Any::Number(yrs::Number::from(1.0)));
                blobs.insert(&mut txn, format!("Projects/X/bad{i}.png"), yrs::Any::Map(std::sync::Arc::new(m)));
            }
            // An embed in a shared note, followed by more text: positions after it are off.
            let t = texts.insert(&mut txn, "Projects/X/embed.md", yrs::TextPrelim::new("hello world"));
            t.insert_embed(&mut txn, 5, yrs::Any::Bool(true));
            t.insert(&mut txn, 7, " there");
            t.remove_range(&mut txn, 0, 2);
        }
        let update = doc.transact().encode_state_as_update_v1(&yrs::StateVector::default());
        ws.send(Message::Binary(M::Sync(SyncMessage::Update(update)).encode_v1().into())).await.expect("send");
        tokio::time::sleep(Duration::from_secs(2)).await;
    });
    // The server must still bridge ordinary edits in both directions.
    a.append("Projects/X/ok.md", "still-alive");
    eventually!(15, b.contains("Shared/X/ok.md", "still-alive"), "bridge still works after hostile metadata", b.proc, server.proc);
    b.append("Shared/X/ok.md", "and-back");
    eventually!(15, a.contains("Projects/X/ok.md", "and-back"), "bridge still works the other way", a.proc, server.proc);
    let (code, _) = api_raw(&server, "", "GET", "/health", None);
    assert_eq!(code, 200);
    assert!(!server.proc.log_contains("panicked"), "a task panicked:\n{}", server.proc.log_text());
}

#[test]
fn revoked_user_token_closes_its_live_websocket() {
    let dir = tmp("sec-revoke-token");
    let server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    let phone = api_json(&server, &owner, "POST", "/api/me/tokens", Some(json!({"label": "phone"})))["token"].as_str().unwrap().to_string();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        use futures::StreamExt;
        use tokio_tungstenite::tungstenite::Message;
        let mut ws_phone = ws_connect(&format!("ws://127.0.0.1:{}/ws/va?token={phone}", server.port)).await;
        let mut ws_owner = ws_connect(&format!("ws://127.0.0.1:{}/ws/va?token={owner}", server.port)).await;
        // Drain the hello messages.
        let _ = tokio::time::timeout(Duration::from_secs(2), ws_phone.next()).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), ws_owner.next()).await;
        // Blocking HTTP must not run on the async runtime thread.
        let (url, tok) = (format!("{}/api/me/tokens/phone", server.url()), owner.clone());
        let code = tokio::task::spawn_blocking(move || {
            reqwest::blocking::Client::new().delete(url).bearer_auth(tok).send().unwrap().status().as_u16()
        })
        .await
        .unwrap();
        assert_eq!(code, 204);
        // The re-check runs every 30 s.
        let closed = tokio::time::timeout(Duration::from_secs(45), async {
            loop {
                match ws_phone.next().await {
                    None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return true,
                    _ => {}
                }
            }
        })
        .await
        .unwrap_or(false);
        assert!(closed, "socket with a revoked token stayed open");
        // The owner's own socket is untouched.
        let still_open = tokio::time::timeout(Duration::from_secs(3), ws_owner.next()).await.is_err()
            || matches!(ws_owner.next().await, Some(Ok(Message::Binary(_))) | Some(Ok(Message::Ping(_))));
        assert!(still_open, "unrelated socket was closed too");
    });
}

#[test]
fn reading_never_creates_vaults_and_creation_is_capped() {
    let dir = tmp("sec-vaults");
    let server = Server::start(&dir, "s", &["--max-vaults-per-user", "2"]);
    let owner = server.owner_token.clone();
    let hash = hex64('a');
    // A read on a vault that does not exist is 404 and leaves nothing behind.
    let (code, _) = api_raw(&server, &owner, "GET", &format!("/blobs/ghost/{hash}"), None);
    assert_eq!(code, 404);
    let me = api_json(&server, &owner, "GET", "/me", None);
    assert_eq!(me["vaults"].as_array().unwrap().len(), 0, "{me}");
    // Writes create, up to the cap.
    let payload = b"blob";
    let h = hex::encode(sha2::Sha256::digest(payload));
    for v in ["one", "two"] {
        let (code, _) = api_raw_bytes(&server, &owner, "PUT", &format!("/blobs/{v}/{h}"), payload);
        assert_eq!(code, 201, "vault {v}");
    }
    let (code, body) = api_raw_bytes(&server, &owner, "PUT", &format!("/blobs/three/{h}"), payload);
    assert_eq!(code, 409, "third vault should be refused: {body}");
    // The WebSocket route refuses to create past the cap too (refusal happens at the upgrade).
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ws_code = rt.block_on(async {
        match tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{}/ws/three?token={owner}", server.port)).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => resp.status().as_u16(),
            Ok(_) => 101,
            Err(e) => panic!("unexpected error {e}"),
        }
    });
    assert_eq!(ws_code, 409, "WebSocket should refuse to create past the cap");
    let me = api_json(&server, &owner, "GET", "/me", None);
    assert_eq!(me["vaults"].as_array().unwrap().len(), 2, "{me}");
}


// ------------------------------------------------------------------ limits (plan batch 1)

#[test]
fn per_token_connection_cap_and_auth_failure_budget() {
    let dir = tmp("sec-caps");
    let server = Server::start(&dir, "s", &["--max-conns-per-token", "3"]);
    let owner = server.owner_token.clone();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        let url = format!("ws://127.0.0.1:{}/ws/va?token={owner}", server.port);
        let mut held = Vec::new();
        for _ in 0..3 {
            held.push(ws_connect(&url).await);
        }
        match tokio_tungstenite::connect_async(&url).await {
            Err(tokio_tungstenite::tungstenite::Error::Http(resp)) => assert_eq!(resp.status().as_u16(), 429),
            other => panic!("fourth connection should be refused, got {:?}", other.is_ok()),
        }
        drop(held.pop());
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(tokio_tungstenite::connect_async(&url).await.is_ok(), "slot not released on close");
    });
    // 30 failed user-token attempts from one address → further bad tokens get 429 for a while.
    // A good token still passes: behind a proxy one address is many clients, and one device
    // with a stale token must not lock the others out.
    for _ in 0..30 {
        api_raw(&server, "rp_00000000000000000000000000000000", "GET", "/me", None);
    }
    let (code, _) = api_raw(&server, "rp_00000000000000000000000000000000", "GET", "/me", None);
    assert_eq!(code, 429, "auth failure budget not enforced");
    let (code, _) = api_raw(&server, &owner, "GET", "/me", None);
    assert_eq!(code, 200, "a valid token was refused because of someone else's failures");
}

#[test]
fn document_size_cap_and_awareness_cap() {
    let dir = tmp("sec-doccap");
    let server = Server::start(&dir, "s", &["--max-doc-mb", "1"]);
    let owner = server.owner_token.clone();
    let a = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    a.write("small.md", "hello\n");
    eventually!(10, a.proc.log_contains("reconcile done"), "client up", a.proc);
    let url = format!("ws://127.0.0.1:{}/ws/va?token={owner}", server.port);
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async {
        use futures::{SinkExt, StreamExt};
        use tokio_tungstenite::tungstenite::Message;
        use yrs::sync::{Message as M, SyncMessage};
        use yrs::updates::encoder::Encode;
        use yrs::{Map, ReadTxn, Transact};
        let mut ws = ws_connect(&url).await;
        // A 2 MB note into a 1 MB document: refused.
        let doc = yrs::Doc::new();
        let texts = doc.get_or_insert_map("texts");
        {
            let mut txn = doc.transact_mut();
            texts.insert(&mut txn, "huge.md", yrs::TextPrelim::new("x".repeat(2 * 1024 * 1024).as_str()));
        }
        let update = doc.transact().encode_state_as_update_v1(&yrs::StateVector::default());
        ws.send(Message::Binary(M::Sync(SyncMessage::Update(update)).encode_v1().into())).await.unwrap();
        // An awareness update claiming 1 000 clients: ignored.
        let mut clients = std::collections::HashMap::new();
        for i in 1..=1000u64 {
            clients.insert(yrs::block::ClientID::new(i), yrs::sync::awareness::AwarenessUpdateEntry { clock: 1, json: "{\"user\":{\"name\":\"x\"}}".into() });
        }
        let aw = yrs::sync::awareness::AwarenessUpdate { clients };
        ws.send(Message::Binary(M::Awareness(aw).encode_v1().into())).await.unwrap();
        // Then a legitimate small awareness state and a step 1, to see the server still answers.
        let mut one = std::collections::HashMap::new();
        one.insert(yrs::block::ClientID::new(4242), yrs::sync::awareness::AwarenessUpdateEntry { clock: 1, json: "{\"user\":{\"name\":\"probe\"}}".into() });
        ws.send(Message::Binary(M::Awareness(yrs::sync::awareness::AwarenessUpdate { clients: one }).encode_v1().into())).await.unwrap();
        let d2 = yrs::Doc::new();
        ws.send(Message::Binary(M::Sync(SyncMessage::SyncStep1(d2.transact().state_vector())).encode_v1().into())).await.unwrap();
        let answered = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(Ok(m)) = ws.next().await {
                if let Message::Binary(_) = m {
                    return true;
                }
            }
            false
        })
        .await
        .unwrap_or(false);
        assert!(answered);
    });
    sleep(2.0);
    assert!(!a.exists("huge.md"), "oversized update was applied");
    assert!(server.proc.log_contains("would exceed 1 MB"), "size refusal not logged");
    assert!(server.proc.log_contains("awareness update with 1000 clients ignored"), "awareness cap not enforced");
}

#[test]
fn cors_is_not_wide_open_by_default() {
    let dir = tmp("sec-cors");
    let server = Server::start(&dir, "s", &[]);
    let client = reqwest::blocking::Client::new();
    let res = client.get(format!("{}/health", server.url())).header("origin", "https://evil.example").send().unwrap();
    assert!(res.headers().get("access-control-allow-origin").is_none(), "arbitrary origin allowed");
    let res = client.get(format!("{}/health", server.url())).header("origin", "app://obsidian.md").send().unwrap();
    assert_eq!(res.headers().get("access-control-allow-origin").map(|v| v.to_str().unwrap()), Some("app://obsidian.md"));
}

#[test]
fn initial_token_file_keeps_the_token_out_of_the_log() {
    let dir = tmp("sec-tokenfile");
    let file = dir.join("owner.token");
    let server = Server::start(&dir, "s", &["--initial-token-file", file.to_str().unwrap()]);
    let token = std::fs::read_to_string(&file).unwrap().trim().to_string();
    assert!(token.starts_with("rp_"));
    assert!(!server.proc.log_contains(&token), "token still in the log");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let (code, _) = api_raw(&server, &token, "GET", "/me", None);
    assert_eq!(code, 200);
}

#[test]
fn per_user_storage_quota_refuses_uploads_past_the_cap() {
    let dir = tmp("sec-userquota");
    let server = Server::start(&dir, "s", &["--max-user-mb", "1"]);
    let owner = server.owner_token.clone();
    let big = vec![1u8; 700 * 1024];
    let h1 = hex::encode(sha2::Sha256::digest(&big));
    let (code, _) = api_raw_bytes(&server, &owner, "PUT", &format!("/blobs/va/{h1}"), &big);
    assert_eq!(code, 201);
    let big2 = vec![2u8; 700 * 1024];
    let h2 = hex::encode(sha2::Sha256::digest(&big2));
    let (code, body) = api_raw_bytes(&server, &owner, "PUT", &format!("/blobs/va/{h2}"), &big2);
    assert_eq!(code, 413, "second upload should exceed the 1 MB quota: {body}");
    // Re-uploading an existing blob is free (same hash, nothing new stored).
    let (code, _) = api_raw_bytes(&server, &owner, "PUT", &format!("/blobs/va/{h1}"), &big);
    assert_eq!(code, 201);
    let me = api_json(&server, &owner, "GET", "/me", None);
    assert!(me["usage_bytes"].as_u64().unwrap() >= 700 * 1024, "{me}");
    assert_eq!(me["quota_bytes"].as_u64().unwrap(), 1024 * 1024);
}

#[test]
fn mount_secrets_are_sealed_at_rest() {
    let dir = tmp("sec-sealed");
    let server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    let bob = server.add_user("bob");
    let a = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    a.write("S/a.md", "x\n");
    eventually!(10, a.proc.log_contains("reconcile done"), "owner up", a.proc);
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "S", "name": "S"})));
    let ticket = r["ticket"].as_str().unwrap().to_string();
    let secret = ticket_secret(&ticket);
    let b = Client::start(&server, &bob, "vb", &dir.join("B"), "B");
    eventually!(10, b.proc.log_contains("reconcile done"), "bob up", b.proc);
    let m = api_json(&server, &bob, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": ticket})));
    assert_eq!(m["access"], "rw", "{m}");
    // The registry on disk never holds the plaintext secret (WAL mode: the row may still be
    // in the -wal file, so read both).
    let mut db = std::fs::read(server.data.join("registry.db")).unwrap();
    db.extend(std::fs::read(server.data.join("registry.db-wal")).unwrap_or_default());
    let hay = String::from_utf8_lossy(&db);
    assert!(!hay.contains(&secret), "mount secret stored in plaintext");
    assert!(hay.contains("enc1:"), "no sealed secret found");
    // And the mount still works after a restart (the key is derived from identity.key).
    drop(a);
    let mut server = server;
    server.restart(&[]);
    let a = Client::start(&server, &owner, "va", &dir.join("A"), "A2");
    eventually!(15, a.proc.log_contains("reconcile done"), "owner back", a.proc);
    a.append("S/a.md", "after-restart");
    eventually!(20, b.contains("Shared/S/a.md", "after-restart"), "mount works after restart with sealed secret", b.proc, server.proc);
}

// ------------------------------------------------------------------ helpers

fn urlenc(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn api_raw_bytes(server: &Server, token: &str, method: &str, path: &str, body: &[u8]) -> (u16, String) {
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(40)).build().unwrap();
    let url = format!("{}{}", server.url(), path);
    let req = match method {
        "PUT" => client.put(&url),
        "POST" => client.post(&url),
        m => panic!("method {m}"),
    };
    let mut req = req.body(body.to_vec());
    if !token.is_empty() {
        req = req.bearer_auth(token);
    }
    let res = req.send().expect("http");
    (res.status().as_u16(), res.text().unwrap_or_default())
}

/// Pull the secret out of a ticket the way the plugin does.
fn ticket_secret(ticket: &str) -> String {
    let body = ticket.trim().strip_prefix("samsyn").unwrap().to_ascii_uppercase();
    let bytes = data_encoding::BASE32_NOPAD.decode(body.as_bytes()).unwrap();
    let mut pos = 1;
    let mut read_str = || {
        let mut len = 0usize;
        let mut shift = 0;
        loop {
            let b = bytes[pos];
            pos += 1;
            len |= ((b & 0x7f) as usize) << shift;
            if b & 0x80 == 0 {
                break;
            }
            shift += 7;
        }
        let s = String::from_utf8(bytes[pos..pos + len].to_vec()).unwrap();
        pos += len;
        s
    };
    let _share = read_str();
    let _name = read_str();
    let _node = read_str();
    read_str()
}

use sha2::Digest;
