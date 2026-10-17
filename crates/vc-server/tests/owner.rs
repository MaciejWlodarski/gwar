//! The server's owner and the managers of private channels (docs/e2e-build.md, section 6, 6b).

mod common;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use gwar_mls::Credential;
use gwar_mls::owner::{Anchor, Genesis, ManagerList, OwnerChain, OwnerLink, SignedAnchor, SignedLink, SignedList};
use serde_json::{Value, json};

fn now() -> i64 {
    vc_server::core::now_ms()
}

/// The chain as the server reports it, verified.
async fn chain(client: &mut Client) -> OwnerChain {
    let page = client.ok("owner.chain", json!({"generation": 1, "after": 0})).await;
    let genesis = Genesis::parse(page["genesis"].as_str().unwrap()).unwrap();
    let links = page["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| SignedLink::parse(l["statement"].as_str().unwrap(), l["signature"].as_str().unwrap()).unwrap())
        .collect();
    OwnerChain::verify(genesis, links).expect("the server's chain verifies")
}

fn link(chain: &OwnerChain, from: &Client, to: &str, anchor: Option<&SignedAnchor>) -> SignedLink {
    let link = OwnerLink {
        server_key: chain.genesis().server_key.clone(),
        generation: chain.generation(),
        seq: chain.seq() + 1,
        previous_link_hash: chain.head(),
        previous_owner_account_key: B64.encode(from.key.verifying_key().as_bytes()),
        new_owner_uid: to.into(),
        anchor_hash: anchor.map(SignedAnchor::hash),
        signed_at: now(),
    };
    SignedLink::sign(link, &from.key)
}

fn transfer(link: &SignedLink) -> Value {
    json!({"statement": link.link().statement(), "signature": link.signature()})
}

fn anchor(chain: &OwnerChain, owner: &Client, seq: u64, confirmers: &[&str]) -> SignedAnchor {
    let mut confirmers: Vec<String> = confirmers.iter().map(|c| c.to_string()).collect();
    confirmers.sort();
    let a = Anchor {
        server_key: chain.genesis().server_key.clone(),
        generation: chain.generation(),
        owner_head: chain.head(),
        anchor_seq: seq,
        confirmers,
        signed_at: now(),
    };
    SignedAnchor::sign(a, &owner.key)
}

/// A list signed by `signer`, a local identity (its device key is its account).
fn list(chain: &OwnerChain, anchor: &SignedAnchor, seq: u64, managers: &[&str], signer: &Client) -> SignedList {
    let mut managers: Vec<String> = managers.iter().map(|m| m.to_string()).collect();
    managers.sort();
    let l = ManagerList {
        server_key: chain.genesis().server_key.clone(),
        generation: chain.generation(),
        anchor_hash: anchor.hash(),
        seq,
        managers,
        signer_uid: signer.welcome.uid.clone(),
        signed_at: now(),
    };
    let credential = Credential::Local { device_key: B64.encode(signer.key.verifying_key().as_bytes()) };
    SignedList::sign(l, credential, &signer.key)
}

fn update(anchor: &SignedAnchor, list: &SignedList) -> Value {
    json!({"anchor": B64.encode(anchor.encode()), "list": B64.encode(list.encode())})
}

#[tokio::test]
async fn the_first_admin_token_makes_the_owner_and_others_never_do() {
    let server = TestServer::start().await;
    let mut root = Client::connect(&server, "root").await;
    assert!(root.welcome.owner.is_none(), "a new server has no owner");
    root.redeem(&server.admin_token).await;
    let owned = root.next_event("owner.updated").await;
    assert_eq!(
        (owned["uid"].as_str(), owned["generation"].as_u64(), owned["seq"].as_u64()),
        (Some(root.welcome.uid.as_str()), Some(1), Some(0))
    );
    // Reported to newcomers, with an empty manager view.
    let mut alice = Client::connect(&server, "alice").await;
    let owner = alice.welcome.owner.clone().unwrap();
    assert_eq!(owner.uid, root.welcome.uid);
    let managers = alice.welcome.managers.clone().unwrap();
    assert!(managers.anchor.is_none() && managers.list.is_none());
    assert_eq!(chain(&mut alice).await.owner(), root.welcome.uid);
    // Another admin token grants Admin only.
    let token = root.ok("token.create", json!({"group": 1})).await["token"].as_str().unwrap().to_owned();
    alice.redeem(&token).await;
    let later = Client::connect(&server, "later").await;
    assert_eq!(later.welcome.owner.unwrap().uid, root.welcome.uid);
}

#[tokio::test]
async fn ownership_moves_only_by_a_link_from_the_current_owner_against_the_current_head() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let bob = Client::connect(&server, "bob").await;
    let c = chain(&mut root).await;
    // Signed by someone else, or naming an anchor the server does not have.
    assert_eq!(root.fails("owner.transfer", transfer(&link(&c, &alice, &bob.welcome.uid, None))).await, "bad_request");
    let stray = anchor(&c, &root, 1, &[]);
    assert_eq!(
        root.fails("owner.transfer", transfer(&link(&c, &root, &alice.welcome.uid, Some(&stray)))).await,
        "conflict"
    );
    assert_eq!(root.fails("owner.transfer", transfer(&link(&c, &root, &uid_of(&new_key()), None))).await, "not_found");
    // The owner moves it to Alice, who gets the Admin group.
    let first = link(&c, &root, &alice.welcome.uid, None);
    root.ok("owner.transfer", transfer(&first)).await;
    let moved = alice.next_event("owner.updated").await;
    assert_eq!((moved["uid"].as_str(), moved["seq"].as_u64()), (Some(alice.welcome.uid.as_str()), Some(1)));
    let uid = alice.welcome.uid.clone();
    let me = alice.next_event_where("client.updated", |c| c["uid"] == uid.as_str()).await;
    assert!(me["groups"].as_array().unwrap().contains(&json!(1)));
    // A link against the old head is refused; the chain verifies with the new link.
    assert_eq!(root.fails("owner.transfer", transfer(&link(&c, &root, &bob.welcome.uid, None))).await, "conflict");
    let c = chain(&mut alice).await;
    assert_eq!((c.seq(), c.owner()), (1, alice.welcome.uid.clone()));
}

#[tokio::test]
async fn managers_update_accepts_only_current_newer_pairs_and_reports_pending_and_stale() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut carol = Client::connect(&server, "carol").await; // a confirmer
    let dave = Client::connect(&server, "dave").await; // a manager
    let editors =
        root.ok("group.create", json!({"name": "Editors", "permissions": ["channel_edit"]})).await["group"]["id"]
            .clone();
    for c in [&carol, &dave] {
        root.ok("member.groups", json!({"uid": c.welcome.uid, "groups": [2, editors]})).await;
    }
    let pending = root.next_event_where("managers.updated", |m| m["pending"].as_array().unwrap().len() == 2).await;
    assert!(pending["anchor"].is_null());

    let c = chain(&mut root).await;
    let a1 = anchor(&c, &root, 1, &[&carol.welcome.uid]);
    root.ok("managers.update", update(&a1, &list(&c, &a1, 1, &[&dave.welcome.uid], &root))).await;
    let view = root.next_event_where("managers.updated", |m| !m["anchor"].is_null()).await;
    assert_eq!(view["pending"], json!([]));
    // A confirmer renews it; a manager or an older seq cannot.
    carol.ok("managers.update", update(&a1, &list(&c, &a1, 2, &[&dave.welcome.uid], &carol))).await;
    assert_eq!(root.fails("managers.update", update(&a1, &list(&c, &a1, 3, &[], &dave))).await, "bad_request");
    assert_eq!(root.fails("managers.update", update(&a1, &list(&c, &a1, 1, &[], &root))).await, "conflict");
    assert_eq!(root.fails("managers.update", update(&a1, &list(&c, &a1, 2, &[], &root))).await, "conflict");
    // Losing `channel_edit` makes Dave stale.
    root.ok("member.groups", json!({"uid": dave.welcome.uid, "groups": [2]})).await;
    let stale = root.next_event_where("managers.updated", |m| m["stale"] == json!([dave.welcome.uid])).await;
    assert_eq!(stale["pending"], json!([]));
    // A newcomer sees the pair, and it verifies against the chain.
    let fresh = Client::connect(&server, "fresh").await;
    let m = fresh.welcome.managers.unwrap();
    let a = SignedAnchor::parse(&B64.decode(m.anchor.unwrap()).unwrap()).unwrap();
    let l = SignedList::parse(&B64.decode(m.list.unwrap()).unwrap()).unwrap();
    assert!(c.list_current(&a, &l, now(), &()));
    assert_eq!(l.list().seq, 2);
}

#[tokio::test]
async fn a_transfer_hands_over_only_the_current_anchor_and_a_former_owner_mints_none() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut alice = Client::connect(&server, "alice").await;
    let carol = Client::connect(&server, "carol").await;
    let c = chain(&mut root).await;
    let x = anchor(&c, &root, 1, &[&carol.welcome.uid]);
    root.ok("managers.update", update(&x, &list(&c, &x, 1, &[], &root))).await;
    // Without naming X, the move is refused.
    assert_eq!(root.fails("owner.transfer", transfer(&link(&c, &root, &alice.welcome.uid, None))).await, "conflict");
    root.ok("owner.transfer", transfer(&link(&c, &root, &alice.welcome.uid, Some(&x)))).await;
    let c = chain(&mut alice).await;
    // The former owner's new anchor is not current; the new owner renews X's list.
    let z = anchor(&OwnerChain::verify(c.genesis().clone(), vec![]).unwrap(), &root, 9, &[]);
    assert_eq!(root.fails("managers.update", update(&z, &list(&c, &z, 2, &[], &root))).await, "bad_request");
    assert_eq!(root.fails("managers.update", update(&x, &list(&c, &x, 2, &[], &root))).await, "bad_request");
    alice.ok("managers.update", update(&x, &list(&c, &x, 2, &[], &alice))).await;
}

#[tokio::test]
async fn a_transfer_to_a_listed_manager_survives_a_restart() {
    let dir = tempfile::tempdir().unwrap();
    let (database, data_dir) = (dir.path().join("vc.sqlite3"), dir.path().to_path_buf());
    let config = || vc_server::Config {
        database: Some(database.clone()),
        data_dir: Some(data_dir.clone()),
        ..common::base_config()
    };
    let mut running = vc_server::start(config()).await.unwrap();
    let admin_token = running.admin_token.take().unwrap();
    let server = TestServer { running, admin_token };
    let mut root = admin(&server, "root").await;
    let dave = Client::connect(&server, "dave").await;
    let c = chain(&mut root).await;
    let x = anchor(&c, &root, 1, &[]);
    root.ok("managers.update", update(&x, &list(&c, &x, 1, &[&dave.welcome.uid], &root))).await;
    root.ok("owner.transfer", transfer(&link(&c, &root, &dave.welcome.uid, Some(&x)))).await;
    let uid = dave.welcome.uid.clone();
    root.close().await;
    dave.close().await;
    drop(server);
    // The stored chain and pair still verify: the server starts, with Dave as the owner.
    let server = TestServer { running: vc_server::start(config()).await.unwrap(), admin_token: String::new() };
    let fresh = Client::connect(&server, "fresh").await;
    assert_eq!(fresh.welcome.owner.unwrap().uid, uid);
    assert!(fresh.welcome.managers.unwrap().list.is_some());
}

#[tokio::test]
async fn a_newcomer_given_channel_edit_by_default_is_reported_pending() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    // The default Member role may edit channels.
    root.ok("group.update", json!({"group": 2, "permissions": ["channel_edit"]})).await;
    let newcomer = Client::connect(&server, "newcomer").await;
    let uid = newcomer.welcome.uid.clone();
    root.next_event_where("managers.updated", |m| m["pending"].as_array().unwrap().contains(&json!(uid))).await;
}

#[tokio::test]
async fn the_owner_is_admitted_to_private_channels_whatever_their_roles() {
    let server = TestServer::start().await;
    let mut root = admin(&server, "root").await;
    let mut other = Client::connect(&server, "other").await;
    root.ok("member.groups", json!({"uid": other.welcome.uid, "groups": [1, 2]})).await;
    let private = channel_id(&other.create_channel(json!({"name": "p", "private": true})).await);
    // Another Admin takes the owner's Admin role away: the owner still sees the channel.
    other.ok("member.groups", json!({"uid": root.welcome.uid, "groups": [2]})).await;
    let late = Client::connect_as(&server, new_key(), "probe", None).await.unwrap();
    assert!(late.welcome.channels.iter().all(|c| c.id != private), "others do not");
    root.ok("channel.join", json!({"channel": private})).await;
    assert!(root.drain_events("channel.hidden").await.is_empty());
}

#[tokio::test]
async fn the_first_token_owner_sees_private_channels_made_before_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("vc.sqlite3");
    let server = TestServer::start_with(|c| c.database = Some(database.clone())).await;
    // Another admin, by a later token, makes a private channel while the first token is unused.
    let store = vc_server::store::Store::open(&database).unwrap();
    let later = vc_server::core::issue_token(&store, vc_server::store::ADMIN_GROUP, None).unwrap();
    let mut other = Client::connect(&server, "other").await;
    other.redeem(&later).await;
    let private = channel_id(&other.create_channel(json!({"name": "p", "private": true})).await);
    let mut root = Client::connect(&server, "root").await;
    assert!(root.welcome.channels.iter().all(|c| c.id != private));
    root.redeem(&server.admin_token).await;
    assert_eq!(root.next_event("channel.created").await["id"], private);
}
