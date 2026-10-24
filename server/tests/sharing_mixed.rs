//! Whole-vault share with a read-only ticket that keeps some paths writable: the owner shares
//! everything, bob can read all of it and write only `Inbox/` and `root.md`.

mod common;

use common::*;
use serde_json::json;

fn readonly(p: &std::path::Path) -> bool {
    std::fs::metadata(p).map(|m| m.permissions().readonly()).unwrap_or(false)
}

#[test]
fn whole_vault_read_only_with_writable_paths() {
    let dir = tmp("mixed");
    let mut server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    let bob_tok = server.add_user("bob");

    let a = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    a.write("Notes/a.md", "a-content\n");
    a.write("Inbox/i.md", "i-content\n");
    a.write("root.md", "root-content\n");
    let b = Client::start(&server, &bob_tok, "vb", &dir.join("B"), "B");
    eventually!(15, a.proc.log_contains("reconcile done") && b.proc.log_contains("reconcile done"), "clients up", a.proc, b.proc);
    eventually!(10, api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None)["shares"].as_array().is_some(), "vault registered", server.proc);

    // Validation first: writable paths need a read-only ticket and must be sane.
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "", "access": "rw", "rw_paths": ["Inbox"]})));
    assert_eq!(code, 400, "{body}");
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "", "access": "ro", "rw_paths": ["../x"]})));
    assert_eq!(code, 400, "{body}");
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Notes/a.md", "access": "ro", "rw_paths": ["Inbox"]})));
    assert_eq!(code, 400, "single-note shares take no writable paths: {body}");

    // Share the whole vault: read-only, except Inbox/ (folder) and root.md (an existing note).
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "", "access": "ro", "rw_paths": ["Inbox", "root.md"]})));
    let share_id = r["share"]["id"].as_str().expect("share id").to_string();
    let ticket = r["ticket"].as_str().expect("ticket").to_string();
    assert_eq!(r["share"]["prefix"], "", "{r}");
    assert_eq!(r["share"]["name"], "va", "whole-vault share is named after the vault: {r}");
    let inspect = api_json(&server, &owner, "POST", "/api/tickets/inspect", Some(json!({"ticket": ticket})));
    assert_eq!(inspect["access"], "ro", "{inspect}");
    assert_eq!(inspect["rw_paths"], json!(["Inbox/", "root.md"]), "{inspect}");

    // The same path twice is refused; the root cannot move.
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": ""})));
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("already shared"), "{body}");
    let (code, _) = api_raw(&server, &owner, "PATCH", &format!("/api/shares/{share_id}"), Some(json!({"path": "Elsewhere"})));
    assert_eq!(code, 400);

    // Bob mounts it.
    let r = api_json(&server, &bob_tok, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": ticket, "mount_path": "Shared/V"})));
    assert_eq!(r["access"], "ro", "mount: {r}");
    assert_eq!(r["rw_paths"], json!(["Inbox/", "root.md"]), "mount: {r}");
    eventually!(15, b.exists("Shared/V/Notes/a.md") && b.exists("Shared/V/Inbox/i.md") && b.exists("Shared/V/root.md"), "bob gets the vault", b.proc, server.proc);
    let info = api_json(&server, &bob_tok, "GET", "/api/vaults/vb/sharing", None);
    assert_eq!(info["mounts"][0]["rw_paths"], json!(["Inbox/", "root.md"]), "{info}");

    // The headless client learns the mixed access within 20 s: read-only files locked on disk,
    // the writable paths left alone.
    eventually!(30, b.proc.log_contains("read-only share (except Shared/V/Inbox/, Shared/V/root.md)"), "bob learns mixed access", b.proc);
    eventually!(5, readonly(&b.path("Shared/V/Notes/a.md")), "read-only part locked", b.proc);
    assert!(!readonly(&b.path("Shared/V/Inbox/i.md")), "writable folder locked");
    assert!(!readonly(&b.path("Shared/V/root.md")), "writable note locked");

    // Writes inside the writable paths flow to the owner, including new files and blobs.
    b.append("Shared/V/Inbox/i.md", "edited-by-bob");
    eventually!(10, a.contains("Inbox/i.md", "edited-by-bob"), "owner sees bob's inbox edit", a.proc, server.proc);
    b.write("Shared/V/Inbox/new.md", "from bob\n");
    b.write_bytes("Shared/V/Inbox/bob.gif", b"GIF89a bob");
    eventually!(15, a.read("Inbox/new.md") == "from bob\n" && a.same_bytes("Inbox/bob.gif", &b.path("Shared/V/Inbox/bob.gif")), "owner gets bob's new files", a.proc, server.proc);
    b.append("Shared/V/root.md", "root-by-bob");
    eventually!(10, a.contains("root.md", "root-by-bob"), "owner sees bob's root.md edit", a.proc, server.proc);

    // Writes outside are undone: edits restored, new files rescued next to the mount.
    make_writable(&b.path("Shared/V/Notes/a.md"));
    b.append("Shared/V/Notes/a.md", "bob-in-notes");
    sleep(3.0);
    assert!(!a.contains("Notes/a.md", "bob-in-notes"), "read-only edit reached the owner");
    eventually!(5, !b.contains("Shared/V/Notes/a.md", "bob-in-notes"), "read-only edit restored", b.proc);
    b.write("Shared/V/Notes/bob.md", "bob-new\n");
    eventually!(8, b.read("Shared/V (read-only copy)/Notes/bob.md") == "bob-new\n" && !b.exists("Shared/V/Notes/bob.md"), "new file in read-only part moved aside", b.proc);
    assert!(!a.exists("Notes/bob.md"));

    // The owner's edits reach bob everywhere.
    a.append("Notes/a.md", "edited-by-owner");
    a.append("Inbox/i.md", "owner-in-inbox");
    eventually!(10, b.contains("Shared/V/Notes/a.md", "edited-by-owner") && b.contains("Shared/V/Inbox/i.md", "owner-in-inbox"), "bob sees owner", b.proc);

    // A second, plain read & write ticket for the same share still works as before.
    let rw = api_json(&server, &owner, "POST", &format!("/api/shares/{share_id}/tickets"), Some(json!({"label": "rw", "access": "rw"})));
    assert_eq!(rw["rw_paths"], json!([]), "{rw}");
    let listing = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    let grants = listing["shares"][0]["grants"].as_array().unwrap();
    assert_eq!(grants[0]["rw_paths"], json!(["Inbox/", "root.md"]), "{listing}");
    assert_eq!(grants[1]["rw_paths"], json!([]), "{listing}");
    assert_eq!(listing["shares"][0]["path"], "", "{listing}");

    // A folder inside the whole-vault share can be shared on its own, to someone else, with its
    // own access: carol gets Notes/ read & write. Her edits reach the owner and, through the
    // vault, bob's whole-vault mount; bob still cannot write there.
    let carol_tok = server.add_user("carol");
    let c = Client::start(&server, &carol_tok, "vc", &dir.join("C"), "C");
    eventually!(10, c.proc.log_contains("reconcile done"), "carol up", c.proc);
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Notes", "name": "Notes"})));
    let notes_id = r["share"]["id"].as_str().expect("share id").to_string();
    let notes_ticket = r["ticket"].as_str().expect("ticket").to_string();
    let r = api_json(&server, &carol_tok, "POST", "/api/vaults/vc/mounts", Some(json!({"ticket": notes_ticket, "mount_path": "Shared/N"})));
    assert_eq!(r["access"], "rw", "carol mount: {r}");
    eventually!(15, c.contains("Shared/N/a.md", "edited-by-owner") && !c.exists("Shared/N/Inbox"), "carol gets Notes only", c.proc, server.proc);
    c.append("Shared/N/a.md", "carol-in-notes");
    eventually!(10, a.contains("Notes/a.md", "carol-in-notes"), "owner sees carol", a.proc, server.proc);
    eventually!(10, b.contains("Shared/V/Notes/a.md", "carol-in-notes"), "bob sees carol through the vault", b.proc, server.proc);
    c.write("Shared/N/carol.md", "carol-new\n");
    eventually!(10, a.read("Notes/carol.md") == "carol-new\n" && b.read("Shared/V/Notes/carol.md") == "carol-new\n", "carol's new note reaches both", a.proc, b.proc, server.proc);
    make_writable(&b.path("Shared/V/Notes/a.md"));
    b.append("Shared/V/Notes/a.md", "bob-still-ro");
    sleep(3.0);
    assert!(!a.contains("Notes/a.md", "bob-still-ro") && !c.contains("Shared/N/a.md", "bob-still-ro"), "bob's read-only edit leaked");
    // Moving the inner share is fine; a share containing other shares cannot be moved; a mount
    // may never overlap a share.
    let (code, body) = api_raw(&server, &owner, "PATCH", &format!("/api/shares/{notes_id}"), Some(json!({"path": "Archive/Notes"})));
    assert_eq!(code, 200, "{body}");
    eventually!(15, a.exists("Archive/Notes/a.md") && b.exists("Shared/V/Archive/Notes/a.md") && c.exists("Shared/N/a.md"), "inner share moved for everyone", a.proc, b.proc, c.proc);
    let listing = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(listing["shares"].as_array().unwrap().len(), 2, "{listing}");
    let (code, body) = api_raw(&server, &carol_tok, "POST", "/api/vaults/vc/mounts", Some(json!({"ticket": ticket, "mount_path": "Shared/N/inner"})));
    assert_eq!(code, 409, "{body}");

    // Restart: the bridges come back with the same access.
    server.restart(&[]);
    b.append("Shared/V/Inbox/i.md", "after-restart");
    eventually!(30, a.contains("Inbox/i.md", "after-restart"), "inbox edit flows after restart", a.proc, server.proc);
    c.append("Shared/N/a.md", "carol-after-restart");
    eventually!(15, a.contains("Archive/Notes/a.md", "carol-after-restart") && b.contains("Shared/V/Archive/Notes/a.md", "carol-after-restart"), "nested share flows after restart", a.proc, b.proc, server.proc);
    make_writable(&b.path("Shared/V/Archive/Notes/a.md"));
    b.append("Shared/V/Archive/Notes/a.md", "notes-after-restart");
    sleep(3.0);
    assert!(!a.contains("Archive/Notes/a.md", "notes-after-restart"), "read-only edit reached the owner after restart");
    eventually!(5, !b.contains("Shared/V/Archive/Notes/a.md", "notes-after-restart"), "read-only edit restored after restart", b.proc);

    assert!(server.errors().is_empty(), "server errors: {:?}", server.errors());
}
