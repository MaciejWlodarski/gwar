//! Attachment associations and cleanup are atomic even outside the running actor.
use gwar_proto::SealedMessage;
use gwar_server::{
    core::now_ms,
    store::{ChannelRow, Store, mls::GroupRow},
};
use rusqlite::{Connection, params};

struct Fixture {
    dir: tempfile::TempDir,
    store: Store,
    db: Connection,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gwar.sqlite3");
        let store = Store::open(&path).unwrap();
        store.seed_channels().unwrap();
        let db = Connection::open(path).unwrap();
        db.execute_batch("PRAGMA foreign_keys=ON").unwrap();
        let f = Self { dir, store, db };
        f.group("g", "dm:a:b");
        f
    }

    fn group(&self, group: &str, conversation: &str) {
        self.store
            .mls_write_group(
                &GroupRow {
                    group: group.into(),
                    conversation: conversation.into(),
                    incarnation: 0,
                    epoch: 0,
                    committed_at: 1,
                    group_info: vec![],
                    observer: vec![],
                    created_at: 1,
                },
                &[],
                &[],
                None,
                None,
                &[],
                None,
            )
            .unwrap();
    }

    fn file(&self, id: &str, uploader: &str, sealed: bool) {
        self.store.insert_file(id, uploader, "name", "image/png", 3, "hash", Some((10, 20)), 1, sealed).unwrap();
    }

    fn message(&self, author: &str, identity: &str, files: &[&str]) -> SealedMessage {
        let mut m = row(author, identity);
        m.id = self
            .store
            .sealed_insert(&m, b"opaque", &files.iter().map(|s| (*s).into()).collect::<Vec<_>>())
            .unwrap()
            .unwrap();
        m
    }

    fn exists(&self, file: &str) -> bool {
        self.store.file(file).unwrap().is_some()
    }
}

fn row(author: &str, identity: &str) -> SealedMessage {
    SealedMessage {
        id: 0,
        conversation: "dm:a:b".into(),
        group: "g".into(),
        epoch: 0,
        author_uid: author.into(),
        author_name: author.into(),
        message_id: identity.into(),
        revision: 0,
        ciphertext: "b3BhcXVl".into(),
        sent_at: 1,
        edited_at: None,
        deleted: false,
    }
}

#[test]
fn plaintext_and_sealed_numeric_ids_never_share_files() {
    for delete_sealed in [false, true] {
        let f = Fixture::new();
        let channel = f.store.channels().unwrap()[0].id;
        let plain = f.store.add_message(channel, "a", "A", "plain", 1).unwrap();
        f.file("plain", "a", false);
        f.store.attach_file("plain", plain).unwrap();
        f.file("sealed", "a", true);
        let sealed = f.message("a", "one", &["sealed"]);
        assert_eq!(plain, sealed.id);
        assert_eq!(f.store.file_channel("plain").unwrap(), Some(channel));
        assert_eq!(f.store.file_channel("sealed").unwrap(), None);
        assert_eq!(f.store.file_conversation("plain").unwrap(), None);
        assert_eq!(f.store.file_conversation("sealed").unwrap().as_deref(), Some("dm:a:b"));
        if delete_sealed {
            f.store.sealed_remove("dm:a:b", sealed.id).unwrap();
        } else {
            f.store.delete_message(plain).unwrap();
        }
        assert_eq!(f.exists("plain"), delete_sealed);
        assert_eq!(f.exists("sealed"), !delete_sealed);
        assert_eq!(f.store.removed_files().unwrap(), vec![if delete_sealed { "sealed" } else { "plain" }]);
    }
}

#[test]
fn sealed_file_ties_and_message_identity_roll_back_on_failure() {
    let f = Fixture::new();
    f.file("a", "a", true);
    f.file("b", "a", true);
    f.db.execute_batch("CREATE TRIGGER fail_tie AFTER UPDATE OF sealed_message ON files WHEN NEW.id='b' BEGIN SELECT RAISE(ABORT,'injected'); END;").unwrap();
    let message = row("a", "one");
    assert!(f.store.sealed_insert(&message, b"opaque", &["a".into(), "b".into()]).is_err());
    assert!(f.store.sealed_message("dm:a:b", "one").unwrap().is_none());
    assert!(f.store.file_conversation("a").unwrap().is_none());
    f.db.execute_batch("DROP TRIGGER fail_tie").unwrap();
    let message = f.message("a", "one", &["a", "b"]);
    assert_eq!(f.store.file_conversation("a").unwrap().as_deref(), Some("dm:a:b"));
    // The schema also rejects either mode being attached through the other association.
    let channel = f.store.channels().unwrap()[0].id;
    let plain = f.store.add_message(channel, "a", "A", "plain", 1).unwrap();
    assert!(f.store.attach_file("a", plain).is_err());
    f.file("plain", "a", false);
    assert!(f.db.execute("UPDATE files SET sealed_message=?1 WHERE id='plain'", [message.id]).is_err());
}

#[test]
fn sealed_deletion_rolls_back_message_files_and_deferred_charges_together() {
    let f = Fixture::new();
    f.file("file", "a", true);
    let mut message = f.message("a", "one", &["file"]);
    f.db.execute_batch("CREATE TRIGGER fail_delete AFTER DELETE ON files BEGIN SELECT RAISE(ABORT,'injected'); END;")
        .unwrap();
    message.deleted = true;
    message.revision = 1;
    assert!(f.store.sealed_update(&message, b"deleted").is_err());
    assert!(f.store.sealed_remove("dm:a:b", message.id).is_err());
    assert!(f.exists("file"));
    assert!(!f.store.sealed_message("dm:a:b", "one").unwrap().unwrap().deleted);
    assert!(f.store.removed_files().unwrap().is_empty());
    f.db.execute_batch("DROP TRIGGER fail_delete").unwrap();
    f.store.sealed_update(&message, b"deleted").unwrap();
    assert!(!f.exists("file"));
    assert_eq!(f.store.stored_bytes(Some("a")).unwrap(), 3);
}

#[test]
fn sealed_history_pruning_removes_only_the_pruned_messages_files() {
    let f = Fixture::new();
    f.file("old", "a", true);
    f.file("kept", "a", true);
    f.message("a", "0", &["old"]);
    f.message("a", "1", &["kept"]);
    for id in 2..=1000 {
        f.message("a", &id.to_string(), &[]);
    }
    assert!(!f.exists("old"));
    assert!(f.exists("kept"));
    assert_eq!(f.store.removed_files().unwrap(), vec!["old"]);
    assert_eq!(f.db.query_row("SELECT COUNT(*) FROM sealed_messages", [], |r| r.get::<_, u32>(0)).unwrap(), 1000);
}

#[test]
fn sealed_channel_subtree_deletion_and_member_removal_are_atomic() {
    let f = Fixture::new();
    let user = f.store.touch_user("a", "pk", "A", 1).unwrap().id;
    let mut parent = None;
    let mut channels = Vec::new();
    for id in ["parent", "child", "unrelated"] {
        let channel = f
            .store
            .create_channel(&ChannelRow {
                id: 0,
                parent,
                name: id.into(),
                topic: String::new(),
                position: 0,
                password_hash: None,
                max_clients: None,
                encrypted: true,
                history: true,
                private: false,
                access: None,
                overrides: Vec::new(),
                synced: false,
            })
            .unwrap();
        parent = if id == "parent" { Some(channel) } else { None };
        channels.push(channel);
        let conversation = format!("channel:{channel}");
        f.group(id, &conversation);
        f.file(id, "a", true);
        let mut message = row("a", id);
        message.group = id.into();
        message.conversation = conversation;
        f.store.sealed_insert(&message, b"opaque", &[id.into()]).unwrap();
    }
    f.db.execute_batch(
        "CREATE TRIGGER fail_channel AFTER DELETE ON channels BEGIN SELECT RAISE(ABORT,'injected'); END;",
    )
    .unwrap();
    assert!(f.store.delete_channel(channels[0]).is_err());
    for id in ["parent", "child", "unrelated"] {
        assert!(f.exists(id));
    }
    assert!(f.store.removed_files().unwrap().is_empty());
    f.db.execute_batch("DROP TRIGGER fail_channel").unwrap();
    f.store.delete_channel(channels[0]).unwrap();
    assert!(!f.exists("parent"));
    assert!(!f.exists("child"));
    assert!(f.exists("unrelated"));
    f.db.execute_batch("CREATE TRIGGER fail_member AFTER DELETE ON users BEGIN SELECT RAISE(ABORT,'injected'); END;")
        .unwrap();
    assert!(f.store.remove_member(user, "a", true).is_err());
    assert!(f.exists("unrelated"));
    assert!(f.store.user_by_uid("a").unwrap().is_some());
}

#[test]
fn sealed_member_cli_removal_and_pruning_keep_or_delete_the_right_files() {
    for prune in [false, true] {
        for delete_messages in [false, true] {
            let f = Fixture::new();
            f.store.touch_user("a", "pk", "A", now_ms() - 3 * 86_400_000).unwrap();
            f.store.touch_user("b", "pk2", "B", now_ms()).unwrap();
            for (id, uploader, sealed) in
                [("own", "a", true), ("peer", "b", true), ("pending", "a", true), ("plain", "a", false)]
            {
                f.file(id, uploader, sealed);
            }
            f.message("a", "own", &["own"]);
            f.message("b", "peer", &["peer"]);
            let files_dir = f.dir.path().join("files");
            std::fs::create_dir(&files_dir).unwrap();
            for id in ["own", "peer", "pending", "plain"] {
                std::fs::write(files_dir.join(id), b"abc").unwrap();
            }
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_gwar-server"));
            command.arg("--data-dir").arg(f.dir.path()).arg("members").env_remove("GWAR_DATA_DIR");
            if prune {
                command.args(["prune", "--inactive-days", "1"]);
            } else {
                command.args(["remove", "a"]);
            }
            if delete_messages {
                command.arg("--delete-messages");
            }
            let output = command.output().unwrap();
            assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
            assert!(!f.exists("pending"));
            assert!(!f.exists("plain"));
            assert_eq!(f.exists("own"), !delete_messages);
            assert!(f.exists("peer"));
            assert_eq!(files_dir.join("own").exists(), !delete_messages);
            assert!(!files_dir.join("pending").exists());
            assert_eq!(f.store.stored_bytes(Some("a")).unwrap(), if delete_messages { 0 } else { 3 });
            assert!(f.store.expire_unattached(i64::MAX).unwrap().is_empty());
            assert!(f.store.expire_orphans().unwrap().is_empty());
            assert_eq!(f.exists("own"), !delete_messages);
        }
    }
}

#[test]
fn expiry_checks_both_message_associations() {
    let f = Fixture::new();
    let channel = f.store.channels().unwrap()[0].id;
    let plain = f.store.add_message(channel, "a", "A", "plain", 1).unwrap();
    for (id, sealed) in [("plain", false), ("sealed", true), ("unused_plain", false), ("unused_sealed", true)] {
        f.file(id, "a", sealed);
    }
    f.store.attach_file("plain", plain).unwrap();
    let sealed = f.message("a", "one", &["sealed"]);
    let mut expired = f.store.expire_unattached(2).unwrap();
    expired.sort();
    assert_eq!(expired, vec!["unused_plain", "unused_sealed"]);
    assert!(f.exists("plain") && f.exists("sealed"));
    // Simulate legacy/corrupt orphan associations, independently of their numeric ids.
    f.db.execute_batch("PRAGMA foreign_keys=OFF").unwrap();
    f.db.execute("UPDATE files SET message=?1 WHERE id='plain'", [plain + 100]).unwrap();
    assert_eq!(f.store.expire_orphans().unwrap(), vec!["plain"]);
    assert!(f.exists("sealed"));
    f.db.execute("UPDATE files SET sealed_message=?1 WHERE id='sealed'", params![sealed.id + 100]).unwrap();
    assert_eq!(f.store.expire_orphans().unwrap(), vec!["sealed"]);
}
