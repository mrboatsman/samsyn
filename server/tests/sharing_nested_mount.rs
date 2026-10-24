//! A mount inside the owner's own share: the owner shares the whole vault with bob and then
//! mounts carol's folder into it. Carol's content stays out of bob's copy unless the owner
//! opts in, per enclosing share; the choice can be changed later, follows a move, survives a
//! restart, and lapses when the mount is removed.

mod common;

use common::*;
use serde_json::json;

#[test]
fn mount_inside_share_is_carried_onward_only_on_request() {
    let dir = tmp("nested-mount");
    let mut server = Server::start(&dir, "s", &[]);
    let owner = server.owner_token.clone();
    let bob_tok = server.add_user("bob");
    let carol_tok = server.add_user("carol");
    let dave_tok = server.add_user("dave");

    let a = Client::start(&server, &owner, "va", &dir.join("A"), "A");
    for i in 0..6 {
        a.write(&format!("Notes/n{i}.md"), &format!("note {i}\n"));
    }
    let b = Client::start(&server, &bob_tok, "vb", &dir.join("B"), "B");
    let c = Client::start(&server, &carol_tok, "vc", &dir.join("C"), "C");
    c.write("Stuff/s.md", "carol-content\n");
    c.write("Stuff/deep/t.md", "carol-deep\n");
    let d = Client::start(&server, &dave_tok, "vd", &dir.join("D"), "D");
    eventually!(
        20,
        [&a, &b, &c, &d].iter().all(|x| x.proc.log_contains("reconcile done")),
        "clients up",
        a.proc,
        b.proc,
        c.proc,
        d.proc
    );

    // Owner shares the whole vault with bob, and Notes/ on its own (to check the equal-path rule).
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": ""})));
    let vault_share = r["share"]["id"].as_str().expect("share id").to_string();
    let vault_ticket = r["ticket"].as_str().expect("ticket").to_string();
    assert_eq!(r["forward_mounts"], json!([]), "{r}");
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Notes"})));
    assert!(r["share"]["id"].is_string(), "{r}");
    let r = api_json(&server, &bob_tok, "POST", "/api/vaults/vb/mounts", Some(json!({"ticket": vault_ticket, "mount_path": "Shared/V"})));
    assert_eq!(r["access"], "rw", "bob mount: {r}");
    eventually!(15, b.exists("Shared/V/Notes/n0.md"), "bob gets the vault", b.proc, server.proc);

    // Carol shares Stuff/ with the owner.
    let r = api_json(&server, &carol_tok, "POST", "/api/vaults/vc/shares", Some(json!({"path": "Stuff", "name": "Stuff"})));
    let carol_ticket = r["ticket"].as_str().expect("ticket").to_string();

    // Mounting onto a share's own folder is refused; strictly inside is fine and skipped by default.
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/mounts", Some(json!({"ticket": carol_ticket, "mount_path": "Notes"})));
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("overlaps what you share"), "{body}");
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/mounts", Some(json!({"ticket": carol_ticket, "mount_path": "Shared/Carol", "forward_shares": ["nope"]})));
    assert_eq!(code, 400, "unknown forward id accepted: {body}");
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/mounts", Some(json!({"ticket": carol_ticket, "mount_path": "Shared/Carol"})));
    let mount_id = r["mount"]["id"].as_str().unwrap_or_else(|| panic!("mount id: {r}")).to_string();
    assert_eq!(r["forward_shares"], json!([]), "{r}");
    eventually!(15, a.exists("Shared/Carol/s.md") && a.exists("Shared/Carol/deep/t.md"), "owner gets carol's folder", a.proc, server.proc);
    sleep(3.0);
    assert!(!b.exists("Shared/V/Shared/Carol/s.md") && !b.exists("Shared/V/Shared"), "carol's content leaked to bob by default");
    let listing = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(listing["mounts"][0]["forward_shares"], json!([]), "{listing}");

    // Nothing may be shared from inside the mount, and mounts never nest.
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Shared/Carol/deep"})));
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("cannot be shared onward"), "{body}");
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/mounts", Some(json!({"ticket": carol_ticket, "mount_path": "Shared/Carol/inner"})));
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("share with you"), "{body}");

    // Owner opts in: bob gets carol's folder, edits flow both ways along the chain.
    let (code, body) = api_raw(&server, &owner, "PUT", &format!("/api/mounts/{mount_id}/forward"), Some(json!({"share_ids": [vault_share]})));
    assert_eq!(code, 200, "{body}");
    eventually!(15, b.read("Shared/V/Shared/Carol/s.md") == "carol-content\n" && b.exists("Shared/V/Shared/Carol/deep/t.md"), "bob gets carol's folder once forwarded", b.proc, server.proc);
    c.append("Stuff/s.md", "carol-edit");
    eventually!(10, b.contains("Shared/V/Shared/Carol/s.md", "carol-edit"), "carol's edit reaches bob", b.proc, server.proc);
    b.append("Shared/V/Shared/Carol/s.md", "bob-edit");
    eventually!(10, c.contains("Stuff/s.md", "bob-edit") && a.contains("Shared/Carol/s.md", "bob-edit"), "bob's edit reaches carol", c.proc, a.proc, server.proc);
    let listing = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(listing["mounts"][0]["forward_shares"], json!([vault_share]), "{listing}");

    // Opting out again removes bob's copy; the owner keeps the mount.
    let (code, body) = api_raw(&server, &owner, "PUT", &format!("/api/mounts/{mount_id}/forward"), Some(json!({"share_ids": []})));
    assert_eq!(code, 200, "{body}");
    eventually!(15, !b.exists("Shared/V/Shared/Carol/s.md") && !b.exists("Shared/V/Shared/Carol/deep/t.md"), "bob loses carol's folder", b.proc, server.proc);
    assert!(a.contains("Shared/Carol/s.md", "bob-edit"), "owner lost the mount");
    c.append("Stuff/s.md", "still-private");
    eventually!(10, a.contains("Shared/Carol/s.md", "still-private"), "owner still synced with carol", a.proc, server.proc);
    sleep(2.0);
    assert!(!b.exists("Shared/V/Shared/Carol/s.md"), "carol's content came back to bob");

    // The rule survives a restart.
    server.restart(&[]);
    c.append("Stuff/s.md", "after-restart");
    eventually!(30, a.contains("Shared/Carol/s.md", "after-restart"), "owner synced after restart", a.proc, server.proc);
    sleep(2.0);
    assert!(!b.exists("Shared/V/Shared/Carol/s.md"), "carol's content leaked to bob after restart");

    // A new share around the mount can carry it from the start: dave gets Shared/ with carol inside.
    let (code, body) = api_raw(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Shared", "forward_mounts": ["nope"]})));
    assert_eq!(code, 400, "unknown mount id accepted: {body}");
    let r = api_json(&server, &owner, "POST", "/api/vaults/va/shares", Some(json!({"path": "Shared", "name": "Shared", "forward_mounts": [mount_id]})));
    let shared_share = r["share"]["id"].as_str().unwrap_or_else(|| panic!("share id: {r}")).to_string();
    assert_eq!(r["forward_mounts"], json!([mount_id]), "{r}");
    let shared_ticket = r["ticket"].as_str().expect("ticket").to_string();
    let r = api_json(&server, &dave_tok, "POST", "/api/vaults/vd/mounts", Some(json!({"ticket": shared_ticket, "mount_path": "X"})));
    assert_eq!(r["access"], "rw", "dave mount: {r}");
    eventually!(15, d.contains("X/Carol/s.md", "after-restart"), "dave gets carol's folder through the new share", d.proc, server.proc);
    sleep(2.0);
    assert!(!b.exists("Shared/V/Shared/Carol/s.md"), "the vault share started carrying the mount on its own");
    let listing = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(listing["mounts"][0]["forward_shares"], json!([shared_share]), "{listing}");

    // A share containing a mount cannot be moved; the mount can, and the decisions follow it.
    let (code, body) = api_raw(&server, &owner, "PATCH", &format!("/api/shares/{shared_share}"), Some(json!({"path": "Elsewhere"})));
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("unmount those first"), "{body}");
    let (code, body) = api_raw(&server, &owner, "PATCH", &format!("/api/mounts/{mount_id}"), Some(json!({"path": "Shared/C2"})));
    assert_eq!(code, 200, "{body}");
    eventually!(15, a.exists("Shared/C2/s.md") && !a.exists("Shared/Carol/s.md"), "owner's mount moved", a.proc, server.proc);
    eventually!(15, d.exists("X/C2/s.md") && !d.exists("X/Carol/s.md"), "dave follows the move", d.proc, server.proc);
    sleep(2.0);
    assert!(!b.exists("Shared/V/Shared/C2/s.md") && !b.exists("Shared/V/Shared/Carol/s.md"), "the move leaked carol's content to bob");
    c.append("Stuff/s.md", "after-move");
    eventually!(10, a.contains("Shared/C2/s.md", "after-move") && d.contains("X/C2/s.md", "after-move"), "edits flow after the move", a.proc, d.proc, server.proc);
    assert!(!b.exists("Shared/V/Shared/C2/s.md"));
    // Onto another share's own folder is refused; into it is fine. Leaving Shared/ ends the
    // decision for that share (dave loses the folder) and the Notes share starts out skipping it.
    let (code, body) = api_raw(&server, &owner, "PATCH", &format!("/api/mounts/{mount_id}"), Some(json!({"path": "Notes"})));
    assert_eq!(code, 409, "{body}");
    let (code, body) = api_raw(&server, &owner, "PATCH", &format!("/api/mounts/{mount_id}"), Some(json!({"path": "Notes/C3"})));
    assert_eq!(code, 200, "{body}");
    eventually!(15, a.contains("Notes/C3/s.md", "after-move") && !a.exists("Shared/C2/s.md"), "owner's mount moved into Notes", a.proc, server.proc);
    eventually!(15, !d.exists("X/C2/s.md") && !d.exists("X/C2"), "dave loses the folder that left the share", d.proc, server.proc);
    let listing = api_json(&server, &owner, "GET", "/api/vaults/va/sharing", None);
    assert_eq!(listing["mounts"][0]["mount_path"], "Notes/C3", "{listing}");
    assert_eq!(listing["mounts"][0]["forward_shares"], json!([]), "{listing}");
    sleep(2.0);
    assert!(!b.exists("Shared/V/Notes/C3/s.md") && !b.exists("Shared/V/Shared/C2/s.md"), "the move leaked carol's content to bob");

    // Unmounting turns the folder into ordinary notes, which the enclosing shares then carry.
    let (code, body) = api_raw(&server, &owner, "DELETE", &format!("/api/mounts/{mount_id}"), None);
    assert_eq!(code, 204, "{body}");
    eventually!(15, b.contains("Shared/V/Notes/C3/s.md", "after-move"), "bob gets the leftover notes once unmounted", b.proc, server.proc);

    assert!(server.errors().is_empty(), "server errors: {:?}", server.errors());
}
