//! Same-server sharing: owner shares Projects/X, bob mounts it read & write, carol read-only.
//! Headless clients stand in for plugins. Mirrors server/scripts/share-e2e.sh.

mod common;

use common::*;
use serde_json::json;

#[test]
fn share_mount_edit_revoke_restart() {
    let dir = tmp("same-server");
    let mut server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    let bob_tok = server.add_user("bob");
    let carol_tok = server.add_user("carol");

    let a = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    a.write("Projects/X/a.md", "a-content\n");
    a.write("Projects/X/sub/b.md", "b-content\n");
    a.write("private.md", "secret\n");
    a.write_bytes("Projects/X/pic.png", b"\x89PNG fake");
    let b = Client::start(&server, &bob_tok, "vb", &dir.join("B"), "B");
    eventually!(15, a.proc.log_contains("reconcile done") && b.proc.log_contains("reconcile done"), "clients up", a.proc, b.proc);

    // Create share.
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Projects/X", "name": "X"})));
    let share_id = r["share"]["id"].as_str().expect("share id").to_string();
    let ticket = r["ticket"].as_str().expect("ticket").to_string();

    // The same path twice is refused (the check and the insert are serialized); a share may
    // nest inside another one, that is covered by sharing_mixed.
    let (c1, _) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Projects/X", "name": "P"})));
    assert_eq!(c1, 409);

    // Bob mounts.
    let r = api_json(&server, &bob_tok, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": ticket, "mount_path": "Shared/X"})));
    assert_eq!(r["access"], "rw", "mount: {r}");
    eventually!(15, b.exists("Shared/X/a.md") && b.exists("Shared/X/pic.png"), "bob gets files", b.proc, server.proc);
    assert_eq!(b.read("Shared/X/a.md"), "a-content\n");
    assert!(b.exists("Shared/X/sub/b.md"));
    assert!(!b.exists("private.md"), "private file leaked");
    assert!(b.same_bytes("Shared/X/pic.png", &a.path("Projects/X/pic.png")), "blob");

    // Both directions, concurrent.
    b.append("Shared/X/a.md", "edited-by-bob");
    eventually!(10, a.contains("Projects/X/a.md", "edited-by-bob"), "owner sees bob", a.proc, server.proc);
    a.append("Projects/X/sub/b.md", "edited-by-owner");
    eventually!(10, b.contains("Shared/X/sub/b.md", "edited-by-owner"), "bob sees owner", b.proc);
    a.write("Projects/X/a.md", &format!("TOP\n{}", a.read("Projects/X/a.md")));
    b.append("Shared/X/a.md", "BOTTOM");
    eventually!(
        10,
        a.read("Projects/X/a.md") == b.read("Shared/X/a.md") && a.contains("Projects/X/a.md", "TOP") && a.contains("Projects/X/a.md", "BOTTOM"),
        "concurrent edits merge",
        a.proc,
        b.proc
    );

    // New note and blob from bob; delete from owner.
    b.write("Shared/X/new.md", "from bob\n");
    b.write_bytes("Shared/X/bob.gif", b"GIF89a bob");
    eventually!(15, a.read("Projects/X/new.md") == "from bob\n" && a.same_bytes("Projects/X/bob.gif", &b.path("Shared/X/bob.gif")), "owner gets bob's files", a.proc, server.proc);
    a.remove("Projects/X/sub/b.md");
    eventually!(10, !b.exists("Shared/X/sub/b.md"), "delete propagates", b.proc);

    // Carol, read-only.
    let ro = api_json(&server, &owner, "POST", &format!("/api/shares/{share_id}/tickets"), Some(json!({"label": "carol", "access": "ro"})));
    let ro_ticket = ro["ticket"].as_str().unwrap().to_string();
    let c = Client::start(&server, &carol_tok, "vc", &dir.join("C"), "C");
    eventually!(10, c.proc.log_contains("reconcile done"), "carol up", c.proc);
    let r = api_json(&server, &carol_tok, "POST", "/api/vaults/vc/mounts", Some(json!({"ticket": ro_ticket})));
    assert_eq!(r["access"], "ro", "carol mount: {r}");
    eventually!(15, c.contains("Shared/X/a.md", "TOP") && c.exists("Shared/X/pic.png"), "carol gets files", c.proc, server.proc);
    // The headless client learns about read-only mounts within 20 s and stops pushing.
    eventually!(30, c.proc.log_contains("read-only share"), "carol learns ro", c.proc);
    let perms = std::fs::metadata(c.path("Shared/X/a.md")).unwrap().permissions();
    assert!(perms.readonly(), "ro files are read-only on disk");
    make_writable(&c.path("Shared/X/a.md"));
    c.append("Shared/X/a.md", "carol-edit");
    sleep(3.0);
    assert!(!a.contains("Projects/X/a.md", "carol-edit"), "ro edit reached owner");
    eventually!(5, !c.contains("Shared/X/a.md", "carol-edit"), "ro edit restored", c.proc);
    c.write("Shared/X/carol.md", "carol-new\n");
    eventually!(8, c.read("Shared/X (read-only copy)/carol.md") == "carol-new\n" && !c.exists("Shared/X/carol.md"), "new file moved aside", c.proc);
    assert!(!a.exists("Projects/X/carol.md"));

    // Listing shows two grants and two mounts.
    let info = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(info["shares"][0]["grants"].as_array().unwrap().len(), 2);
    assert_eq!(info["shares"][0]["mounted_by"], 2);

    // Revoke carol: bob keeps receiving, carol does not.
    let (code, _) = api_raw(&server, &owner, "DELETE", &format!("/api/shares/{share_id}/grants/carol"), None);
    assert_eq!(code, 204);
    a.append("Projects/X/a.md", "after-revoke");
    eventually!(10, b.contains("Shared/X/a.md", "after-revoke"), "bob still receives", b.proc);
    sleep(2.0);
    assert!(!c.contains("Shared/X/a.md", "after-revoke"), "carol still receives after revoke");
    assert!(c.exists("Shared/X/a.md"), "carol keeps files");

    // Un-revoke: the same ticket works again and carol's local mount resumes on its own.
    let (code, body) = api_raw(&server, &owner, "POST", &format!("/api/shares/{share_id}/grants/carol/restore"), None);
    assert_eq!(code, 200, "{body}");
    a.append("Projects/X/a.md", "after-restore");
    eventually!(15, c.contains("Shared/X/a.md", "after-restore"), "carol receives again after restore", c.proc, server.proc);
    // Revoke again, then a second DELETE purges the ticket from the list.
    let (code, _) = api_raw(&server, &owner, "DELETE", &format!("/api/shares/{share_id}/grants/carol"), None);
    assert_eq!(code, 204);
    let (code, _) = api_raw(&server, &owner, "DELETE", &format!("/api/shares/{share_id}/grants/carol"), None);
    assert_eq!(code, 204);
    let info = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(info["shares"][0]["grants"].as_array().unwrap().len(), 1, "revoked ticket purged: {info}");

    // Bob moves his mount to another folder: nothing is deleted for the owner, bob's files move.
    let info = api_json(&server, &bob_tok, "GET", "/api/vaults/vb/sharing", None);
    let mid = info["mounts"][0]["id"].as_str().unwrap().to_string();
    let (code, body) = api_raw(&server, &bob_tok, "PATCH", &format!("/api/mounts/{mid}"), Some(json!({"path": "Team/X"})));
    assert_eq!(code, 200, "{body}");
    eventually!(15, b.exists("Team/X/a.md") && !b.exists("Shared/X/a.md") && b.same_bytes("Team/X/pic.png", &a.path("Projects/X/pic.png")), "bob's files moved", b.proc, server.proc);
    assert!(a.exists("Projects/X/a.md") && a.exists("Projects/X/new.md"), "owner lost files on remount");
    b.append("Team/X/a.md", "after-remount");
    eventually!(10, a.contains("Projects/X/a.md", "after-remount"), "edits flow after remount", a.proc, server.proc);
    let (code, _) = api_raw(&server, &bob_tok, "PATCH", &format!("/api/mounts/{mid}"), Some(json!({"path": "Projects"})));
    assert_eq!(code, 200, "moving to a folder that is only shared on the other side is fine on this side");
    eventually!(15, b.exists("Projects/a.md"), "moved again", b.proc);

    // A folder renamed inside the share arrives elsewhere as delete + create; the emptied old
    // folder is removed too, not left as an empty shell.
    b.write("Projects/Sub/Deep/one.md", "one\n");
    b.write("Projects/Sub/two.md", "two\n");
    eventually!(10, a.exists("Projects/X/Sub/Deep/one.md") && a.exists("Projects/X/Sub/two.md"), "owner gets the subfolder", a.proc, server.proc);
    std::fs::rename(a.path("Projects/X/Sub"), a.path("Projects/X/Moved")).unwrap();
    eventually!(15, b.exists("Projects/Moved/Deep/one.md") && b.exists("Projects/Moved/two.md"), "bob gets the renamed folder", b.proc, server.proc);
    eventually!(10, !b.exists("Projects/Sub"), "bob keeps an empty shell of the old folder", b.proc);
    assert!(b.exists("Projects/a.md"), "mount root pruned");

    // Moving the mount folder on disk is undone by the headless client (the plugin blocks it the
    // same way); only the API/Move… dialog may relocate a mount. The client learns the current
    // roots within 20 s of the remount.
    eventually!(30, b.proc.log_contains("shared with other people here: [\"Projects/\"]"), "bob's client knows the new root", b.proc);
    std::fs::rename(b.path("Projects"), b.path("Elsewhere")).unwrap();
    eventually!(15, b.exists("Projects/a.md") && !b.exists("Elsewhere"), "mount folder moved back", b.proc);
    assert!(b.proc.log_contains("must not be moved"), "no warning logged");
    assert!(a.exists("Projects/X/a.md") && a.exists("Projects/X/new.md"), "owner lost files when bob moved the folder");
    // Moving a note out of the mount is undone too.
    std::fs::rename(b.path("Projects/new.md"), b.path("loose.md")).unwrap();
    eventually!(15, b.exists("Projects/new.md") && !b.exists("loose.md"), "note moved back into the mount", b.proc);
    assert!(a.exists("Projects/X/new.md"), "owner lost the note");
    sleep(2.0);

    // Owner moves the shared folder itself: still shared, nothing deleted for bob.
    let (code, body) = api_raw(&server, &owner, "PATCH", &format!("/api/shares/{share_id}"), Some(json!({"path": "Work/X"})));
    assert_eq!(code, 200, "{body}");
    eventually!(15, a.exists("Work/X/a.md") && !a.exists("Projects/X/a.md"), "owner's files moved", a.proc, server.proc);
    assert!(b.exists("Projects/a.md") && b.exists("Projects/new.md"), "bob lost files on share move");
    a.append("Work/X/a.md", "after-share-move");
    eventually!(10, b.contains("Projects/a.md", "after-share-move"), "edits flow after share move", b.proc, server.proc);
    let info = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(info["shares"][0]["path"], "Work/X");

    // Restart: bridges reattach.
    server.restart(&[]);
    b.append("Projects/a.md", "after-restart");
    eventually!(30, a.contains("Work/X/a.md", "after-restart"), "edit flows after restart", a.proc, server.proc);

    // Unmount keeps files.
    let info = api_json(&server, &bob_tok, "GET", "/api/vaults/vb/sharing", None);
    let mid = info["mounts"][0]["id"].as_str().unwrap().to_string();
    let (code, _) = api_raw(&server, &bob_tok, "DELETE", &format!("/api/mounts/{mid}"), None);
    assert_eq!(code, 204);
    assert!(b.exists("Projects/a.md"));

    assert!(server.errors().is_empty(), "server errors: {:?}", server.errors());
}
