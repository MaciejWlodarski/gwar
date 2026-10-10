//! Removing members and pruning the inactive ones: over the protocol and with the CLI.

mod common;

use std::{
    path::{Path, PathBuf},
    process::{Command, Output},
};

use common::*;
use serde_json::{Value, json};
use vc_proto::{GroupId, Permission};
use vc_server::{core::now_ms, members::lock_data_dir, store::Store};

const DAY: i64 = 24 * 3600 * 1000;

/// A database prepared before the server starts, and a connection to it that
/// stays open next to the server's (SQLite's WAL mode allows that).
struct Seeded {
    dir: tempfile::TempDir,
    store: Store,
}

impl Seeded {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("vc.sqlite3")).unwrap();
        Self { dir, store }
    }

    fn database(&self) -> PathBuf {
        self.dir.path().join("vc.sqlite3")
    }

    fn files(&self) -> PathBuf {
        self.dir.path().join("files")
    }

    /// A member last seen `days` ago holding `groups`; returns its row id.
    fn member(&self, uid: &str, days: i64, groups: &[GroupId]) -> i64 {
        let user = self.store.touch_user(uid, "pk", uid, now_ms() - days * DAY).unwrap();
        self.store.set_user_groups(user.id, groups).unwrap();
        user.id
    }

    fn known(&self, uid: &str) -> bool {
        self.store.user_by_uid(uid).unwrap().is_some()
    }

    async fn start(&self) -> TestServer {
        let (database, files) = (self.database(), self.files());
        TestServer::start_with(|c| {
            c.database = Some(database);
            c.files_dir = Some(files);
        })
        .await
    }
}

fn prune(days: u32, without_groups_only: bool, delete_messages: bool, dry_run: bool) -> Value {
    json!({
        "inactive_days": days,
        "without_groups_only": without_groups_only,
        "delete_messages": delete_messages,
        "dry_run": dry_run,
    })
}

fn uids(reply: &Value) -> Vec<String> {
    let mut uids: Vec<String> =
        reply["uids"].as_array().unwrap().iter().map(|u| u.as_str().unwrap().to_owned()).collect();
    uids.sort();
    uids
}

async fn upload(server: &TestServer, client: &mut Client, name: &str, bytes: &[u8]) -> String {
    let reserved = client.ok("file.upload", json!({"name": name, "size": bytes.len(), "mime": "text/plain"})).await;
    let url = format!("{}{}", server.http(), reserved["upload_url"].as_str().unwrap());
    let status = reqwest::Client::new().put(&url).body(bytes.to_vec()).send().await.unwrap().status();
    assert_eq!(status, 204, "upload accepted");
    reserved["file"].as_str().unwrap().to_owned()
}

/// Waits for a file to leave the disk (it is removed off the core's task).
async fn gone_from_disk(path: &Path) {
    within("file removal", async {
        while path.exists() {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
}

/// The Admin group as the welcome lists it (a client that redeemed its token afterwards has the same).
fn admin_group_holds_member_remove(client: &Client) -> bool {
    client.welcome.groups.iter().any(|g| g.id == 1 && g.permissions.contains(&Permission::MemberRemove))
}

// ---------------------------------------------------------- member.remove

#[tokio::test]
async fn only_admins_and_holders_of_the_permission_may_remove_members() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let bob = Client::connect(&server, "bob").await;
    assert!(admin_group_holds_member_remove(&root), "Admin holds every permission");
    assert!(!alice.welcome.permissions.contains(&Permission::MemberRemove), "members do not get it by default");

    let target = json!({"uid": bob.welcome.uid, "delete_messages": false});
    assert_eq!(alice.fails("member.remove", target.clone()).await, "forbidden");
    assert_eq!(alice.fails("member.prune", prune(30, true, false, true)).await, "forbidden");

    root.ok("member.remove", target).await;
    assert_eq!(root.fails("member.remove", json!({"uid": bob.welcome.uid})).await, "not_found");
}

#[tokio::test]
async fn admin_gets_the_permission_on_an_existing_database() {
    let seeded = Seeded::new();
    // As an older server left it: Admin without the newest permission.
    let old =
        serde_json::to_string(&Permission::ALL.iter().filter(|p| **p != Permission::MemberRemove).collect::<Vec<_>>());
    rusqlite::Connection::open(seeded.database())
        .unwrap()
        .execute("UPDATE groups SET permissions=?1 WHERE id=1", [old.unwrap()])
        .unwrap();
    let server = seeded.start().await;
    let root = admin(&server, "root").await;
    assert!(admin_group_holds_member_remove(&root));
}

#[tokio::test]
async fn nobody_removes_themselves_or_anyone_above_them() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let mut carol = Client::connect(&server, "carol").await;
    let janitors =
        root.ok("group.create", json!({"name": "Janitors", "permissions": ["member_remove"]})).await["group"]["id"]
            .clone();
    let mods =
        root.ok("group.create", json!({"name": "Mods", "permissions": ["client_kick"]})).await["group"]["id"].clone();
    root.ok("member.groups", json!({"uid": alice.welcome.uid, "groups": [2, janitors]})).await;
    root.ok("member.groups", json!({"uid": bob.welcome.uid, "groups": [2, mods]})).await;

    // Not oneself; not an admin; not a moderator whose rights she lacks.
    assert_eq!(alice.fails("member.remove", json!({"uid": alice.welcome.uid})).await, "bad_request");
    assert_eq!(root.fails("member.remove", json!({"uid": root.welcome.uid})).await, "bad_request");
    assert_eq!(alice.fails("member.remove", json!({"uid": root.welcome.uid})).await, "forbidden");
    assert_eq!(alice.fails("member.remove", json!({"uid": bob.welcome.uid})).await, "forbidden");
    // A plain member is fine, and a moderator is for an admin.
    alice.ok("member.remove", json!({"uid": carol.welcome.uid})).await;
    carol.expect_disconnect().await;
    root.ok("member.remove", json!({"uid": bob.welcome.uid})).await;
    bob.expect_disconnect().await;
}

#[tokio::test]
async fn removing_an_online_member_disconnects_them_and_they_return_as_a_newcomer() {
    let server = TestServer::start_with(|c| c.server_password = Some("secret".into())).await;
    let mut root = Client::connect_as(&server, new_key(), "root", Some("secret")).await.unwrap();
    root.redeem(&server.admin_token).await;
    let key = new_key();
    let mut carol = Client::connect_as(&server, key.clone(), "carol", Some("secret")).await.unwrap();
    let mut bob = Client::connect_as(&server, new_key(), "bob", Some("secret")).await.unwrap();
    let mods =
        root.ok("group.create", json!({"name": "Mods", "permissions": ["client_kick"]})).await["group"]["id"].clone();
    root.ok("member.groups", json!({"uid": carol.welcome.uid, "groups": [2, mods]})).await;
    // Bans are a separate matter: this one is not touched by the removal.
    root.ok("ban.create", json!({"uid": bob.welcome.uid})).await;
    bob.expect_disconnect().await;

    root.ok("member.remove", json!({"uid": carol.welcome.uid, "delete_messages": false})).await;
    let reason = carol.expect_disconnect().await;
    assert_eq!(reason, json!({"kind": "removed", "by": "root"}));
    let removed = root.next_event("member.removed").await;
    assert_eq!(removed, json!({"uid": carol.welcome.uid}));

    // Not a ban: with the password she gets back in, but without the old roles.
    let refused =
        Client::connect_as(&server, key.clone(), "carol", None).await.err().expect("a newcomer needs the password");
    assert_eq!(refused.code, vc_proto::ErrorCode::WrongPassword);
    let again = Client::connect_as(&server, key, "carol", Some("secret")).await.expect("not banned");
    let me = again.welcome.clients.iter().find(|c| c.id == again.id()).unwrap();
    assert_eq!(me.groups, vec![2]);
    let listed = root.ok("ban.list", json!({})).await;
    assert_eq!(listed["bans"].as_array().unwrap().len(), 1, "the ban stays");
}

#[tokio::test]
async fn removal_can_delete_messages_and_files_or_leave_them() {
    let seeded = Seeded::new();
    let server = seeded.start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let mut bob = Client::connect(&server, "bob").await;
    let lobby = alice.default_channel();
    let side = root.create_channel(json!({"name": "Side"})).await["id"].as_u64().unwrap();

    let attached = upload(&server, &mut alice, "a.txt", b"attached").await;
    let pending = upload(&server, &mut alice, "p.txt", b"never sent").await;
    let one =
        alice.ok("chat.send", json!({"target": {"channel": lobby}, "text": "one", "attachments": [attached]})).await;
    let two = alice.ok("chat.send", json!({"target": {"channel": side}, "text": "two"})).await;
    // Private and server-wide messages are never stored: nothing of them to delete.
    alice.ok("chat.send", json!({"target": "server", "text": "hello everyone"})).await;
    bob.ok("chat.send", json!({"target": {"channel": lobby}, "text": "mine"})).await;

    root.ok("member.remove", json!({"uid": alice.welcome.uid, "delete_messages": true})).await;
    alice.expect_disconnect().await;
    let deleted = bob.drain_events("chat.deleted").await;
    assert_eq!(deleted.len(), 2);
    assert!(deleted.contains(&json!({"channel": lobby, "message": one["id"]})));
    assert!(deleted.contains(&json!({"channel": side, "message": two["id"]})));
    assert_eq!(bob.next_event("member.removed").await["uid"], json!(alice.welcome.uid));

    let history = bob.ok("chat.history", json!({"channel": lobby})).await;
    let texts: Vec<_> = history["messages"].as_array().unwrap().iter().map(|m| m["text"].clone()).collect();
    assert_eq!(texts, vec![json!("mine")]);
    gone_from_disk(&seeded.files().join(&attached)).await;
    gone_from_disk(&seeded.files().join(&pending)).await;
    assert!(!seeded.known(&alice.welcome.uid));

    // Without the flag the words stay behind, still signed with the author's name.
    let mut carol = Client::connect(&server, "carol").await;
    let kept = upload(&server, &mut carol, "c.txt", b"kept").await;
    carol.ok("chat.send", json!({"target": {"channel": lobby}, "text": "stay", "attachments": [kept]})).await;
    root.ok("member.remove", json!({"uid": carol.welcome.uid, "delete_messages": false})).await;
    carol.expect_disconnect().await;
    assert!(bob.drain_events("chat.deleted").await.is_empty());
    let history = bob.ok("chat.history", json!({"channel": lobby})).await;
    let stay = history["messages"].as_array().unwrap().iter().find(|m| m["text"] == "stay").expect("kept");
    assert_eq!(stay["author_name"], "carol");
    assert!(seeded.files().join(&kept).exists());
}

// ----------------------------------------------------------- member.prune

#[tokio::test]
async fn prune_spares_the_recent_the_online_the_caller_and_roles() {
    let seeded = Seeded::new();
    let mods = seeded.store.insert_group("Mods", &[Permission::ClientKick], None).unwrap();
    seeded.member("stale", 100, &[2]);
    seeded.member("stale-mod", 100, &[2, mods]);
    seeded.member("recent", 5, &[2]);
    let lobby = seeded.store.channels().unwrap()[0].id;
    seeded.store.add_message(lobby, "stale", "stale", "ancient", now_ms() - 100 * DAY).unwrap();
    let server = seeded.start().await;
    let mut root = admin(&server, "root").await;
    let mut bob = Client::connect(&server, "bob").await;
    // Both look absent for months, yet one is connected and the other is the caller.
    for uid in [&root.welcome.uid, &bob.welcome.uid] {
        let (id, _) = seeded.store.user_by_uid(uid).unwrap().unwrap();
        seeded.store.set_last_seen(id, now_ms() - 400 * DAY).unwrap();
    }

    assert_eq!(root.fails("member.prune", prune(0, true, false, true)).await, "bad_request");
    assert_eq!(root.fails("member.prune", prune(5000, true, false, true)).await, "bad_request");

    // A preview changes nothing.
    let preview = root.ok("member.prune", prune(30, true, true, true)).await;
    assert_eq!((uids(&preview), preview["count"].as_u64()), (vec!["stale".to_owned()], Some(1)));
    let preview = root.ok("member.prune", prune(30, false, true, true)).await;
    assert_eq!(
        (uids(&preview), preview["count"].as_u64()),
        (vec!["stale".to_owned(), "stale-mod".to_owned()], Some(2))
    );
    assert!(seeded.known("stale") && seeded.known("stale-mod"));
    assert_eq!(root.ok("chat.history", json!({"channel": lobby})).await["messages"].as_array().unwrap().len(), 1);
    assert!(bob.drain_events("member.removed").await.is_empty());

    // The real thing, sparing roles.
    let done = root.ok("member.prune", prune(30, true, true, false)).await;
    assert_eq!((uids(&done), done["count"].as_u64()), (vec!["stale".to_owned()], Some(1)));
    assert_eq!(bob.next_event("member.removed").await, json!({"uid": "stale"}));
    assert_eq!(bob.next_event("chat.deleted").await["channel"], json!(lobby));
    assert!(!seeded.known("stale"));
    for kept in ["stale-mod", "recent", root.welcome.uid.as_str(), bob.welcome.uid.as_str()] {
        assert!(seeded.known(kept), "{kept} is spared");
    }
    assert!(root.ok("chat.history", json!({"channel": lobby})).await["messages"].as_array().unwrap().is_empty());

    // Now including roles; online and the caller still stay.
    let done = root.ok("member.prune", prune(30, false, false, false)).await;
    assert_eq!(uids(&done), vec!["stale-mod".to_owned()]);
    let done = root.ok("member.prune", prune(30, false, false, false)).await;
    assert_eq!((uids(&done), done["count"].as_u64()), (vec![], Some(0)));
    assert!(seeded.known(&root.welcome.uid) && seeded.known(&bob.welcome.uid) && seeded.known("recent"));
}

#[tokio::test]
async fn prune_only_touches_members_the_caller_covers() {
    let seeded = Seeded::new();
    let mods = seeded.store.insert_group("Mods", &[Permission::ClientKick], None).unwrap();
    seeded.member("stale", 100, &[2]);
    seeded.member("stale-mod", 100, &[2, mods]);
    seeded.member("stale-admin", 100, &[1]);
    let server = seeded.start().await;
    let mut root = admin(&server, "root").await;
    let mut jan = Client::connect(&server, "jan").await;
    let janitors =
        root.ok("group.create", json!({"name": "Janitors", "permissions": ["member_remove"]})).await["group"]["id"]
            .clone();
    root.ok("member.groups", json!({"uid": jan.welcome.uid, "groups": [2, janitors]})).await;

    let done = jan.ok("member.prune", prune(30, false, false, false)).await;
    assert_eq!((uids(&done), done["count"].as_u64()), (vec!["stale".to_owned()], Some(1)));
    assert!(seeded.known("stale-mod") && seeded.known("stale-admin"));
}

#[tokio::test]
async fn prune_works_through_big_backlogs_in_batches() {
    let seeded = Seeded::new();
    for i in 0..120 {
        seeded.member(&format!("old-{i:03}"), 200, &[2]);
    }
    let server = seeded.start().await;
    let mut root = admin(&server, "root").await;

    let preview = root.ok("member.prune", prune(30, true, false, true)).await;
    assert_eq!((preview["uids"].as_array().unwrap().len(), preview["count"].as_u64()), (120, Some(120)));
    for (removed, left) in [(50, 120), (50, 70), (20, 20), (0, 0)] {
        let done = root.ok("member.prune", prune(30, true, false, false)).await;
        assert_eq!((done["uids"].as_array().unwrap().len(), done["count"].as_u64()), (removed, Some(left)));
    }
    assert!(!seeded.known("old-000") && !seeded.known("old-119"));
}

// -------------------------------------------------------------------- CLI

fn cli(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_vc-server"))
        .arg("--data-dir")
        .arg(dir)
        .args(["members"])
        .args(args)
        .env_remove("VC_DATA_DIR")
        .output()
        .expect("run vc-server")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn the_command_line_lists_and_prunes_members() {
    let seeded = Seeded::new();
    let mods = seeded.store.insert_group("Mods", &[Permission::ClientKick], None).unwrap();
    seeded.member("stale", 100, &[2]);
    seeded.member("stale-mod", 100, &[2, mods]);
    seeded.member("recent", 3, &[2]);
    let lobby = seeded.store.channels().unwrap()[0].id;
    seeded.store.add_message(lobby, "stale", "stale", "ancient", now_ms() - 100 * DAY).unwrap();
    std::fs::create_dir_all(seeded.files()).unwrap();
    seeded.store.insert_file("f1", "stale", "a.txt", "text/plain", 3, None, now_ms()).unwrap();
    let message = seeded.store.add_message(lobby, "stale", "stale", "with file", now_ms() - 99 * DAY).unwrap();
    seeded.store.attach_file("f1", message).unwrap();
    std::fs::write(seeded.files().join("f1"), "abc").unwrap();
    let dir = seeded.dir.path();

    let all = cli(dir, &["list"]);
    assert!(all.status.success());
    let all = stdout(&all);
    assert!(all.contains("stale\tstale\t") && all.contains("\t2\n"), "message count of stale: {all}");
    assert!(all.contains("stale-mod\t") && all.contains("Member,Mods"), "{all}");
    assert!(all.contains("recent\t"), "{all}");
    let old = stdout(&cli(dir, &["list", "--inactive-days", "30"]));
    assert!(old.contains("stale\t") && !old.contains("recent\t"), "{old}");

    // A preview reports and changes nothing, even while a server holds the directory.
    let server = lock_data_dir(dir).unwrap();
    let preview = cli(dir, &["prune", "--inactive-days", "30", "--delete-messages", "--dry-run"]);
    assert!(preview.status.success());
    let preview = stdout(&preview);
    assert!(preview.contains("stale\t") && !preview.contains("stale-mod\t"), "{preview}");
    assert!(seeded.known("stale"));
    // The real thing refuses to run beside a server.
    let refused = cli(dir, &["prune", "--inactive-days", "30"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("stop it first"));
    assert!(seeded.known("stale"));
    drop(server);

    let done = cli(dir, &["prune", "--inactive-days", "30", "--delete-messages"]);
    assert!(done.status.success(), "{}", String::from_utf8_lossy(&done.stderr));
    assert!(!seeded.known("stale") && seeded.known("stale-mod") && seeded.known("recent"));
    assert!(!seeded.files().join("f1").exists());
    assert!(seeded.store.history(lobby, None, 10).unwrap().is_empty());

    let done = cli(dir, &["prune", "--inactive-days", "30", "--include-grouped"]);
    assert!(done.status.success());
    assert!(!seeded.known("stale-mod") && seeded.known("recent"));
    let conflict = cli(dir, &["prune", "--inactive-days", "30", "--keep-groups", "--include-grouped"]);
    assert!(!conflict.status.success());

    // Named members go regardless of roles or activity; an unknown uid changes nothing.
    let typo = cli(dir, &["remove", "recent", "nobody"]);
    assert!(!typo.status.success() && seeded.known("recent"));
    assert!(cli(dir, &["remove", "recent", "--dry-run"]).status.success() && seeded.known("recent"));
    assert!(cli(dir, &["remove", "recent"]).status.success());
    assert!(!seeded.known("recent"));
}
