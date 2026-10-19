//! Sealed attachment HTTP and actor paths, using the real MLS fixtures.
use super::*;
use gwar_proto::Attachment;

pub(super) async fn upload(server: &TestServer, client: &mut Client, sealed: bool) -> Attachment {
    // Recognizable image bytes must remain opaque when sealed.
    let bytes = b"GIF89a\x01\x00\x02\x00\x00\x00\x00";
    let slot = client
        .ok(
            "file.upload",
            json!({"name":"secret.gif","mime":"image/gif","size":bytes.len(),"sha256":hash(bytes),"sealed":sealed}),
        )
        .await;
    let response = reqwest::Client::new()
        .put(format!("{}{}", server.http(), slot["upload_url"].as_str().unwrap()))
        .body(bytes.to_vec())
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    response.json().await.unwrap()
}

/// Waits until a deleted file's bytes are unlinked and `uploader` is charged `charged` bytes again.
pub(super) async fn released(path: &std::path::Path, database: &std::path::Path, uploader: &str, charged: u64) {
    let store = gwar_server::store::Store::open(database).unwrap();
    for _ in 0..200 {
        if !path.exists() && store.stored_bytes(Some(uploader)).unwrap() == charged {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("{} still on disk or still charged", path.display());
}

async fn get(server: &TestServer, file: &Attachment, token: Option<&str>) -> reqwest::Response {
    let request = reqwest::Client::new().get(format!("{}/files/{}/file", server.http(), file.id));
    let request = if let Some(token) = token { request.bearer_auth(token) } else { request };
    request.send().await.unwrap()
}

fn with_files(conversation: &str, group: &str, id: u32, files: &[&str]) -> Value {
    let mut request = sealed(conversation, group, 0, id);
    request["files"] = json!(files);
    request
}

#[tokio::test]
async fn sealed_upload_is_opaque_and_checks_ciphertext_size_hash_and_limits() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("db");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.files_dir = Some(directory.path().join("files"));
        c.upload_limit = 100;
        c.upload_quota = 200;
    })
    .await;
    let mut a = Client::connect(&server, "alice").await;
    let file = upload(&server, &mut a, true).await;
    assert_eq!((&*file.name, &*file.mime, file.width, file.height), ("file", "application/octet-stream", None, None));
    let db = rusqlite::Connection::open(database).unwrap();
    assert!(db.query_row("SELECT sealed FROM files WHERE id=?1", [&file.id], |r| r.get::<_, bool>(0)).unwrap());
    let response = get(&server, &file, None).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/octet-stream");
    assert!(response.headers()["content-disposition"].to_str().unwrap().starts_with("attachment;"));
    assert_eq!(hash(&response.bytes().await.unwrap()), file.sha256);
    let plain = upload(&server, &mut a, false).await;
    assert_eq!((&*plain.mime, plain.width, plain.height), ("image/gif", Some(1), Some(2)));
    for body in [b"bad".to_vec(), b"ab".to_vec(), b"abcd".to_vec()] {
        let slot =
            a.ok("file.upload", json!({"name":"x","mime":"x","size":3,"sha256":hash(b"abc"),"sealed":true})).await;
        let response = reqwest::Client::new()
            .put(format!("{}{}", server.http(), slot["upload_url"].as_str().unwrap()))
            .body(body)
            .send()
            .await
            .unwrap();
        assert!(matches!(response.status().as_u16(), 400 | 413));
    }
    let reservation = |size| json!({"name":"x","mime":"x","size":size,"sha256":hash(b"x"),"sealed":true});
    assert_eq!(a.fails("file.upload", reservation(101)).await, "too_large");
    a.ok("file.upload", reservation(100)).await;
    assert_eq!(a.fails("file.upload", reservation(100)).await, "quota_exceeded");
}

#[tokio::test]
async fn sealed_file_ties_refuse_wrong_mode_owner_pending_reuse_duplicates_and_over_ten() {
    let directory = tempfile::tempdir().unwrap();
    let server = TestServer::start_with(|c| c.files_dir = Some(directory.path().into())).await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &Device::new(2, 21)).await;
    let conversation = gwar_proto::dm_conversation(&uid(1), &uid(2));
    let group = create(&mut a, &mut alice, 2, 1).await;
    let plain = upload(&server, &mut a, false).await;
    let own = upload(&server, &mut a, true).await;
    let other = upload(&server, &mut b, true).await;
    let pending =
        a.ok("file.upload", json!({"name":"x","mime":"x","size":3,"sha256":hash(b"abc"),"sealed":true})).await;
    let mut files = Vec::new();
    for _ in 0..11 {
        files.push(upload(&server, &mut a, true).await.id);
    }
    let mut cases = vec![
        vec![plain.id.as_str()],
        vec![other.id.as_str()],
        vec![pending["file"].as_str().unwrap()],
        vec![own.id.as_str(), own.id.as_str()],
        vec![own.id.as_str(), "missing"],
    ];
    cases.push(files.iter().map(String::as_str).collect());
    for case in cases {
        assert_eq!(a.fails("chat.sealed.send", with_files(&conversation, &group, 1, &case)).await, "bad_request");
    }
    let channel = a.default_channel();
    let sig = a.signature(&format!("channel:{channel}"), "file", &[], std::slice::from_ref(&own));
    assert_eq!(
        a.fails(
            "chat.send",
            json!({"target":{"channel":channel},"text":"file","attachments":[own.id],"signature":sig})
        )
        .await,
        "not_found"
    );
    // Failed attempts did not reserve the identity or consume a valid file.
    a.ok("chat.sealed.send", with_files(&conversation, &group, 1, &[&own.id])).await;
    assert_eq!(a.fails("chat.sealed.send", with_files(&conversation, &group, 2, &[&own.id])).await, "bad_request");
    a.ok(
        "chat.sealed.send",
        with_files(&conversation, &group, 2, &files[..10].iter().map(String::as_str).collect::<Vec<_>>()),
    )
    .await;
}

#[tokio::test]
async fn sealed_files_use_conversation_access_and_wait_for_active_downloads_on_delete() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("db");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.files_dir = Some(directory.path().join("files"));
    })
    .await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    a.redeem(&server.admin_token).await;
    let b = connect_device(&server, &Device::new(2, 21)).await;
    let outsider = connect_device(&server, &Device::new(3, 31)).await;
    let conversation = gwar_proto::dm_conversation(&uid(1), &uid(2));
    let group = create(&mut a, &mut alice, 2, 1).await;
    let file = upload(&server, &mut a, true).await;
    a.ok("server.update", json!({"file_access":"members"})).await;
    assert_eq!(get(&server, &file, Some(&a.welcome.file_access)).await.status(), 404);
    let mut message = with_files(&conversation, &group, 1, &[&file.id]);
    a.ok("chat.sealed.send", message.clone()).await;
    assert_eq!(get(&server, &file, Some(&a.welcome.file_access)).await.status(), 200);
    assert_eq!(get(&server, &file, Some(&b.welcome.file_access)).await.status(), 200);
    assert_eq!(get(&server, &file, Some(&outsider.welcome.file_access)).await.status(), 404);
    assert_eq!(get(&server, &file, None).await.status(), 404);
    assert_eq!(get(&server, &file, Some("invalid")).await.status(), 404);
    a.ok("server.update", json!({"file_access":"link"})).await;
    assert_eq!(get(&server, &file, None).await.status(), 200);
    message.as_object_mut().unwrap().remove("files");
    message["revision"] = json!(1);
    a.ok("chat.sealed.edit", message.clone()).await;
    assert_eq!(get(&server, &file, None).await.status(), 200);
    let (_, path, reading) = server.running.core.file_open(file.id.clone(), None).await.unwrap();
    message["revision"] = json!(2);
    a.ok("chat.delete_own", message).await;
    assert_eq!(get(&server, &file, None).await.status(), 404);
    let store = gwar_server::store::Store::open(&database).unwrap();
    assert!(store.file(&file.id).unwrap().is_none());
    assert!(path.exists());
    assert_eq!(store.stored_bytes(Some(&uid(1))).unwrap(), file.size);
    drop(reading);
    for _ in 0..100 {
        if store.stored_bytes(Some(&uid(1))).unwrap() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(!path.exists());
    assert_eq!(store.stored_bytes(Some(&uid(1))).unwrap(), 0);
}

#[tokio::test]
async fn sealed_channel_files_follow_read_access_moderator_removal_and_channel_deletion() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("db");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.files_dir = Some(directory.path().join("files"));
    })
    .await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    a.redeem(&server.admin_token).await;
    let mut b = connect_device(&server, &Device::new(2, 21)).await;
    let channel = channel_id(&a.create_channel(json!({"name":"sealed","encrypted":true})).await);
    let group = create_channel_group(&mut a, &mut alice, channel, true, 1).await.unwrap();
    let conversation = format!("channel:{channel}");
    a.ok("server.update", json!({"file_access":"members"})).await;
    for id in 1..=2 {
        let file = upload(&server, &mut a, true).await;
        let sent = a.ok("chat.sealed.send", with_files(&conversation, &group, id, &[&file.id])).await;
        assert_eq!(get(&server, &file, Some(&b.welcome.file_access)).await.status(), 200);
        assert_eq!(get(&server, &file, None).await.status(), 404);
        if id == 1 {
            let ban = a.ok("ban.create", json!({"client":b.id(),"reason":"test"})).await;
            assert_eq!(get(&server, &file, Some(&b.welcome.file_access)).await.status(), 404);
            a.ok("ban.delete", json!({"ban":ban["ban"]["id"]})).await;
            b = connect_device(&server, &Device::new(2, 21)).await;
            a.ok("chat.sealed.remove", json!({"conversation":conversation,"id":sent["id"]})).await;
        } else {
            a.ok("channel.delete", json!({"channel":channel})).await;
        }
        assert_eq!(get(&server, &file, Some(&b.welcome.file_access)).await.status(), 404);
        assert!(server.running.core.file_info(file.id.clone()).await.is_none());
        released(&directory.path().join("files").join(&file.id), &database, &uid(1), 0).await;
    }
}
