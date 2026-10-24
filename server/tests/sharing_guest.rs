//! Guest mode: owner's server runs with --public-url; two headless guests join with tickets
//! only (rw and ro). Mirrors server/scripts/guest-e2e.sh.

mod common;

use common::*;
use serde_json::json;

#[test]
fn guests_join_with_tickets_only() {
    let dir = tmp("guest");
    let port = free_port();
    let url = format!("http://127.0.0.1:{port}");
    let mut server = Server::start_at(dir.join("data"), dir.join("server.log"), port, &["--no-p2p", "--public-url", &url, "--max-share-mb", "1"]);
    let owner = server.owner_token.clone();

    let o = Client::start(&server, &owner, "va", &dir.join("O"), "O");
    o.write("Projects/X/a.md", "a-content\n");
    o.write("Projects/X/sub/b.md", "b-content\n");
    o.write("private.md", "secret\n");
    o.write_bytes("Projects/X/pic.png", b"\x89PNG fake");
    eventually!(15, o.proc.log_contains("reconcile done"), "owner up", o.proc);

    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Projects/X", "name": "X"})));
    let share_id = r["share"]["id"].as_str().expect("share id").to_string();
    let ticket = r["ticket"].as_str().expect("ticket").to_string();
    let ro = api_json(&server, &owner, "POST", &format!("/api/shares/{share_id}/tickets"), Some(json!({"label": "guest-ro", "access": "ro"})));
    let ro_ticket = ro["ticket"].as_str().unwrap().to_string();
    let inspect = api_json(&server, &owner, "POST", "/api/tickets/inspect", Some(json!({"ticket": ticket})));
    assert_eq!(inspect["url"], url, "ticket carries the public url");

    // Auth hygiene: wrong secret and unknown share answer alike; too many failures -> 429.
    let bad = "ab".repeat(32);
    let (c1, _) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?secret={bad}"), None);
    let (c2, _) = api_raw(&server, "", "GET", &format!("/guest/shares/nope?secret={bad}"), None);
    assert_eq!((c1, c2), (403, 403));
    for _ in 0..9 {
        api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?secret={bad}"), None);
    }
    let (c3, _) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?secret={bad}"), None);
    assert_eq!(c3, 429);
    assert!(!server.proc.log_contains("secret="), "secret leaked into the log");
    // The limiter is per IP and the guests below come from loopback too: restart to clear it.
    server.restart(&["--no-p2p", "--public-url", &url, "--max-share-mb", "1"]);

    // Session exchange: the secret is presented once, a session id works everywhere after.
    let secret = {
        let t = api_json(&server, &owner, "POST", "/api/tickets/inspect", Some(json!({"ticket": ticket})));
        assert_eq!(t["valid"], true);
        // The inspect route does not reveal the secret; decode the ticket like a client would.
        ticket_secret(&ticket)
    };
    let (code, body) = api_raw(&server, &secret, "POST", &format!("/guest/session/{share_id}"), None);
    assert_eq!(code, 200, "{body}");
    let session = serde_json::from_str::<serde_json::Value>(&body).unwrap()["session"].as_str().unwrap().to_string();
    assert!(session.starts_with("gs_"), "session ids are prefixed so they cannot be mistaken for secrets");
    let (code, body) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?session={session}"), None);
    assert_eq!(code, 200, "session as query param: {body}");
    let (code, _) = api_raw(&server, &session, "GET", &format!("/guest/shares/{share_id}"), None);
    assert_eq!(code, 200, "session as bearer");
    let (code, _) = api_raw(&server, "", "GET", &format!("/guest/shares/nope?session={session}"), None);
    assert_eq!(code, 403, "session bound to its share");
    let (code, _) = api_raw(&server, "", "GET", &format!("/guest/shares/{share_id}?session={session}"), None);
    assert_eq!(code, 200, "a mismatch elsewhere must not destroy the session");

    // Per-device tokens: issue one, use it, revoke it. Labels are unique, the listing marks
    // the caller's own token, and that one cannot be revoked by accident.
    let t = api_json(&server, &owner, "POST", "/api/me/tokens", Some(json!({"label": "phone"})));
    let phone = t["token"].as_str().unwrap().to_string();
    let (code, _) = api_raw(&server, &phone, "GET", "/me", None);
    assert_eq!(code, 200);
    let (code, body) = api_raw(&server, &owner, "POST", "/api/me/tokens", Some(json!({"label": "phone"})));
    assert_eq!(code, 409, "duplicate label accepted: {body}");
    let list = api_json(&server, &phone, "GET", "/api/me/tokens", None);
    let tokens = list["tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2, "{list}");
    assert!(tokens.iter().any(|x| x["label"] == "phone" && x["current"] == true), "{list}");
    assert!(tokens.iter().any(|x| x["label"] == "initial" && x["current"] == false), "{list}");
    let (code, body) = api_raw(&server, &phone, "DELETE", "/api/me/tokens/phone", None);
    assert_eq!(code, 409, "a device revoked the token it is using: {body}");
    let (code, _) = api_raw(&server, &owner, "DELETE", "/api/me/tokens/phone", None);
    assert_eq!(code, 204);
    let (code, _) = api_raw(&server, &phone, "GET", "/me", None);
    assert_eq!(code, 401, "revoked device token still works");

    let g1 = Client::start_guest(&ticket, &dir.join("G1"), "G1");
    let g2 = Client::start_guest(&ro_ticket, &dir.join("G2"), "G2");
    eventually!(20, g1.exists("a.md") && g1.exists("pic.png") && g2.exists("a.md"), "guests get files", g1.proc, g2.proc, server.proc);
    assert_eq!(g1.read("a.md"), "a-content\n");
    assert!(g1.proc.log_contains("guest session established"), "headless guest did not exchange a session");
    assert!(!server.proc.log_contains("secret="), "secret in server log");
    assert!(g1.exists("sub/b.md"));
    assert!(g1.same_bytes("pic.png", &o.path("Projects/X/pic.png")));
    assert!(!g1.exists("private.md"));
    let info = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    let seen: Vec<bool> = info["shares"][0]["grants"].as_array().unwrap().iter().map(|g| !g["last_seen_at"].is_null()).collect();
    assert_eq!(seen, vec![true, true], "last_seen_at: {info}");

    g1.append("a.md", "edited-by-guest");
    eventually!(10, o.contains("Projects/X/a.md", "edited-by-guest"), "owner sees guest edit", o.proc, server.proc);
    o.append("Projects/X/sub/b.md", "edited-by-owner");
    eventually!(10, g1.contains("sub/b.md", "edited-by-owner") && g2.contains("sub/b.md", "edited-by-owner"), "guests see owner edit", g1.proc, g2.proc);
    g1.write("new.md", "from guest\n");
    g1.write_bytes("guest.gif", b"GIF89a guest");
    eventually!(15, o.read("Projects/X/new.md") == "from guest\n" && o.same_bytes("Projects/X/guest.gif", &g1.path("guest.gif")), "owner gets guest's files", o.proc, server.proc);
    eventually!(10, g2.same_bytes("guest.gif", &g1.path("guest.gif")), "ro guest gets the blob", g2.proc);

    // Upload quota: the share allows 1 MB of blobs; a 2 MB file from the rw guest is refused.
    g1.write_bytes("big.bin", &vec![7u8; 2 * 1024 * 1024]);
    eventually!(15, g1.proc.log_contains("413"), "quota refusal logged (HTTP 413)", g1.proc, server.proc);
    sleep(2.0);
    assert!(!o.exists("Projects/X/big.bin"), "quota-exceeding blob reached the owner");

    // Read-only guest.
    eventually!(30, std::fs::metadata(g2.path("a.md")).map(|m| m.permissions().readonly()).unwrap_or(false), "ro files read-only on disk", g2.proc);
    make_writable(&g2.path("a.md"));
    g2.append("a.md", "ro-edit");
    sleep(3.0);
    assert!(!o.contains("Projects/X/a.md", "ro-edit"));
    eventually!(5, !g2.contains("a.md", "ro-edit"), "ro edit restored", g2.proc);
    // The restore itself fires a watcher event; it must not be mistaken for another edit.
    sleep(3.0);
    assert_eq!(g2.proc.log_count("restoring the server's version"), 1, "ro restore looped on its own write");
    g2.write("mine.md", "mine\n");
    eventually!(8, g2.read("read-only copy/mine.md") == "mine\n" && !g2.exists("mine.md"), "ro guest's new file moved aside", g2.proc);
    assert!(!g2.proc.log_contains("conflict"), "ro restore made a conflict copy");

    // A read-only guest with a writable folder: `sub/` flows, the rest is locked.
    let mixed = api_json(&server, &owner, "POST", &format!("/api/shares/{share_id}/tickets"), Some(json!({"label": "guest-mixed", "access": "ro", "rw_paths": ["sub"]})));
    let mixed_ticket = mixed["ticket"].as_str().unwrap().to_string();
    assert_eq!(mixed["rw_paths"], json!(["sub/"]), "{mixed}");
    let msecret = ticket_secret(&mixed_ticket);
    let (code, body) = api_raw(&server, &msecret, "GET", &format!("/guest/shares/{share_id}"), None);
    assert_eq!(code, 200, "{body}");
    let ginfo: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(ginfo["access"], "ro");
    assert_eq!(ginfo["rw_paths"], json!(["sub/"]), "{ginfo}");
    let g3 = Client::start_guest(&mixed_ticket, &dir.join("G3"), "G3");
    eventually!(20, g3.exists("a.md") && g3.exists("sub/b.md"), "mixed guest gets files", g3.proc, server.proc);
    eventually!(30, g3.proc.log_contains("read-only (except sub/)"), "mixed guest learns its writable path", g3.proc);
    eventually!(5, readonly(&g3.path("a.md")), "read-only part locked", g3.proc);
    assert!(!readonly(&g3.path("sub/b.md")), "writable folder locked");
    g3.append("sub/b.md", "mixed-guest-edit");
    eventually!(10, o.contains("Projects/X/sub/b.md", "mixed-guest-edit"), "owner sees the mixed guest's edit", o.proc, server.proc);
    g3.write_bytes("sub/g3.gif", b"GIF89a g3");
    eventually!(15, o.same_bytes("Projects/X/sub/g3.gif", &g3.path("sub/g3.gif")), "mixed guest can upload blobs", o.proc, server.proc);
    make_writable(&g3.path("a.md"));
    g3.append("a.md", "mixed-ro-edit");
    sleep(3.0);
    assert!(!o.contains("Projects/X/a.md", "mixed-ro-edit"), "read-only edit from the mixed guest reached the owner");
    eventually!(5, !g3.contains("a.md", "mixed-ro-edit"), "mixed guest's read-only edit restored", g3.proc);

    // Revoke the ro ticket: the guest is cut off and exits; the rw guest continues.
    let (code, _) = api_raw(&server, &owner, "DELETE", &format!("/api/shares/{share_id}/grants/guest-ro"), None);
    assert_eq!(code, 204);
    eventually!(45, g2.proc.log_contains("refused the connection"), "ro guest cut off", g2.proc, server.proc);
    o.append("Projects/X/a.md", "after-revoke");
    eventually!(10, g1.contains("a.md", "after-revoke"), "rw guest still receives", g1.proc);
    sleep(1.0);
    assert!(!g2.contains("a.md", "after-revoke"));

    assert!(server.errors().is_empty(), "server errors: {:?}", server.errors());
}

fn readonly(p: &std::path::Path) -> bool {
    std::fs::metadata(p).map(|m| m.permissions().readonly()).unwrap_or(false)
}

/// Pull the secret out of a ticket the way the plugin does (base32, postcard: version byte, then
/// length-prefixed strings share_id, name, node_id, secret).
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
