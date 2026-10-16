//! Delivery service integration against real OpenMLS clients and a local server.

mod common;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use ed25519_dalek::{Signer as _, SigningKey};
use gwar_mls::{
    Credential, DESCRIPTOR_EXTENSION, Descriptor, Kind, SUITE, SignedDescriptor,
    ds::{Observer, commit_aad},
    history::{Chunk, DeviceHpkeKey, Header, hpke_key_pair},
    signer::{Ed25519Signer, server_credential},
};
use openmls::prelude::{
    tls_codec::{Deserialize as _, Serialize as _},
    *,
};
use openmls_rust_crypto::OpenMlsRustCrypto;
use serde_json::{Value, json};
use vc_server::core::{RevokedDevice, now_ms};

fn now() -> i64 {
    now_ms()
}
fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}
fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}
fn uid(seed: u8) -> String {
    uid_of(&key(seed))
}
fn credential(account: u8, device: u8) -> Credential {
    Credential::Account(
        serde_json::from_value(certificate(&key(account), &key(device), now() - 1000, now() + 29 * 86_400_000))
            .unwrap(),
    )
}

fn capabilities() -> Capabilities {
    Capabilities::new(
        None,
        Some(&[SUITE]),
        Some(&[ExtensionType::Unknown(DESCRIPTOR_EXTENSION), ExtensionType::LastResort]),
        None,
        Some(&[CredentialType::Basic]),
    )
}

fn join_config() -> MlsGroupJoinConfig {
    MlsGroupJoinConfig::builder()
        .use_ratchet_tree_extension(true)
        .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
        .build()
}

fn wire(message: impl Into<MlsMessageOut>) -> Vec<u8> {
    message.into().tls_serialize_detached().unwrap()
}

/// A device of account `account`, with device key seed `device`.
struct Device {
    provider: OpenMlsRustCrypto,
    signer: Ed25519Signer,
    credential: Credential,
    time: Option<i64>,
    group: Option<MlsGroup>,
}

impl Device {
    fn new(account: u8, device: u8) -> Self {
        Self {
            provider: OpenMlsRustCrypto::default(),
            signer: Ed25519Signer(key(device)),
            credential: credential(account, device),
            time: None,
            group: None,
        }
    }

    fn with_key(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential: BasicCredential::new(self.credential.encode().into_bytes()).into(),
            signature_key: self.signer.0.verifying_key().to_bytes().to_vec().into(),
        }
    }

    fn key_package(&self) -> Vec<u8> {
        self.key_package_for(Lifetime::default())
    }

    fn key_package_for(&self, lifetime: Lifetime) -> Vec<u8> {
        self.package(lifetime, false)
    }

    fn package(&self, lifetime: Lifetime, last_resort: bool) -> Vec<u8> {
        let builder = KeyPackage::builder();
        let builder = if last_resort { builder.mark_as_last_resort() } else { builder };
        let bundle = builder
            .key_package_lifetime(lifetime)
            .leaf_node_capabilities(capabilities())
            .build(SUITE, &self.provider, &self.signer, self.with_key())
            .unwrap();
        bundle.key_package().tls_serialize_detached().unwrap()
    }

    fn create(&mut self, descriptor: &SignedDescriptor, external_sender: &str) {
        let extensions = Extensions::from_vec(vec![
            Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
                &[ExtensionType::Unknown(DESCRIPTOR_EXTENSION)],
                &[],
                &[CredentialType::Basic],
            )),
            Extension::Unknown(DESCRIPTOR_EXTENSION, UnknownExtension(descriptor.encode())),
            Extension::ExternalSenders(vec![ExternalSender::new(
                B64.decode(external_sender).unwrap().into(),
                BasicCredential::new(server_credential(external_sender)).into(),
            )]),
        ])
        .unwrap();
        let config = MlsGroupCreateConfig::builder()
            .ciphersuite(SUITE)
            .capabilities(capabilities())
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .with_group_context_extensions(extensions)
            .build();
        self.group = Some(
            MlsGroup::new_with_group_id(
                &self.provider,
                &self.signer,
                &config,
                GroupId::from_slice(&descriptor.group_id()),
                self.with_key(),
            )
            .unwrap(),
        );
    }

    /// The GroupInfo of the current epoch; channel groups leave the tree out after creation.
    fn group_info(&self) -> Vec<u8> {
        let group = self.group.as_ref().unwrap();
        let context = group.public_group().group_context();
        let descriptor =
            SignedDescriptor::parse(&context.extensions().unknown(DESCRIPTOR_EXTENSION).unwrap().0).unwrap();
        let tree = descriptor.descriptor.kind == Kind::Dm || context.epoch().as_u64() == 0;
        wire(group.export_group_info(self.provider.crypto(), &self.signer, tree).unwrap())
    }

    /// Adds devices by key package; returns (commit, welcome, group info) and merges.
    fn add(&mut self, packages: &[Vec<u8>]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
        let packages: Vec<KeyPackage> = packages
            .iter()
            .map(|p| {
                KeyPackageIn::tls_deserialize_exact(p)
                    .unwrap()
                    .validate(self.provider.crypto(), ProtocolVersion::Mls10)
                    .unwrap()
            })
            .collect();
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().unwrap();
        group.set_aad(commit_aad(self.time.unwrap_or_else(now)));
        let (commit, welcome, _) = group.add_members(provider, signer, &packages).unwrap();
        group.merge_pending_commit(provider).unwrap();
        let (commit, welcome) = (wire(commit), wire(welcome));
        (commit, welcome, self.group_info())
    }

    fn update(&mut self) -> (Vec<u8>, Vec<u8>) {
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().unwrap();
        group.set_aad(commit_aad(self.time.unwrap_or_else(now)));
        let commit = group.self_update(provider, signer, LeafNodeParameters::default()).unwrap().into_commit();
        group.merge_pending_commit(provider).unwrap();
        (wire(commit), self.group_info())
    }

    /// Joins from a GroupInfo, with the tree separately when it leaves the tree out.
    fn join_external(&mut self, group_info: &[u8], tree: Option<&[u8]>) -> (Vec<u8>, Vec<u8>) {
        let MlsMessageBodyIn::GroupInfo(info) = MlsMessageIn::tls_deserialize_exact(group_info).unwrap().extract()
        else {
            panic!("not a group info")
        };
        let builder = MlsGroup::external_commit_builder();
        let builder = match tree {
            Some(tree) => builder.with_ratchet_tree(RatchetTreeIn::tls_deserialize_exact(tree).unwrap()),
            None => builder,
        };
        let (mut group, bundle) = builder
            .with_config(join_config())
            .with_aad(commit_aad(self.time.unwrap_or_else(now)))
            .build_group(&self.provider, info, self.with_key())
            .unwrap()
            .leaf_node_parameters(LeafNodeParameters::builder().with_capabilities(capabilities()).build())
            .load_psks(self.provider.storage())
            .unwrap()
            .build(self.provider.rand(), self.provider.crypto(), &self.signer, |_| true)
            .unwrap()
            .finalize(&self.provider)
            .unwrap();
        if group.pending_commit().is_some() {
            group.merge_pending_commit(&self.provider).unwrap();
        }
        self.group = Some(group);
        (wire(bundle.into_commit()), self.group_info())
    }

    fn process(&mut self, message: &[u8]) {
        let message = MlsMessageIn::tls_deserialize_exact(message).unwrap().try_into_protocol_message().unwrap();
        let provider = &self.provider;
        let group = self.group.as_mut().unwrap();
        match group.process_message(provider, message).unwrap().into_content() {
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                group.merge_staged_commit(provider, *staged).unwrap()
            }
            ProcessedMessageContent::ProposalMessage(proposal) => {
                group.store_pending_proposal(provider.storage(), *proposal).unwrap()
            }
            _ => panic!("unexpected message"),
        }
    }

    fn commit_pending(&mut self) -> (Vec<u8>, Vec<u8>) {
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().unwrap();
        group.set_aad(commit_aad(self.time.unwrap_or_else(now)));
        let (commit, _, _) = group.commit_to_pending_proposals(provider, signer).unwrap();
        group.merge_pending_commit(provider).unwrap();
        (wire(commit), self.group_info())
    }
}

async fn connect_device(server: &TestServer, device: &Device) -> Client {
    let Credential::Account(cert) = &device.credential else { panic!("account fixture") };
    Client::connect_with(server, device.signer.0.clone(), |raw, key| {
        let mut h = raw.hello(key, "device", None);
        set_certificate(&mut h, serde_json::to_value(cert).unwrap(), key);
        h
    })
    .await
    .unwrap()
}

fn descriptor(device: &Device, other: u8, server: &str, nonce: u8) -> SignedDescriptor {
    SignedDescriptor::sign(
        Descriptor {
            server_key: server.into(),
            conversation: vc_proto::dm_conversation(&device.credential.uid(), &uid(other)),
            kind: Kind::Dm,
            incarnation: 0,
            previous_group_id: None,
            private: false,
            history: true,
            founding_owner: None,
            channel_group_id: None,
            creator_uid: device.credential.uid(),
            created_at: now() - 500,
            creation_nonce: b64(&[nonce; 16]),
        },
        device.credential.clone(),
        &device.signer.0,
    )
}

async fn create(client: &mut Client, device: &mut Device, other: u8, nonce: u8) -> String {
    let d = descriptor(device, other, &client.server_key, nonce);
    device.create(&d, &client.server_key);
    let reply = client
        .ok(
            "mls.group.create",
            json!({"conversation":d.descriptor.conversation,"group_info":b64(&device.group_info())}),
        )
        .await;
    reply["group"].as_str().unwrap().into()
}

fn update_request(group: &str, epoch: u64, update: &(Vec<u8>, Vec<u8>)) -> Value {
    json!({"group":group,"epoch":epoch,"commit":b64(&update.0),"group_info":b64(&update.1)})
}

async fn add(client: &mut Client, alice: &mut Device, bob: &mut Device, group: &str) {
    let epoch = alice.group.as_ref().unwrap().epoch().as_u64();
    let (commit, welcome, info) = alice.add(&[bob.key_package()]);
    client
        .ok(
            "mls.commit",
            json!({"group":group,"epoch":epoch,"commit":b64(&commit),"welcome":b64(&welcome),"group_info":b64(&info)}),
        )
        .await;
    let MlsMessageBodyIn::Welcome(welcome) = MlsMessageIn::tls_deserialize_exact(&welcome).unwrap().extract() else {
        panic!("welcome")
    };
    bob.group = Some(
        StagedWelcome::new_from_welcome(&bob.provider, &join_config(), welcome, None)
            .unwrap()
            .into_group(&bob.provider)
            .unwrap(),
    );
}

async fn revoke(server: &TestServer, account: u8, device: u8) {
    let account_key = public_key(&key(account));
    let device_key = public_key(&key(device));
    let revoked_at = now();
    let signature = b64(&key(account)
        .sign(format!("gwar revoke v1\n{account_key}\n{device_key}\n{revoked_at}").as_bytes())
        .to_bytes());
    server
        .running
        .core
        .revoked(vec![RevokedDevice { seq: 1, account_key, device_key, revoked_at, signature }], 1)
        .await;
    server.running.core.info().await.unwrap();
}

async fn hpke(client: &mut Client, device: &Device, seed: u8) -> String {
    let public = hpke_key_pair(&[seed; 32]).1;
    let published = DeviceHpkeKey::sign(&device.signer.0, &public, now());
    client
        .ok(
            "mls.device.publish",
            json!({"hpke_key":published.hpke_key,"created_at":published.created_at,"signature":published.signature}),
        )
        .await;
    b64(&public)
}

fn chunk(alice: &Device, client: &Client, group: &str, recipient: &Device, hpke: &str) -> Vec<u8> {
    Chunk::seal(
        Header {
            server_key: client.server_key.clone(),
            group_id: vc_proto::decode_fixed(group).unwrap(),
            recipient_device_key: public_key(&recipient.signer.0),
            recipient_hpke_key: hpke.into(),
            sender_device_key: public_key(&alice.signer.0),
            first_epoch: 0,
            last_epoch: 0,
            created_at: now(),
        },
        &[(0, [7; 32])],
        &alice.signer.0,
    )
    .unwrap()
}

#[tokio::test]
async fn packages_are_consumed_paged_and_last_resort_and_revocation_are_respected() {
    let server = TestServer::start().await;
    let alice = Device::new(1, 11);
    let bob = Device::new(2, 21);
    let phone = Device::new(2, 22);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let mut p = connect_device(&server, &phone).await;
    let regular = bob.key_package();
    let last = bob.package(Lifetime::default(), true);
    b.ok("mls.key_packages.publish", json!({"packages":[b64(&regular)],"last_resort":b64(&last)})).await;
    let near = phone.key_package_for(Lifetime::new(899));
    p.ok("mls.key_packages.publish", json!({"packages":[b64(&near)]})).await;
    a.ok("mls.key_packages.publish", json!({"packages":[b64(&alice.key_package())]})).await;
    let one = a.ok("mls.key_packages.claim", json!({"uids":[uid(1),uid(2)],"limit":1})).await;
    assert_eq!(one["packages"].as_array().unwrap().len(), 1);
    assert_eq!(one["packages"][0]["package"], b64(&regular));
    assert!(one["next"].is_null());
    for _ in 0..2 {
        let fallback = a.ok("mls.key_packages.claim", json!({"uids":[uid(2)]})).await;
        assert_eq!(fallback["packages"][0]["package"], b64(&last));
    }
    p.ok("mls.key_packages.publish", json!({"packages":[b64(&phone.key_package())]})).await;
    let first = a.ok("mls.key_packages.claim", json!({"uids":[uid(2)],"limit":1})).await;
    let next = first["next"].as_str().unwrap();
    let second = a.ok("mls.key_packages.claim", json!({"uids":[uid(2)],"limit":1,"after":next})).await;
    assert_eq!(first["packages"].as_array().unwrap().len(), 1);
    assert_eq!(second["packages"].as_array().unwrap().len(), 1);
    assert_ne!(first["packages"][0]["device_key"], second["packages"][0]["device_key"]);
    hpke(&mut b, &bob, 1).await;
    hpke(&mut p, &phone, 2).await;
    let first = a.ok("mls.devices", json!({"uids":[uid(2)],"limit":1})).await;
    let second = a.ok("mls.devices", json!({"uids":[uid(2)],"limit":1,"after":first["next"]})).await;
    assert_eq!(second["devices"].as_array().unwrap().len(), 1);
    assert!(second["next"].is_null());
    revoke(&server, 2, 21).await;
    assert_eq!(a.ok("mls.key_packages.claim", json!({"uids":[uid(2)]})).await["packages"], json!([]));
    let devices = a.ok("mls.devices", json!({"uids":[uid(2)]})).await;
    assert_eq!(devices["devices"].as_array().unwrap().len(), 1);
    assert_eq!(devices["devices"][0]["device_key"], public_key(&phone.signer.0));
    assert_eq!(a.fails("mls.devices", json!({"uids":vec![uid(2);21]})).await, "bad_request");
    assert_eq!(a.fails("mls.devices", json!({"uids":[],"limit":101})).await, "bad_request");
    assert_eq!(
        a.fails("mls.key_packages.publish", json!({"packages":[b64(&phone.key_package())]})).await,
        "bad_request"
    );
}

#[tokio::test]
async fn creation_is_first_wins_and_commits_are_ordered_with_replayable_welcomes() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut carol = Device::new(3, 31);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let mut c = connect_device(&server, &carol).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    let d = descriptor(&bob, 1, &b.server_key, 2);
    bob.create(&d, &b.server_key);
    let conflict = b
        .request(
            "mls.group.create",
            json!({"conversation":d.descriptor.conversation,"group_info":b64(&bob.group_info())}),
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code, vc_proto::ErrorCode::Conflict);
    assert_eq!(conflict.message, group);
    assert_eq!(c.fails("mls.group_info", json!({"group":group})).await, "forbidden");
    let stranger = carol.join_external(&alice.group_info(), None);
    assert_eq!(c.fails("mls.commit", update_request(&group, 0, &stranger)).await, "forbidden");
    assert_eq!(a.fails("mls.commit", update_request(&group, 0, &stranger)).await, "bad_request");
    let join = bob.join_external(&alice.group_info(), None);
    assert_eq!(a.fails("mls.commit", update_request(&group, 0, &join)).await, "forbidden");
    assert_eq!(b.ok("mls.commit", update_request(&group, 0, &join)).await["epoch"], 1);
    alice.process(&join.0);
    let alice_commit = alice.update();
    let bob_commit = bob.update();
    let ar = a.send("mls.commit", update_request(&group, 1, &alice_commit)).await;
    let br = b.send("mls.commit", update_request(&group, 1, &bob_commit)).await;
    let (ar, br) = tokio::join!(a.reply(ar), b.reply(br));
    assert_eq!(usize::from(ar.is_ok()) + usize::from(br.is_ok()), 1);
    assert_eq!(ar.err().or(br.err()).unwrap().code, vc_proto::ErrorCode::Stale);
    let log = a.ok("mls.log", json!({"group":group,"since":0,"limit":1})).await;
    assert_eq!(log["head"], 2);
    assert_eq!(log["oldest"], 0);
    assert_eq!(log["entries"][0]["epoch"], 0);
    assert_eq!(log["next"], 1);
    let end = a.ok("mls.log", json!({"group":group,"since":log["next"],"limit":1})).await;
    assert_eq!(end["entries"][0]["epoch"], 1);
    assert!(end["next"].is_null());
    let empty = a.ok("mls.log", json!({"group":group,"since":2})).await;
    assert_eq!(empty["entries"], json!([]));
    let view = a.ok("mls.group_info", json!({"group":group})).await;
    assert_eq!(view["epoch"], 2);
    let observer = Observer::create(&B64.decode(view["group_info"].as_str().unwrap()).unwrap(), &a.server_key).unwrap();
    assert_eq!(observer.epoch(), 2);
}

#[tokio::test]
async fn welcome_fetch_is_repeatable_paged_and_acknowledgements_are_device_scoped() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut carol = Device::new(3, 31);
    let mut other_bob = Device::new(2, 22);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let mut c = connect_device(&server, &carol).await;
    let mut phone = connect_device(&server, &other_bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    assert_eq!(b.next_event("mls.welcome").await["group"], group);
    let fetched = b.ok("mls.welcomes", json!({})).await;
    assert_eq!(fetched["welcomes"].as_array().unwrap().len(), 1);
    assert_eq!(b.ok("mls.welcomes", json!({})).await, fetched);
    phone.ok("mls.welcomes.ack", json!({"ids":[fetched["welcomes"][0]["id"]]})).await;
    assert_eq!(b.ok("mls.welcomes", json!({})).await, fetched);
    let second = create(&mut c, &mut carol, 2, 2).await;
    add(&mut c, &mut carol, &mut bob, &second).await;
    let first = b.ok("mls.welcomes", json!({"limit":1})).await;
    let last = b.ok("mls.welcomes", json!({"limit":1,"after":first["next"]})).await;
    assert_eq!(first["welcomes"].as_array().unwrap().len(), 1);
    assert_eq!(last["welcomes"].as_array().unwrap().len(), 1);
    assert_ne!(first["welcomes"][0]["id"], last["welcomes"][0]["id"]);
    let ids = json!([first["welcomes"][0]["id"], last["welcomes"][0]["id"]]);
    add(&mut a, &mut alice, &mut other_bob, &group).await;
    let phone_welcome = phone.ok("mls.welcomes", json!({})).await;
    assert_eq!(phone_welcome["welcomes"].as_array().unwrap().len(), 1);
    b.ok("mls.welcomes.ack", json!({"ids":ids})).await;
    assert_eq!(b.ok("mls.welcomes", json!({})).await["welcomes"], json!([]));
}

#[tokio::test]
async fn history_chunks_bind_sender_recipient_key_membership_and_end_requests() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let phone = Device::new(2, 22);
    let carol = Device::new(3, 31);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let mut p = connect_device(&server, &phone).await;
    let mut c = connect_device(&server, &carol).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    let public = hpke(&mut b, &bob, 4).await;
    b.ok("mls.history.request", json!({"group":group})).await;
    assert_eq!(a.next_event("mls.history.wanted").await, json!({"group":group,"device":public_key(&bob.signer.0)}));
    let wanted = a.ok("mls.history.wanted", json!({})).await;
    assert_eq!(wanted["wanted"].as_array().unwrap().len(), 1);
    assert_eq!(b.fails("mls.history.request", json!({"group":group})).await, "rate_limited");
    assert_eq!(c.fails("mls.history.request", json!({"group":group})).await, "forbidden");
    assert_eq!(p.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));
    let bytes = chunk(&alice, &a, &group, &bob, &public);
    let share = json!({"group":group,"recipient":public_key(&bob.signer.0),"chunk":b64(&bytes)});
    assert_eq!(b.fails("mls.history.share", share.clone()).await, "bad_request");
    assert_eq!(p.fails("mls.history.share", share.clone()).await, "forbidden");
    assert_eq!(c.fails("mls.history.share", share.clone()).await, "forbidden");
    let wrong = chunk(&alice, &a, &group, &bob, &b64(&hpke_key_pair(&[5; 32]).1));
    assert_eq!(
        a.fails("mls.history.share", json!({"group":group,"recipient":public_key(&bob.signer.0),"chunk":b64(&wrong)}))
            .await,
        "bad_request"
    );
    let wrong_recipient = json!({"group":group,"recipient":public_key(&phone.signer.0),"chunk":b64(&bytes)});
    assert_eq!(a.fails("mls.history.share", wrong_recipient).await, "forbidden");
    let mut tampered = bytes.clone();
    let n = tampered.len();
    tampered[n - 2] ^= 1;
    assert_eq!(
        a.fails(
            "mls.history.share",
            json!({"group":group,"recipient":public_key(&bob.signer.0),"chunk":b64(&tampered)})
        )
        .await,
        "bad_request"
    );
    a.ok("mls.history.share", share.clone()).await;
    assert_eq!(b.next_event("mls.history").await["group"], group);
    assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));
    assert_eq!(b.fails("mls.history.request", json!({"group":group})).await, "rate_limited");
    a.ok("mls.history.share", share).await;
    assert_eq!(b.ok("mls.history.fetch", json!({})).await["chunks"].as_array().unwrap().len(), 1);
    let different = chunk(&alice, &a, &group, &bob, &public);
    a.ok("mls.history.share", json!({"group":group,"recipient":public_key(&bob.signer.0),"chunk":b64(&different)}))
        .await;
    let first = b.ok("mls.history.fetch", json!({"group":group,"limit":1})).await;
    let second = b.ok("mls.history.fetch", json!({"group":group,"limit":1,"after":first["next"]})).await;
    assert_eq!(first["chunks"].as_array().unwrap().len(), 1);
    assert_eq!(second["chunks"].as_array().unwrap().len(), 1);
    assert!(second["next"].is_null());
    assert_eq!(b.ok("mls.history.fetch", json!({"limit":1})).await, first);
    let imported = Chunk::parse(&B64.decode(first["chunks"][0]["chunk"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(imported.open(&hpke_key_pair(&[4; 32]).0).unwrap(), vec![(0, [7; 32])]);
    a.ok("mls.history.ack", json!({"ids":[first["chunks"][0]["id"]]})).await;
    assert_eq!(b.ok("mls.history.fetch", json!({})).await["chunks"].as_array().unwrap().len(), 2);
    b.ok("mls.history.ack", json!({"ids":[first["chunks"][0]["id"],second["chunks"][0]["id"]]})).await;
    assert_eq!(b.ok("mls.history.fetch", json!({})).await["chunks"], json!([]));
}

#[tokio::test]
async fn removal_proposals_reach_offline_devices_and_are_reissued_after_a_competing_update() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut phone = Device::new(1, 12);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let offline = connect_device(&server, &phone).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    add(&mut a, &mut alice, &mut phone, &group).await;
    offline.close().await;
    let competing = alice.update();
    revoke(&server, 2, 21).await;
    let proposal = a.next_event("mls.proposal").await;
    assert_eq!(proposal["epoch"], 2);
    assert!(a.ok("mls.groups", json!({})).await["groups"][0]["pending"].as_bool().unwrap());
    a.ok("mls.commit", update_request(&group, 2, &competing)).await;
    let reissued = a.next_event("mls.proposal").await;
    assert_eq!(reissued["epoch"], 3);
    assert_ne!(proposal["proposal"], reissued["proposal"]);
    let mut restored = connect_device(&server, &phone).await;
    let log = restored.ok("mls.log", json!({"group":group,"since":2})).await;
    assert_eq!(log["pending"], json!([reissued["proposal"]]));
    phone.process(&competing.0);
    let proposal = B64.decode(reissued["proposal"].as_str().unwrap()).unwrap();
    phone.process(&proposal);
    alice.process(&proposal);
    let removal = alice.commit_pending();
    a.ok("mls.commit", update_request(&group, 3, &removal)).await;
    let log = restored.ok("mls.log", json!({"group":group,"since":3})).await;
    assert_eq!(log["entries"][0]["proposals"], json!([b64(&proposal)]));
    assert_eq!(log["pending"], json!([]));
    phone.process(&removal.0);
    assert_eq!(phone.group.as_ref().unwrap().members().count(), 2);
    assert_eq!(a.ok("mls.groups", json!({})).await["groups"][0]["pending"], false);
    let _ = &mut b;
}

#[tokio::test]
async fn member_removal_and_bans_reconcile_without_deleting_direct_message_groups() {
    for banned in [false, true] {
        let server = TestServer::start().await;
        let mut alice = Device::new(1, 11);
        let mut bob = Device::new(2, 21);
        let mut a = connect_device(&server, &alice).await;
        let mut b = connect_device(&server, &bob).await;
        a.redeem(&server.admin_token).await;
        let group = create(&mut a, &mut alice, 2, 1).await;
        add(&mut a, &mut alice, &mut bob, &group).await;
        if banned {
            a.ok("ban.create", json!({"uid":uid(2)})).await;
        } else {
            a.ok("member.remove", json!({"uid":uid(2),"delete_messages":false})).await;
        }
        let pending = a.ok("mls.log", json!({"group":group,"since":0})).await;
        assert_eq!(pending["pending"].as_array().unwrap().len(), 1);
        assert_eq!(a.ok("mls.groups", json!({})).await["groups"][0]["group"], group);
        assert_eq!(a.ok("mls.group_info", json!({"group":group})).await["epoch"], 1);
        let d = descriptor(&alice, 2, &a.server_key, 3);
        let mut attempted = Device::new(1, 11);
        attempted.create(&d, &a.server_key);
        assert_eq!(
            a.fails(
                "mls.group.create",
                json!({"conversation":d.descriptor.conversation,"group_info":b64(&attempted.group_info())})
            )
            .await,
            "forbidden"
        );
        let _ = &mut b;
    }
}

#[tokio::test]
async fn commit_clock_errors_do_not_mutate_observers_and_the_clock_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.data_dir = Some(dir.path().to_path_buf());
    })
    .await;
    let mut alice = Device::new(1, 11);
    let bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let _b = connect_device(&server, &bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    let state = alice.provider.storage().values.read().unwrap().clone();
    alice.time = Some(now() - 301_000);
    let expired = alice.update();
    assert_eq!(a.fails("mls.commit", update_request(&group, 0, &expired)).await, "expired");
    assert_eq!(a.ok("mls.group_info", json!({"group":group})).await["epoch"], 0);
    let db = rusqlite::Connection::open(&database).unwrap();
    let mark: i64 =
        db.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='mls_commit_clock'", [], |r| r.get(0)).unwrap();
    assert!(mark >= now() - 1000);
    *alice.provider.storage().values.write().unwrap() = state;
    alice.group = Some(
        MlsGroup::load(alice.provider.storage(), &GroupId::from_slice(&vc_proto::decode_fixed::<32>(&group).unwrap()))
            .unwrap()
            .unwrap(),
    );
    alice.time = Some(now());
    let fresh = alice.update();
    // Advance the durable mark as if the system clock had been set forward.
    let high = now() + 600_000;
    db.execute("UPDATE meta SET value=?1 WHERE key='mls_commit_clock'", [high.to_string()]).unwrap();
    a.close().await;
    drop(server);
    drop(db);
    let mut config = base_config();
    config.database = Some(database);
    config.data_dir = Some(dir.path().to_path_buf());
    let running = vc_server::start(config).await.unwrap();
    let server = TestServer { running, admin_token: String::new() };
    let mut restored = connect_device(&server, &Device::new(1, 11)).await;
    assert_eq!(restored.fails("mls.commit", update_request(&group, 0, &expired)).await, "expired");
    assert_eq!(restored.fails("mls.commit", update_request(&group, 0, &fresh)).await, "expired");
    let db = rusqlite::Connection::open(dir.path().join("gwar.sqlite")).unwrap();
    let persisted: i64 =
        db.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='mls_commit_clock'", [], |r| r.get(0)).unwrap();
    assert!(persisted >= high);
}

#[tokio::test]
async fn early_and_backwards_commit_times_are_distinct() {
    for early in [true, false] {
        let server = TestServer::start().await;
        let mut alice = Device::new(1, 11);
        let bob = Device::new(2, 21);
        let mut a = connect_device(&server, &alice).await;
        let _b = connect_device(&server, &bob).await;
        let group = create(&mut a, &mut alice, 2, 1).await;
        alice.time = Some(if early { now() + 301_000 } else { now() - 10_000 });
        let update = alice.update();
        assert_eq!(
            a.fails("mls.commit", update_request(&group, 0, &update)).await,
            if early { "early" } else { "bad_request" }
        );
        assert_eq!(a.ok("mls.log", json!({"group":group,"since":0})).await["head"], 0);
    }
}

#[tokio::test]
async fn decoded_blob_and_authenticated_frame_limits_are_enforced() {
    let server = TestServer::start().await;
    let mut a = Client::connect(&server, "a").await;
    for (op, d) in [
        ("mls.key_packages.publish", json!({"packages":[b64(&vec![0;4*1024+1])]})),
        (
            "mls.group.create",
            json!({"conversation":vc_proto::dm_conversation(&a.welcome.uid,&uid(2)),"group_info":b64(&vec![0;128*1024+1])}),
        ),
    ] {
        assert_eq!(a.fails(op, d).await, "too_large");
    }
    // This request exceeds the old 96 KiB transport cap and reaches validation.
    assert_eq!(
        a.fails("mls.group.create", json!({"conversation":"invalid","group_info":b64(&vec![0;128*1024])})).await,
        "bad_request"
    );
    let text = "x".repeat(640 * 1024 + 1);
    a.send_raw(&text).await;
    a.expect_disconnect().await;
    let mut raw = open(&server).await;
    raw.send(json!({"id":1,"op":"hello","d":{"junk":"x".repeat(97*1024)}})).await;
    raw.expect_closed().await;
}

#[tokio::test]
async fn group_and_history_request_lists_page_with_next() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut carol = Device::new(3, 31);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let mut c = connect_device(&server, &carol).await;
    let one = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &one).await;
    b.ok("mls.history.request", json!({"group":one})).await;
    let two = create(&mut a, &mut alice, 3, 2).await;
    add(&mut a, &mut alice, &mut carol, &two).await;
    c.ok("mls.history.request", json!({"group":two})).await;
    for (op, field) in [("mls.groups", "groups"), ("mls.history.wanted", "wanted")] {
        let first = a.ok(op, json!({"limit":1})).await;
        assert_eq!(first[field].as_array().unwrap().len(), 1);
        let second = a.ok(op, json!({"limit":1,"after":first["next"]})).await;
        assert_eq!(second[field].as_array().unwrap().len(), 1);
        assert!(second["next"].is_null());
        assert_ne!(first[field][0]["group"], second[field][0]["group"]);
    }
}

#[tokio::test]
async fn restart_reconciles_cli_member_removal_and_preserves_proposal_references() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.data_dir = Some(dir.path().to_path_buf());
    })
    .await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let b = connect_device(&server, &bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    a.close().await;
    b.close().await;
    drop(server);
    let store = vc_server::store::Store::open(&database).unwrap();
    let id = store.user_by_uid(&uid(2)).unwrap().unwrap().0;
    store.remove_member(id, &uid(2), false).unwrap();
    drop(store);
    let config = || {
        let mut c = base_config();
        c.database = Some(database.clone());
        c.data_dir = Some(dir.path().to_path_buf());
        c
    };
    let running = vc_server::start(config()).await.unwrap();
    let server = TestServer { running, admin_token: String::new() };
    let mut a = connect_device(&server, &alice).await;
    let first = a.ok("mls.log", json!({"group":group,"since":1})).await;
    assert_eq!(first["pending"].as_array().unwrap().len(), 1);
    let proposal = first["pending"][0].clone();
    a.close().await;
    drop(server);
    let running = vc_server::start(config()).await.unwrap();
    let server = TestServer { running, admin_token: String::new() };
    let mut a = connect_device(&server, &alice).await;
    assert_eq!(a.ok("mls.log", json!({"group":group,"since":1})).await["pending"], json!([proposal]));
    alice.process(&B64.decode(proposal.as_str().unwrap()).unwrap());
    let removal = alice.commit_pending();
    a.ok("mls.commit", update_request(&group, 1, &removal)).await;
    let log = a.ok("mls.log", json!({"group":group,"since":1})).await;
    assert_eq!(log["entries"][0]["proposals"], json!([proposal]));
    assert_eq!(log["pending"], json!([]));
}

#[tokio::test]
async fn a_failed_commit_transaction_can_be_retried_without_advancing_the_group() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite");
    let server = TestServer::start_with(|c| c.database = Some(database.clone())).await;
    let mut alice = Device::new(1, 11);
    let bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let _b = connect_device(&server, &bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    let update = alice.update();
    let db = rusqlite::Connection::open(database).unwrap();
    db.execute_batch(
        "CREATE TRIGGER refuse_log BEFORE INSERT ON mls_log BEGIN SELECT RAISE(ABORT,'test rollback'); END;",
    )
    .unwrap();
    assert_eq!(a.fails("mls.commit", update_request(&group, 0, &update)).await, "internal");
    assert_eq!(a.ok("mls.log", json!({"group":group,"since":0})).await["head"], 0);
    assert_eq!(a.ok("mls.group_info", json!({"group":group})).await["epoch"], 0);
    assert!(a.drain_events("mls.commit").await.is_empty(), "a rolled-back commit must not broadcast");
    db.execute_batch("DROP TRIGGER refuse_log;").unwrap();
    assert_eq!(a.ok("mls.commit", update_request(&group, 0, &update)).await["epoch"], 1);
}

#[tokio::test]
async fn retention_prunes_blobs_and_requests_but_keeps_groups_and_pending_proposals() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite");
    let server = TestServer::start_with(|c| c.database = Some(database.clone())).await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    let public = hpke(&mut b, &bob, 4).await;
    let bytes = chunk(&alice, &a, &group, &bob, &public);
    a.ok("mls.history.share", json!({"group":group,"recipient":public_key(&bob.signer.0),"chunk":b64(&bytes)})).await;
    b.ok("mls.history.request", json!({"group":group})).await;
    b.ok("mls.key_packages.publish", json!({"packages":[b64(&bob.key_package())]})).await;
    revoke(&server, 2, 21).await;
    let store = vc_server::store::Store::open(&database).unwrap();
    store.mls_prune(now() + 100 * 86_400_000).unwrap();
    for (op, field, d) in
        [("mls.history.wanted", "wanted", json!({})), ("mls.log", "entries", json!({"group":group,"since":0}))]
    {
        assert_eq!(a.ok(op, d).await[field], json!([]));
    }
    let log = a.ok("mls.log", json!({"group":group,"since":0})).await;
    assert_eq!(log["oldest"], log["head"]);
    assert_eq!(log["pending"].as_array().unwrap().len(), 1);
    let db = rusqlite::Connection::open(&database).unwrap();
    for table in ["mls_key_packages", "mls_log", "mls_welcomes", "mls_history", "mls_history_requests"] {
        let count: u32 = db.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0)).unwrap();
        assert_eq!(count, 0, "{table}");
    }
    assert_eq!(a.ok("mls.groups", json!({})).await["groups"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn package_publication_checks_lifetimes_and_rolls_back_over_quota() {
    let server = TestServer::start().await;
    let alice = Device::new(1, 11);
    let bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let packages: Vec<_> = (0..20).map(|_| b64(&bob.key_package())).collect();
    assert_eq!(b.ok("mls.key_packages.publish", json!({"packages":packages})).await["count"], 20);
    let overflow = bob.key_package();
    assert_eq!(b.fails("mls.key_packages.publish", json!({"packages":[b64(&overflow)]})).await, "bad_request");
    let seconds = (now() / 1000) as u64;
    let future = bob.key_package_for(Lifetime::init(seconds + 600, seconds + 3600));
    assert_eq!(b.fails("mls.key_packages.publish", json!({"packages":[b64(&future)]})).await, "bad_request");
    let claim = a.ok("mls.key_packages.claim", json!({"uids":[uid(2)]})).await;
    assert!(packages.iter().any(|p| claim["packages"][0]["package"] == *p));
    assert_eq!(b.ok("mls.key_packages.publish", json!({"packages":[]})).await["count"], 19);
}

#[tokio::test]
async fn commit_and_history_decoded_limits_allow_the_largest_request_through_the_gateway() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    hpke(&mut b, &bob, 4).await;
    for field in ["commit", "group_info", "welcome"] {
        let mut d = json!({"group":group,"epoch":1,"commit":"","group_info":""});
        d[field] = json!(b64(&vec![0; 128 * 1024 + 1]));
        assert_eq!(a.fails("mls.commit", d).await, "too_large");
    }
    assert_eq!(
        a.fails(
            "mls.history.share",
            json!({"group":group,"recipient":public_key(&bob.signer.0),"chunk":b64(&vec![0;48*1024+1])})
        )
        .await,
        "too_large"
    );
    let maximum = b64(&vec![0; 128 * 1024]);
    assert_eq!(
        a.fails("mls.commit", json!({"group":group,"epoch":1,"commit":maximum,"group_info":maximum,"welcome":maximum}))
            .await,
        "bad_request"
    );
    a.ok("ping", json!({})).await;
}

#[tokio::test]
async fn history_share_rejects_self_and_future_epochs_and_returns_quota_errors() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite");
    let server = TestServer::start_with(|c| c.database = Some(database.clone())).await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    let own_hpke = hpke(&mut a, &alice, 3).await;
    let own = chunk(&alice, &a, &group, &alice, &own_hpke);
    assert_eq!(
        a.fails("mls.history.share", json!({"group":group,"recipient":public_key(&alice.signer.0),"chunk":b64(&own)}))
            .await,
        "bad_request"
    );
    let public = hpke(&mut b, &bob, 4).await;
    let mut future = Chunk::parse(&chunk(&alice, &a, &group, &bob, &public)).unwrap().header;
    future.first_epoch = 2;
    future.last_epoch = 2;
    let future = Chunk::seal(future, &[(2, [7; 32])], &alice.signer.0).unwrap();
    assert_eq!(
        a.fails("mls.history.share", json!({"group":group,"recipient":public_key(&bob.signer.0),"chunk":b64(&future)}))
            .await,
        "bad_request"
    );
    assert_eq!(b.ok("mls.history.fetch", json!({})).await["chunks"], json!([]));
    let db = rusqlite::Connection::open(&database).unwrap();
    for i in 0..64u8 {
        db.execute(
            "INSERT INTO mls_history VALUES(?1,?2,?3,?4,0,0,X'00',?5,?6)",
            rusqlite::params![b64(&[i; 16]), group, public_key(&bob.signer.0), uid(1), now(), vec![i; 32]],
        )
        .unwrap();
    }
    let bytes = chunk(&alice, &a, &group, &bob, &public);
    assert_eq!(
        a.fails("mls.history.share", json!({"group":group,"recipient":public_key(&bob.signer.0),"chunk":b64(&bytes)}))
            .await,
        "quota_exceeded"
    );
    assert!(b.drain_events("mls.history").await.is_empty());
}

#[tokio::test]
async fn blob_pages_fit_the_wire_budget_and_resume_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite");
    let server = TestServer::start_with(|c| c.database = Some(database.clone())).await;
    let mut alice = Device::new(1, 11);
    let bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    let db = rusqlite::Connection::open(database).unwrap();
    for i in 0..18u8 {
        let id = b64(&[i; 16]);
        db.execute(
            "INSERT INTO mls_history VALUES(?1,?2,?3,?4,0,0,?5,?6,?7)",
            rusqlite::params![id, group, public_key(&bob.signer.0), uid(1), vec![i; 48 * 1024], now(), vec![i; 32]],
        )
        .unwrap();
        db.execute(
            "INSERT INTO mls_welcomes VALUES(?1,?2,?3,?4,?5)",
            rusqlite::params![id, public_key(&bob.signer.0), group, vec![i; 128 * 1024], now()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO mls_log VALUES(?1,?2,'[]',?3,?4)",
            rusqlite::params![group, i, vec![i; 128 * 1024], now()],
        )
        .unwrap();
    }
    for (op, field) in [("mls.history.fetch", "chunks"), ("mls.welcomes", "welcomes"), ("mls.log", "entries")] {
        let mut after = Value::Null;
        let mut all = Vec::new();
        loop {
            let d = if op == "mls.log" {
                json!({"group":group,"since":after.as_u64().unwrap_or(0)})
            } else if after.is_null() {
                json!({"group":group})
            } else {
                json!({"group":group,"after":after})
            };
            let response = b.ok(op, d).await;
            assert!(serde_json::to_vec(&json!({"re":u32::MAX,"ok":response})).unwrap().len() <= 640 * 1024);
            let values = response[field].as_array().unwrap();
            assert!(!values.is_empty());
            assert!(values.len() <= if field == "chunks" { 9 } else { 3 });
            all.extend(values.iter().map(|v| {
                if field == "entries" { v["epoch"].to_string() } else { v["id"].as_str().unwrap().to_owned() }
            }));
            after = response["next"].clone();
            if after.is_null() {
                break;
            }
        }
        assert_eq!(all.len(), 18);
        let unique: std::collections::BTreeSet<_> = all.into_iter().collect();
        assert_eq!(unique.len(), 18);
    }
}

#[tokio::test]
async fn failed_clock_persistence_does_not_reply_expired_or_announce_a_commit() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("gwar.sqlite");
    let server = TestServer::start_with(|c| c.database = Some(database.clone())).await;
    let mut alice = Device::new(1, 11);
    let bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let _b = connect_device(&server, &bob).await;
    let group = create(&mut a, &mut alice, 2, 1).await;
    alice.time = Some(now() - 301_000);
    let expired = alice.update();
    let db = rusqlite::Connection::open(database).unwrap();
    db.execute_batch("CREATE TRIGGER fail_clock BEFORE INSERT ON meta WHEN new.key='mls_commit_clock' BEGIN SELECT RAISE(ABORT,'test clock failure'); END;").unwrap();
    assert_eq!(a.fails("mls.commit", update_request(&group, 0, &expired)).await, "internal");
    assert_eq!(a.ok("mls.group_info", json!({"group":group})).await["epoch"], 0);
    assert!(a.drain_events("mls.commit").await.is_empty());
    db.execute_batch("DROP TRIGGER fail_clock;").unwrap();
    assert_eq!(a.fails("mls.commit", update_request(&group, 0, &expired)).await, "expired");
    let clock: i64 =
        db.query_row("SELECT CAST(value AS INTEGER) FROM meta WHERE key='mls_commit_clock'", [], |r| r.get(0)).unwrap();
    assert!(clock >= now() - 1000);
}

// ---------------------------------------------------------------- sealed direct messages

fn sealed(conversation: &str, group: &str, epoch: u64, id: u32) -> Value {
    json!({"conversation":conversation,"group":group,"epoch":epoch,"message_id":b64(&id.to_be_bytes().repeat(4)),"ciphertext":b64(b"opaque ciphertext")})
}

#[tokio::test]
async fn sealed_revisions_reach_every_participant_session_and_keep_only_the_latest() {
    use vc_server::core::bridge::{BridgeMsg, BridgeNote};
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    let mut a2 = connect_device(&server, &Device::new(1, 12)).await;
    let mut b = connect_device(&server, &Device::new(2, 21)).await;
    let mut b2 = connect_device(&server, &Device::new(2, 22)).await;
    let mut outsider = connect_device(&server, &Device::new(3, 31)).await;
    let conversation = vc_proto::dm_conversation(&uid(1), &uid(2));
    let group = create(&mut a, &mut alice, 2, 1).await;
    let (bridge, mut notes) = tokio::sync::mpsc::unbounded_channel();
    server.running.core.bridge(BridgeMsg::Attach(bridge)).await;
    a.sync().await;
    while notes.try_recv().is_ok() {}
    let request = sealed(&conversation, &group, 0, 1);
    let sent = a.ok("chat.sealed.send", request.clone()).await;
    assert_eq!(sent["revision"], 0);
    assert_eq!(sent["deleted"], false);
    for client in [&mut a, &mut a2, &mut b, &mut b2] {
        assert_eq!(client.next_event("chat.sealed").await, sent);
    }
    assert!(outsider.drain_events("chat.sealed").await.is_empty());
    assert_eq!(a.fails("chat.sealed.send", request.clone()).await, "conflict");
    assert_eq!(
        b.fails("chat.sealed.send", request.clone()).await,
        "conflict",
        "identity is per conversation, not per author"
    );
    assert_eq!(outsider.fails("chat.sealed.send", request.clone()).await, "forbidden");
    assert_eq!(outsider.fails("chat.sealed.history", json!({"conversation":conversation})).await, "forbidden");
    assert_eq!(
        outsider.fails("chat.read", json!({"conversation":conversation,"message":sent["id"]})).await,
        "forbidden"
    );

    let mut revision = request.clone();
    revision["revision"] = json!(3);
    revision["ciphertext"] = json!(b64(b"edited ciphertext"));
    assert_eq!(b.fails("chat.sealed.edit", revision.clone()).await, "forbidden");
    assert_eq!(b.fails("chat.delete_own", revision.clone()).await, "forbidden");
    let edited = a2.ok("chat.sealed.edit", revision.clone()).await;
    assert_eq!(edited["id"], sent["id"]);
    assert_eq!(edited["sent_at"], sent["sent_at"]);
    assert!(edited["edited_at"].is_i64());
    assert_eq!(a.fails("chat.sealed.edit", revision.clone()).await, "bad_request");
    revision["revision"] = json!(2);
    assert_eq!(a.fails("chat.delete_own", revision.clone()).await, "bad_request");
    revision["revision"] = json!(4);
    revision["ciphertext"] = json!(b64(b"signed deletion ciphertext"));
    let deleted = a.ok("chat.delete_own", revision.clone()).await;
    assert_eq!(deleted["deleted"], true);
    assert_eq!(deleted["revision"], 4);
    revision["revision"] = json!(5);
    for op in ["chat.sealed.edit", "chat.delete_own"] {
        assert_eq!(a.fails(op, revision.clone()).await, "bad_request");
    }
    for client in [&mut a, &mut a2, &mut b, &mut b2] {
        assert_eq!(client.next_event("chat.sealed").await, edited);
        assert_eq!(client.next_event("chat.sealed").await, deleted);
    }
    assert_eq!(b.ok("chat.sealed.history", json!({"conversation":conversation})).await["sealed"], json!([deleted]));
    assert!(
        b.ok("chat.sealed.history", json!({"conversation":conversation,"before":sent["id"]})).await["sealed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(outsider.drain_events("chat.sealed").await.is_empty());
    while let Ok(note) = notes.try_recv() {
        assert!(
            !matches!(note, BridgeNote::Event(e) if matches!(*e, vc_proto::Event::ChatSealed(_) | vc_proto::Event::ChatSealedRemoved { .. }))
        );
    }

    // The same client id is independent in a different conversation.
    let mut other = Device::new(1, 11);
    let other_group = create(&mut a, &mut other, 3, 2).await;
    a.ok("chat.sealed.send", sealed(&vc_proto::dm_conversation(&uid(1), &uid(3)), &other_group, 0, 1)).await;
    let b3 = connect_device(&server, &Device::new(2, 23)).await;
    assert_eq!(b3.welcome.conversations.len(), 1);
    assert_eq!(b3.welcome.conversations[0].conversation, conversation);
}

#[tokio::test]
async fn sealed_writes_require_the_live_epoch_and_no_pending_removal() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let _b = connect_device(&server, &bob).await;
    let conversation = vc_proto::dm_conversation(&uid(1), &uid(2));
    let group = create(&mut a, &mut alice, 2, 1).await;
    let original = sealed(&conversation, &group, 0, 1);
    a.ok("chat.sealed.send", original.clone()).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    for op in ["chat.sealed.send", "chat.sealed.edit", "chat.delete_own"] {
        let mut request = original.clone();
        if op == "chat.sealed.send" {
            request["message_id"] = sealed(&conversation, &group, 0, 2)["message_id"].clone();
        } else {
            request["revision"] = json!(1);
        }
        assert_eq!(a.fails(op, request.clone()).await, "stale");
        request["epoch"] = json!(2);
        assert_eq!(a.fails(op, request.clone()).await, "stale");
        request["epoch"] = json!(1);
        request["group"] = json!(b64(&[9; 32]));
        assert_eq!(a.fails(op, request).await, "stale");
    }
    revoke(&server, 2, 21).await;
    for op in ["chat.sealed.send", "chat.sealed.edit", "chat.delete_own"] {
        let mut request = original.clone();
        request["epoch"] = json!(1);
        request["revision"] = json!(1);
        assert_eq!(a.fails(op, request).await, "pending");
    }
    assert_eq!(
        a.ok("chat.sealed.history", json!({"conversation":conversation})).await["sealed"].as_array().unwrap().len(),
        1
    );
    let log = a.ok("mls.log", json!({"group":group,"since":1})).await;
    for proposal in log["pending"].as_array().unwrap() {
        alice.process(&B64.decode(proposal.as_str().unwrap()).unwrap());
    }
    let commit = alice.commit_pending();
    a.ok("mls.commit", update_request(&group, 1, &commit)).await;
    let mut request = original;
    request["epoch"] = json!(2);
    request["revision"] = json!(2);
    let edited = a.ok("chat.sealed.edit", request.clone()).await;
    assert_eq!(edited["epoch"], 2);
    request["revision"] = json!(3);
    assert_eq!(a.ok("chat.delete_own", request).await["deleted"], true);
    a.ok("chat.sealed.send", sealed(&conversation, &group, 2, 2)).await;
}

#[tokio::test]
async fn sealed_size_encoding_and_history_byte_budget() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    let _b = connect_device(&server, &Device::new(2, 21)).await;
    let conversation = vc_proto::dm_conversation(&uid(1), &uid(2));
    let group = create(&mut a, &mut alice, 2, 1).await;
    let mut request = sealed(&conversation, &group, 0, 1);
    request["ciphertext"] = json!(b64(&vec![0; 24 * 1024 + 1]));
    for op in ["chat.sealed.send", "chat.sealed.edit", "chat.delete_own"] {
        request["revision"] = json!(1);
        assert_eq!(a.fails(op, request.clone()).await, "too_large");
    }
    for ciphertext in ["=", "AA=", "AB", "+/"] {
        request["ciphertext"] = json!(ciphertext);
        assert_eq!(a.fails("chat.sealed.send", request.clone()).await, "bad_request");
    }
    request["ciphertext"] = json!(b64(b"valid"));
    request["message_id"] = json!("not-an-id");
    assert_eq!(a.fails("chat.sealed.send", request.clone()).await, "bad_request");
    request["message_id"] = sealed(&conversation, &group, 0, 1)["message_id"].clone();
    request["conversation"] = json!("server");
    assert_eq!(a.fails("chat.sealed.send", request).await, "bad_request");
    // Fill large messages through the actor (gateway throttling is tested separately).
    for id in 1..=24 {
        let mut request = sealed(&conversation, &group, 0, id);
        request["ciphertext"] = json!(b64(&vec![0; 24 * 1024]));
        server
            .running
            .core
            .request(a.id(), 10000 + id, serde_json::from_value(json!({"op":"chat.sealed.send","d":request})).unwrap())
            .await;
        assert_eq!(a.reply(10000 + id).await.unwrap()["ciphertext"].as_str().unwrap().len(), 32768);
    }
    let page = a.ok("chat.sealed.history", json!({"conversation":conversation,"limit":100})).await;
    assert!(serde_json::to_vec(&page).unwrap().len() < 640 * 1024);
    let messages = page["sealed"].as_array().unwrap();
    assert!(messages.len() < 24 && !messages.is_empty());
    let rest =
        a.ok("chat.sealed.history", json!({"conversation":conversation,"before":messages[0]["id"],"limit":100})).await;
    assert_eq!(rest["sealed"].as_array().unwrap().len() + messages.len(), 24);
}

#[tokio::test]
async fn sealed_cap_paging_unread_and_persistence() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("db.sqlite3");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.data_dir = Some(directory.path().into());
    })
    .await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    let b = connect_device(&server, &Device::new(2, 21)).await;
    let conversation = vc_proto::dm_conversation(&uid(1), &uid(2));
    let group = create(&mut a, &mut alice, 2, 1).await;
    assert!(b.welcome.conversations.is_empty());
    b.close().await;
    let mut last = Value::Null;
    for id in 1..=1001 {
        let request = sealed(&conversation, &group, 0, id);
        server
            .running
            .core
            .request(a.id(), 10000 + id, serde_json::from_value(json!({"op":"chat.sealed.send","d":request})).unwrap())
            .await;
        last = a.reply(10000 + id).await.unwrap();
        // Injected requests bypass the socket; keep it within the idle timeout on slow disks.
        if id % 100 == 0 {
            a.sync().await;
        }
    }
    let store = vc_server::store::Store::open(&database).unwrap();
    assert!(
        store
            .sealed_message(&conversation, sealed(&conversation, &group, 0, 1)["message_id"].as_str().unwrap())
            .unwrap()
            .is_none()
    );
    assert_eq!(a.fails("chat.sealed.send", sealed(&conversation, &group, 0, 1)).await, "conflict");
    let mut before = Value::Null;
    let mut ids = Vec::new();
    loop {
        let page = a.ok("chat.sealed.history", json!({"conversation":conversation,"before":before,"limit":100})).await;
        let messages = page["sealed"].as_array().unwrap();
        if messages.is_empty() {
            break;
        }
        assert!(messages.windows(2).all(|m| m[0]["id"].as_u64() < m[1]["id"].as_u64()));
        before = messages[0]["id"].clone();
        ids.extend(messages.iter().map(|m| m["id"].as_u64().unwrap()));
    }
    assert_eq!(ids.len(), 1000);
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 1000);
    let mut b = connect_device(&server, &Device::new(2, 22)).await;
    assert_eq!(b.welcome.conversations[0].last_read, 0);
    assert_eq!(b.welcome.conversations[0].count, vc_proto::UNREAD_CAP);
    let mut b2 = connect_device(&server, &Device::new(2, 23)).await;
    let mark = last["id"].as_u64().unwrap() - 5;
    b.ok("chat.read", json!({"conversation":conversation,"message":mark})).await;
    assert_eq!(b2.next_event("chat.read").await, json!({"conversation":conversation,"message":mark}));
    b.ok("chat.read", json!({"conversation":conversation,"message":1})).await;
    assert_eq!(b2.next_event("chat.read").await["message"], mark);
    let b3 = connect_device(&server, &Device::new(2, 24)).await;
    assert_eq!(b3.welcome.conversations[0].last_read, u32::try_from(mark).unwrap());
    assert_eq!(b3.welcome.conversations[0].count, 5);
    let default = a.ok("chat.sealed.history", json!({"conversation":conversation})).await;
    assert_eq!(default["sealed"].as_array().unwrap().len(), 50);
    assert_eq!(
        a.ok("chat.sealed.history", json!({"conversation":conversation,"limit":0})).await["sealed"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        a.ok("chat.sealed.history", json!({"conversation":conversation,"limit":1000})).await["sealed"]
            .as_array()
            .unwrap()
            .len(),
        100
    );
    b.ok("chat.read", json!({"conversation":conversation,"message":last["id"]})).await;
    assert!(connect_device(&server, &Device::new(2, 25)).await.welcome.conversations.is_empty());
    store.mls_prune(now() + 100 * 86_400_000).unwrap();
    assert!(store.mls_group(&group).unwrap().is_some());
    assert_eq!(store.sealed_history(&conversation, None, 100).unwrap().len(), 100);
    // A real restart retains ciphertexts and read state.
    drop(server);
    let mut config = base_config();
    config.database = Some(database.clone());
    config.data_dir = Some(directory.path().into());
    let restarted = TestServer { running: vc_server::start(config).await.unwrap(), admin_token: String::new() };
    let mut b = connect_device(&restarted, &Device::new(2, 26)).await;
    assert!(b.welcome.conversations.is_empty());
    assert_eq!(
        b.ok("chat.sealed.history", json!({"conversation":conversation})).await["sealed"]
            .as_array()
            .unwrap()
            .last()
            .unwrap(),
        &last
    );
}

#[tokio::test]
async fn sealed_member_removal_and_pruning_clear_marks_and_optionally_erase_authored_messages() {
    for (delete_messages, prune) in [(false, false), (true, false), (false, true), (true, true)] {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("db.sqlite3");
        let server = TestServer::start_with(|c| {
            c.database = Some(database.clone());
            c.files_dir = Some(directory.path().join("files"));
        })
        .await;
        let mut alice = Device::new(1, 11);
        let mut bob = Device::new(2, 21);
        let mut a = connect_device(&server, &alice).await;
        a.redeem(&server.admin_token).await;
        let mut b = connect_device(&server, &bob).await;
        let mut outsider = connect_device(&server, &Device::new(3, 31)).await;
        let conversation = vc_proto::dm_conversation(&uid(1), &uid(2));
        let group = create(&mut a, &mut alice, 2, 1).await;
        add(&mut a, &mut alice, &mut bob, &group).await;
        let own_file = attachments::upload(&server, &mut a, true).await;
        let peer_file = attachments::upload(&server, &mut b, true).await;
        let pending = attachments::upload(&server, &mut b, true).await;
        let mut own_request = sealed(&conversation, &group, 1, 1);
        own_request["files"] = json!([own_file.id]);
        let mut peer_request = sealed(&conversation, &group, 1, 2);
        peer_request["files"] = json!([peer_file.id]);
        let own = a.ok("chat.sealed.send", own_request).await;
        let peer = b.ok("chat.sealed.send", peer_request).await;
        b.ok("chat.read", json!({"conversation":conversation,"message":peer["id"]})).await;
        let peer_session = b.id();
        b.close().await;
        a.next_event_where("client.left", |e| e["client"] == peer_session).await;
        let db = rusqlite::Connection::open(&database).unwrap();
        if prune {
            db.execute("UPDATE users SET last_seen=?1 WHERE uid=?2", rusqlite::params![now() - 3 * 86_400_000, uid(2)])
                .unwrap();
            a.ok(
                "member.prune",
                json!({"inactive_days":1,"without_groups_only":false,"delete_messages":delete_messages,"dry_run":false}),
            )
            .await;
        } else {
            a.ok("member.remove", json!({"uid":uid(2),"delete_messages":delete_messages})).await;
        }
        let store = vc_server::store::Store::open(&database).unwrap();
        assert!(store.file(&own_file.id).unwrap().is_some());
        assert_eq!(store.file(&peer_file.id).unwrap().is_some(), !delete_messages);
        assert!(store.file(&pending.id).unwrap().is_none());
        assert!(store.expire_unattached(i64::MAX).unwrap().is_empty());
        assert!(store.expire_orphans().unwrap().is_empty());
        assert_eq!(store.file(&peer_file.id).unwrap().is_some(), !delete_messages);
        let files = directory.path().join("files");
        attachments::released(
            &files.join(&pending.id),
            &database,
            &uid(2),
            if delete_messages { 0 } else { peer_file.size },
        )
        .await;
        assert_eq!(files.join(&peer_file.id).exists(), !delete_messages);
        assert!(files.join(&own_file.id).exists());
        let marks: i64 = db.query_row("SELECT COUNT(*) FROM conversation_reads", [], |r| r.get(0)).unwrap();
        assert_eq!(marks, 0);
        let history = a.ok("chat.sealed.history", json!({"conversation":conversation})).await;
        assert_eq!(
            history["sealed"],
            if delete_messages { json!([own.clone()]) } else { json!([own.clone(), peer.clone()]) }
        );
        for op in ["chat.sealed.send", "chat.sealed.edit", "chat.delete_own"] {
            let mut request = sealed(&conversation, &group, 1, 1);
            request["revision"] = json!(1);
            assert_eq!(a.fails(op, request).await, "forbidden");
        }
        a.ok("chat.read", json!({"conversation":conversation,"message":own["id"]})).await;
        assert_eq!(a.ok("mls.log", json!({"group":group,"since":1})).await["pending"].as_array().unwrap().len(), 1);
        if delete_messages {
            assert_eq!(
                a.next_event("chat.sealed.removed").await,
                json!({"conversation":conversation,"ids":[peer["id"]]})
            );
        } else {
            assert!(a.drain_events("chat.sealed.removed").await.is_empty());
        }
        assert!(outsider.drain_events("chat.sealed.removed").await.is_empty());
        let mut b = connect_device(&server, &Device::new(2, 22)).await;
        assert_eq!(b.welcome.conversations[0].last_read, 0);
        assert_eq!(b.welcome.conversations[0].count, if delete_messages { 1 } else { 2 });
        assert_eq!(b.ok("chat.sealed.history", json!({"conversation":conversation})).await, history);
        if delete_messages {
            assert_eq!(b.fails("chat.sealed.send", sealed(&conversation, &group, 1, 2)).await, "pending");
        }
    }
}

#[tokio::test]
async fn sealed_writes_fail_closed_after_proposal_storage_failure_and_retry_reconciliation() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("db.sqlite3");
    let server = TestServer::start_with(|c| c.database = Some(database.clone())).await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let _b = connect_device(&server, &bob).await;
    let conversation = vc_proto::dm_conversation(&uid(1), &uid(2));
    let group = create(&mut a, &mut alice, 2, 1).await;
    add(&mut a, &mut alice, &mut bob, &group).await;
    let original = a.ok("chat.sealed.send", sealed(&conversation, &group, 1, 1)).await;
    a.drain_events("chat.sealed").await;
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute_batch("CREATE TRIGGER fail_proposal BEFORE INSERT ON mls_proposals BEGIN SELECT RAISE(ABORT,'injected proposal failure'); END;").unwrap();
    revoke(&server, 2, 21).await;
    for op in ["chat.sealed.send", "chat.sealed.edit", "chat.delete_own"] {
        let mut request = sealed(&conversation, &group, 1, 1);
        request["revision"] = json!(1);
        assert_eq!(a.fails(op, request).await, "internal");
    }
    assert!(a.drain_events("chat.sealed").await.is_empty());
    assert_eq!(a.ok("chat.sealed.history", json!({"conversation":conversation})).await["sealed"], json!([original]));
    db.execute_batch("DROP TRIGGER fail_proposal").unwrap();
    assert_eq!(a.fails("chat.sealed.send", sealed(&conversation, &group, 1, 2)).await, "pending");
    assert_eq!(a.next_event("mls.proposal").await["group"], group);
}

fn channel_descriptor(device: &Device, channel: u32, history: bool, server: &str, nonce: u8) -> SignedDescriptor {
    SignedDescriptor::sign(
        Descriptor {
            server_key: server.into(),
            conversation: format!("channel:{channel}"),
            kind: Kind::Channel,
            incarnation: 0,
            previous_group_id: None,
            private: false,
            history,
            founding_owner: None,
            channel_group_id: None,
            creator_uid: device.credential.uid(),
            created_at: now() - 500,
            creation_nonce: b64(&[nonce; 16]),
        },
        device.credential.clone(),
        &device.signer.0,
    )
}

/// Registers a new channel group made by `device`; returns the reply or the error code.
async fn create_channel_group(
    client: &mut Client,
    device: &mut Device,
    channel: u32,
    history: bool,
    nonce: u8,
) -> Result<String, String> {
    let d = channel_descriptor(device, channel, history, &client.server_key, nonce);
    device.create(&d, &client.server_key);
    client
        .request(
            "mls.group.create",
            json!({"conversation":d.descriptor.conversation,"group_info":b64(&device.group_info())}),
        )
        .await
        .map(|reply| reply["group"].as_str().unwrap().to_owned())
        .map_err(|e| serde_json::to_value(e.code).unwrap().as_str().unwrap().to_owned())
}

/// Joins `group` by external commit, with the tree from the tree endpoint.
async fn join_channel(server: &TestServer, client: &mut Client, device: &mut Device, group: &str) {
    let info = client.ok("mls.group_info", json!({"group":group})).await;
    let epoch = info["epoch"].as_u64().unwrap();
    let tree = reqwest::Client::new()
        .get(format!("{}/api/mls/{group}/tree", server.http()))
        .bearer_auth(&client.welcome.file_access)
        .send()
        .await
        .unwrap();
    assert_eq!(tree.headers()["x-gwar-epoch"], epoch.to_string().as_str());
    let tree = tree.bytes().await.unwrap();
    let joined = device.join_external(&B64.decode(info["group_info"].as_str().unwrap()).unwrap(), Some(&tree));
    client.ok("mls.commit", update_request(group, epoch, &joined)).await;
}

#[tokio::test]
async fn channel_groups_follow_the_channel_and_every_member_reads_its_sealed_chat() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    a.redeem(&server.admin_token).await;
    let mut b = connect_device(&server, &bob).await;
    let mut c = connect_device(&server, &Device::new(3, 31)).await;

    // Options are fixed at creation; an encrypted channel has no password.
    let secret = a.create_channel(json!({"name":"secret","encrypted":true})).await;
    assert_eq!((secret["encrypted"].clone(), secret["history"].clone()), (json!(true), json!(true)));
    let quiet = a.create_channel(json!({"name":"quiet","encrypted":true,"history":false})).await;
    assert_eq!(quiet["history"], false);
    let open = a.create_channel(json!({"name":"open","history":true})).await;
    assert_eq!((open["encrypted"].clone(), open["history"].clone()), (json!(false), json!(false)));
    assert_eq!(a.fails("channel.create", json!({"name":"x","encrypted":true,"password":"pw"})).await, "bad_request");
    let (secret, quiet, open) = (channel_id(&secret), channel_id(&quiet), channel_id(&open));
    assert_eq!(a.fails("channel.update", json!({"channel":secret,"password":"pw"})).await, "bad_request");
    a.ok("channel.update", json!({"channel":secret,"topic":"t","encrypted":false})).await;
    assert!(b.ok("mls.groups", json!({})).await["groups"].as_array().unwrap().is_empty());

    // Creation rules: the channel's options, an encrypted channel that exists, first wins.
    assert_eq!(
        create_channel_group(&mut a, &mut Device::new(1, 11), secret, false, 1).await,
        Err("bad_request".into())
    );
    assert_eq!(create_channel_group(&mut a, &mut Device::new(1, 11), open, false, 2).await, Err("not_found".into()));
    assert_eq!(create_channel_group(&mut a, &mut Device::new(1, 11), 999, true, 3).await, Err("not_found".into()));
    let group = create_channel_group(&mut b, &mut bob, secret, true, 4).await.unwrap();
    assert_eq!(create_channel_group(&mut a, &mut alice, secret, true, 5).await, Err("conflict".into()));
    create_channel_group(&mut b, &mut Device::new(2, 21), quiet, false, 6).await.unwrap();
    let conversation = format!("channel:{secret}");
    let listed = c.ok("mls.groups", json!({})).await;
    assert_eq!(listed["groups"].as_array().unwrap().len(), 2);
    alice = Device::new(1, 11);
    join_channel(&server, &mut a, &mut alice, &group).await;

    // The conversation's incarnations, under its read rule.
    let history = c.ok("mls.group_history", json!({"conversation":conversation,"limit":1})).await;
    assert_eq!(history["incarnations"][0]["group"], group);
    let descriptor = B64.decode(history["incarnations"][0]["descriptor"].as_str().unwrap()).unwrap();
    let descriptor = SignedDescriptor::parse(&descriptor).unwrap();
    assert_eq!(b64(&descriptor.group_id()), group);
    assert_eq!(descriptor.descriptor.creator_uid, uid(2));
    assert!(history["next"].is_null());
    let dm = vc_proto::dm_conversation(&uid(1), &uid(2));
    assert_eq!(c.fails("mls.group_history", json!({"conversation":dm})).await, "forbidden");

    // Plaintext requests refuse encrypted channels, and sealed ones plaintext channels.
    let text = json!({"target":{"channel":secret},"text":"hi"});
    assert_eq!(a.fails("chat.send", text).await, "bad_request");
    assert_eq!(a.fails("chat.history", json!({"channel":secret})).await, "bad_request");
    assert_eq!(a.fails("chat.read", json!({"channel":secret,"message":1})).await, "bad_request");
    assert_eq!(a.fails("chat.sealed.send", sealed(&format!("channel:{open}"), &group, 1, 1)).await, "bad_request");
    assert_eq!(a.fails("chat.sealed.history", json!({"conversation":"channel:999"})).await, "not_found");
    assert_eq!(a.fails("chat.sealed.history", json!({"conversation":"channel:01"})).await, "bad_request");

    // Every member reads it, in the group or not.
    let sent = b.ok("chat.sealed.send", sealed(&conversation, &group, 1, 1)).await;
    for client in [&mut a, &mut b, &mut c] {
        assert_eq!(client.next_event("chat.sealed").await, sent);
    }
    let second = a.ok("chat.sealed.send", sealed(&conversation, &group, 1, 2)).await;
    assert_eq!(
        c.ok("chat.sealed.history", json!({"conversation":conversation})).await["sealed"].as_array().unwrap().len(),
        2
    );

    // A newcomer starts with everything read; later messages count as unread.
    let mut d = connect_device(&server, &Device::new(4, 41)).await;
    // Every encrypted channel is listed with its read mark, also with nothing unread.
    let mark = d.welcome.conversations.iter().find(|u| u.conversation == conversation).unwrap();
    assert_eq!((mark.last_read, mark.count), (second["id"].as_u64().unwrap() as u32, 0));
    assert!(d.welcome.unread.iter().all(|u| u.channel != secret));
    let third = b.ok("chat.sealed.send", sealed(&conversation, &group, 1, 3)).await;
    d.next_event("chat.sealed").await;
    let d2 = connect_device(&server, &Device::new(4, 42)).await;
    let mark = d2.welcome.conversations.iter().find(|u| u.conversation == conversation).unwrap();
    assert_eq!((mark.last_read, mark.count), (second["id"].as_u64().unwrap() as u32, 1));
    d.ok("chat.read", json!({"conversation":conversation,"message":third["id"]})).await;

    // Moderators remove without a signature; direct messages have no moderators.
    let remove = json!({"conversation":conversation,"id":sent["id"]});
    assert_eq!(c.fails("chat.sealed.remove", remove.clone()).await, "forbidden");
    a.ok("chat.sealed.remove", remove.clone()).await;
    let removed = json!({"conversation":conversation,"ids":[sent["id"]]});
    for client in [&mut b, &mut c] {
        assert_eq!(client.next_event("chat.sealed.removed").await, removed);
    }
    assert_eq!(a.fails("chat.sealed.remove", remove).await, "not_found");
    assert_eq!(a.fails("chat.sealed.remove", json!({"conversation":dm,"id":1})).await, "bad_request");
    // The removed identity cannot be sent again.
    assert_eq!(b.fails("chat.sealed.send", sealed(&conversation, &group, 1, 1)).await, "conflict");

    // Deleting the channel deletes its groups and messages.
    a.ok("channel.delete", json!({"channel":secret})).await;
    assert_eq!(b.fails("mls.group_info", json!({"group":group})).await, "not_found");
    assert_eq!(b.fails("chat.sealed.history", json!({"conversation":conversation})).await, "not_found");
    assert_eq!(c.ok("mls.groups", json!({})).await["groups"].as_array().unwrap().len(), 1);
    let d3 = connect_device(&server, &Device::new(4, 43)).await;
    assert!(d3.welcome.conversations.iter().all(|u| u.conversation != conversation));
}

#[tokio::test]
async fn channel_history_requests_end_only_through_a_chunk_naming_their_assignment() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    a.redeem(&server.admin_token).await;
    let mut b = connect_device(&server, &bob).await;
    let secret = channel_id(&a.create_channel(json!({"name":"secret","encrypted":true})).await);
    let group = create_channel_group(&mut a, &mut alice, secret, true, 1).await.unwrap();
    join_channel(&server, &mut b, &mut bob, &group).await;
    a.next_event("mls.commit").await;
    let public = hpke(&mut b, &bob, 4).await;
    let recipient = public_key(&bob.signer.0);
    b.ok("mls.history.request", json!({"group":group})).await;
    let wanted = a.next_event("mls.history.wanted").await;
    assert_eq!((wanted["group"].clone(), wanted["device"].clone()), (json!(group), json!(recipient)));
    let assignment = wanted["assignment"].as_str().unwrap().to_owned();
    let listed = a.ok("mls.history.wanted", json!({})).await;
    assert_eq!(listed["wanted"], json!([wanted]));
    assert_eq!(b.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));

    // A chunk naming no assignment (or a stale one) is stored but ends nothing.
    for named in [None, Some("stale")] {
        let bytes = chunk(&alice, &a, &group, &bob, &public);
        a.ok("mls.history.share", json!({"group":group,"recipient":recipient,"chunk":b64(&bytes),"assignment":named}))
            .await;
        assert_eq!(b.next_event("mls.history").await["group"], group);
        assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([wanted]));
    }
    // So does a decline by another device or for another assignment.
    b.ok("mls.history.decline", json!({"group":group,"device":recipient,"assignment":assignment})).await;
    a.ok("mls.history.decline", json!({"group":group,"device":recipient,"assignment":"stale"})).await;
    assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([wanted]));

    let bytes = chunk(&alice, &a, &group, &bob, &public);
    a.ok("mls.history.share", json!({"group":group,"recipient":recipient,"chunk":b64(&bytes),"assignment":assignment}))
        .await;
    assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));
    assert_eq!(b.ok("mls.history.fetch", json!({})).await["chunks"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn a_device_rejoining_a_channel_group_ends_its_history_request_and_assignments() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    a.redeem(&server.admin_token).await;
    let mut b = connect_device(&server, &bob).await;
    let secret = channel_id(&a.create_channel(json!({"name":"secret","encrypted":true})).await);
    let group = create_channel_group(&mut a, &mut alice, secret, true, 1).await.unwrap();
    join_channel(&server, &mut b, &mut bob, &group).await;
    a.next_event("mls.commit").await;
    let public = hpke(&mut b, &bob, 4).await;
    let recipient = public_key(&bob.signer.0);
    b.ok("mls.history.request", json!({"group":group})).await;
    let assignment = a.next_event("mls.history.wanted").await["assignment"].as_str().unwrap().to_owned();

    // The same device joins again by external commit, which removes its old leaf:
    // the request was for that leaf, so it ends with its assignment and is not routed again.
    join_channel(&server, &mut b, &mut bob, &group).await;
    a.next_event("mls.commit").await;
    assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));
    let bytes = chunk(&alice, &a, &group, &bob, &public);
    a.ok("mls.history.share", json!({"group":group,"recipient":recipient,"chunk":b64(&bytes),"assignment":assignment}))
        .await;
    assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));
}

/// The next incarnation of `previous`, made by `device`.
fn successor(
    device: &Device,
    previous: &SignedDescriptor,
    incarnation: u64,
    after: &str,
    nonce: u8,
) -> SignedDescriptor {
    let mut d = previous.descriptor.clone();
    d.incarnation = incarnation;
    d.previous_group_id = Some(after.into());
    d.creator_uid = device.credential.uid();
    d.creation_nonce = b64(&[nonce; 16]);
    SignedDescriptor::sign(d, device.credential.clone(), &device.signer.0)
}

#[tokio::test]
async fn a_restart_continues_the_live_group_and_retires_its_predecessor() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &Device::new(2, 21)).await;
    let conversation = vc_proto::dm_conversation(&uid(1), &uid(2));
    let server_key = a.server_key.clone();
    let first = descriptor(&alice, 2, &server_key, 1);
    let old = create(&mut a, &mut alice, 2, 1).await;
    a.ok("chat.sealed.send", sealed(&conversation, &old, 0, 1)).await;
    let mut register = async |d: SignedDescriptor| {
        let mut device = Device::new(1, 11);
        device.create(&d, &server_key);
        let request = json!({"conversation":conversation,"group_info":b64(&device.group_info())});
        a.request("mls.group.create", request)
            .await
            .map(|r| r["group"].as_str().unwrap().to_owned())
            .map_err(|e| e.code)
    };

    // Only the next incarnation, naming the live group, is accepted.
    for (incarnation, previous) in [(2, old.clone()), (1, b64(&[9; 32]))] {
        let d = successor(&alice, &first, incarnation, &previous, 2);
        assert_eq!(register(d).await, Err(vc_proto::ErrorCode::BadRequest));
    }
    let next = register(successor(&alice, &first, 1, &old, 3)).await.unwrap();
    assert_eq!(register(successor(&alice, &first, 1, &old, 4)).await, Err(vc_proto::ErrorCode::Conflict));
    assert_eq!(register(descriptor(&alice, 2, &server_key, 5)).await, Err(vc_proto::ErrorCode::Conflict));

    // The live group is listed with its incarnation; the chain is kept in order.
    let listed = b.ok("mls.groups", json!({})).await;
    assert_eq!(
        listed["groups"],
        json!([{"conversation":conversation,"group":next,"incarnation":1,"epoch":0,"pending":false}])
    );
    let chain = b.ok("mls.group_history", json!({"conversation":conversation})).await;
    assert_eq!(
        chain["incarnations"].as_array().unwrap().iter().map(|i| i["group"].clone()).collect::<Vec<_>>(),
        [json!(old), json!(next)]
    );
    let after = B64.encode("0");
    let chain = b.ok("mls.group_history", json!({"conversation":conversation,"after":after})).await;
    assert_eq!(chain["incarnations"][0]["group"], next);

    // Nobody writes to the predecessor any more; its messages stay readable.
    let update = alice.update();
    assert_eq!(a.fails("mls.commit", update_request(&old, 0, &update)).await, "stale");
    assert_eq!(a.fails("chat.sealed.send", sealed(&conversation, &old, 0, 2)).await, "stale");
    a.ok("chat.sealed.send", sealed(&conversation, &next, 0, 3)).await;
    let history = b.ok("chat.sealed.history", json!({"conversation":conversation})).await;
    let groups: Vec<_> = history["sealed"].as_array().unwrap().iter().map(|m| m["group"].clone()).collect();
    assert_eq!(groups, [json!(old), json!(next)]);
}

#[path = "common/sealed_attachments.rs"]
mod attachments;

#[tokio::test]
async fn tree_endpoint_checks_session_conversation_and_exports_current_epoch() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    a.redeem(&server.admin_token).await;
    let bob = Device::new(2, 21);
    let b = connect_device(&server, &bob).await;
    let c = connect_device(&server, &Device::new(3, 31)).await;
    let channel = a.create_channel(json!({"name":"encrypted", "encrypted":true})).await;
    let group = create_channel_group(&mut a, &mut alice, channel_id(&channel), true, 1).await.unwrap();
    let http = reqwest::Client::new();
    let url = format!("{}/api/mls/{group}/tree", server.http());
    assert_eq!(http.get(&url).send().await.unwrap().status(), 404);
    for token in ["wrong", ""] {
        assert_eq!(http.get(&url).bearer_auth(token).send().await.unwrap().status(), 404);
    }
    let response = http.get(&url).bearer_auth(&b.welcome.file_access).send().await.unwrap();
    assert_eq!(response.status(), 200, "a reader need not have joined the MLS group");
    assert_eq!(response.headers()["content-type"], "application/octet-stream");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(response.headers()["x-gwar-epoch"], "0");
    assert_eq!(response.headers()["access-control-expose-headers"], "x-gwar-epoch");
    let old = response.bytes().await.unwrap();
    let (epoch, expected) = Observer::create(&alice.group_info(), &a.server_key).unwrap().tree().unwrap();
    assert_eq!(epoch, 0);
    assert_eq!(old.as_ref(), expected);
    let (commit, _) = alice.update();
    let tree_free_info =
        wire(alice.group.as_ref().unwrap().export_group_info(alice.provider.crypto(), &alice.signer, false).unwrap());
    a.ok("mls.commit", update_request(&group, 0, &(commit, tree_free_info))).await;
    let response = http.get(&url).bearer_auth(&a.welcome.file_access).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["x-gwar-epoch"], "1");
    let tree = response.bytes().await.unwrap();
    assert_ne!(tree, old);
    assert_eq!(tree.as_ref(), alice.group.as_ref().unwrap().export_ratchet_tree().tls_serialize_detached().unwrap());
    let head = a.ok("mls.group_info", json!({"group":group})).await;
    assert_eq!(head["epoch"], 1);
    let mut joiner = gwar_mls::agent::Agent::new([21; 32], bob.credential.clone(), now()).unwrap();
    joiner.take_changes();
    let out = joiner
        .join_external(
            &a.server_key,
            &B64.decode(head["group_info"].as_str().unwrap()).unwrap(),
            Some(&tree),
            now(),
            &(),
        )
        .unwrap();
    let mut b = b;
    b.ok("mls.commit", update_request(&group, 1, &(out.commit, out.group_info))).await;

    // The same token is scoped by the conversation's read rule, including DMs.
    let dm = create(&mut a, &mut Device::new(1, 11), 2, 2).await;
    let dm_url = format!("{}/api/mls/{dm}/tree", server.http());
    assert_eq!(http.get(&dm_url).bearer_auth(&c.welcome.file_access).send().await.unwrap().status(), 404);
    assert_eq!(http.get(&dm_url).bearer_auth(&a.welcome.file_access).send().await.unwrap().status(), 200);
    assert_eq!(
        http.get(format!("{}/api/mls/{}/tree", server.http(), b64(&[99; 32])))
            .bearer_auth(&a.welcome.file_access)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );

    // A session asks for at most 30 trees a minute; the cached tree is served until then.
    let d = connect_device(&server, &Device::new(4, 41)).await;
    for _ in 0..30 {
        assert_eq!(http.get(&url).bearer_auth(&d.welcome.file_access).send().await.unwrap().status(), 200);
    }
    assert_eq!(http.get(&url).bearer_auth(&d.welcome.file_access).send().await.unwrap().status(), 429);
    assert_eq!(http.get(&url).bearer_auth(&a.welcome.file_access).send().await.unwrap().status(), 200);
}

fn server_descriptor(device: &Device, server: &str, history: bool, nonce: u8) -> SignedDescriptor {
    let mut d = channel_descriptor(device, 1, history, server, nonce).descriptor;
    d.conversation = "server".into();
    d.kind = Kind::Server;
    SignedDescriptor::sign(d, device.credential.clone(), &device.signer.0)
}

async fn create_server_group(client: &mut Client, device: &mut Device, history: bool, nonce: u8) -> Value {
    let d = server_descriptor(device, &client.server_key, history, nonce);
    device.create(&d, &client.server_key);
    json!({"conversation":"server", "group_info":b64(&device.group_info())})
}

#[tokio::test]
async fn encrypted_server_chat_reaches_all_members_and_routes_history_to_later_devices() {
    let server = TestServer::start_with(|c| c.encrypted_server_chat = true).await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    assert_eq!(serde_json::to_value(a.welcome.server.server_chat).unwrap(), json!({"encrypted":true,"history":true}));
    assert_eq!(a.challenge["server"]["server_chat"], json!({"encrypted":true,"history":true}));
    assert_eq!(a.welcome.conversations.iter().find(|c| c.conversation == "server").unwrap().count, 0);
    assert_eq!(a.fails("chat.send", json!({"target":"server","text":"plaintext"})).await, "bad_request");
    assert!(b.drain_events("chat.message").await.is_empty());

    let invalid = create_server_group(&mut a, &mut alice, false, 1).await;
    assert_eq!(a.fails("mls.group.create", invalid).await, "bad_request");
    let request = create_server_group(&mut a, &mut alice, true, 2).await;
    let group = a.ok("mls.group.create", request).await["group"].as_str().unwrap().to_owned();
    assert_eq!(b.ok("mls.groups", json!({})).await["groups"][0]["conversation"], "server");
    let (notes, mut bridge) = tokio::sync::mpsc::unbounded_channel();
    server.running.core.bridge(vc_server::core::bridge::BridgeMsg::Attach(notes)).await;
    let vc_server::core::bridge::BridgeNote::Snapshot { server: info, .. } = bridge.recv().await.unwrap() else {
        panic!("snapshot")
    };
    assert!(info.server_chat.encrypted);
    let first = a.ok("chat.sealed.send", sealed("server", &group, 0, 1)).await;
    assert!(bridge.try_recv().is_err(), "sealed server messages never reach the bridge");
    assert_eq!(b.next_event("chat.sealed").await, first);
    join_channel(&server, &mut b, &mut bob, &group).await;
    alice.process(&B64.decode(a.next_event("mls.commit").await["commit"].as_str().unwrap()).unwrap());
    assert_eq!(b.ok("chat.sealed.history", json!({"conversation":"server"})).await["sealed"], json!([first]));

    let mut carol = Device::new(3, 31);
    let mut c = connect_device(&server, &carol).await;
    let mark = c.welcome.conversations.iter().find(|c| c.conversation == "server").unwrap();
    assert_eq!((mark.last_read, mark.count), (first["id"].as_u64().unwrap() as u32, 0));
    join_channel(&server, &mut c, &mut carol, &group).await;
    let commit = a.next_event("mls.commit").await;
    alice.process(&B64.decode(commit["commit"].as_str().unwrap()).unwrap());
    bob.process(&B64.decode(commit["commit"].as_str().unwrap()).unwrap());
    assert_eq!(c.ok("chat.sealed.history", json!({"conversation":"server"})).await["sealed"], json!([first]));
    let hpke = hpke(&mut c, &carol, 7).await;
    let recipient = public_key(&carol.signer.0);
    c.ok("mls.history.request", json!({"group":group})).await;
    let wanted = a.next_event("mls.history.wanted").await;
    assert_eq!(wanted["device"], recipient);
    assert!(wanted["assignment"].is_string(), "server groups use channel history assignments");
    assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([wanted]));
    let bytes = chunk(&alice, &a, &group, &carol, &hpke);
    a.ok(
        "mls.history.share",
        json!({"group":group,"recipient":recipient,"chunk":b64(&bytes),"assignment":wanted["assignment"]}),
    )
    .await;
    assert_eq!(c.next_event("mls.history").await["group"], group);
    assert_eq!(c.ok("mls.history.fetch", json!({"group":group})).await["chunks"][0]["chunk"], b64(&bytes));
    assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));
    assert_eq!(b.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));
    let second = b.ok("chat.sealed.send", sealed("server", &group, 2, 2)).await;
    assert_eq!(c.next_event("chat.sealed").await, second);
    c.ok("chat.read", json!({"conversation":"server","message":second["id"]})).await;
    let c2 = connect_device(&server, &Device::new(3, 32)).await;
    let mark = c2.welcome.conversations.iter().find(|c| c.conversation == "server").unwrap();
    assert_eq!((mark.last_read, mark.count), (second["id"].as_u64().unwrap() as u32, 0));

    // Only moderators with message_manage may remove another member's sealed text.
    let remove = json!({"conversation":"server","id":first["id"]});
    assert_eq!(c.fails("chat.sealed.remove", remove.clone()).await, "forbidden");
    a.redeem(&server.admin_token).await;
    a.ok("chat.sealed.remove", remove).await;
    for client in [&mut b, &mut c] {
        assert_eq!(
            client.next_event("chat.sealed.removed").await,
            json!({"conversation":"server","ids":[first["id"]]})
        );
    }
    assert_eq!(c.ok("chat.sealed.history", json!({"conversation":"server"})).await["sealed"], json!([second]));
    while let Ok(note) = bridge.try_recv() {
        if let vc_server::core::bridge::BridgeNote::Event(event) = note {
            assert!(!matches!(*event, vc_proto::Event::ChatSealed(_) | vc_proto::Event::ChatSealedRemoved { .. }));
        }
    }
}

#[tokio::test]
async fn plaintext_server_chat_stays_live_only_and_refuses_sealed_conversations() {
    let server = TestServer::start().await;
    let mut alice = Device::new(1, 11);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &Device::new(2, 21)).await;
    assert_eq!(serde_json::to_value(a.welcome.server.server_chat).unwrap(), json!({"encrypted":false,"history":false}));
    assert!(a.welcome.conversations.iter().all(|c| c.conversation != "server"));
    let request = create_server_group(&mut a, &mut alice, true, 1).await;
    assert_eq!(a.fails("mls.group.create", request).await, "bad_request");
    assert_eq!(a.fails("mls.group_history", json!({"conversation":"server"})).await, "forbidden");
    for op in ["chat.sealed.send", "chat.sealed.edit", "chat.delete_own"] {
        let mut request = sealed("server", &b64(&[9; 32]), 0, 1);
        request["revision"] = json!(1);
        assert_eq!(a.fails(op, request).await, "bad_request");
    }
    for (op, request) in [
        ("chat.sealed.history", json!({"conversation":"server"})),
        ("chat.sealed.remove", json!({"conversation":"server","id":1})),
        ("chat.read", json!({"conversation":"server","message":1})),
    ] {
        assert_eq!(a.fails(op, request).await, "bad_request");
    }
    let sent = a.ok("chat.send", json!({"target":"server","text":"hello everyone"})).await;
    assert_eq!(b.next_event("chat.message").await, sent);
    let mut later = connect_device(&server, &Device::new(3, 31)).await;
    assert!(later.welcome.conversations.iter().all(|c| c.conversation != "server"));
    assert!(later.drain_events("chat.message").await.is_empty());
}

#[tokio::test]
async fn encrypted_server_chat_restarts_under_the_channel_rules() {
    let server = TestServer::start_with(|c| c.encrypted_server_chat = true).await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let request = create_server_group(&mut a, &mut alice, true, 1).await;
    let old = a.ok("mls.group.create", request).await["group"].as_str().unwrap().to_owned();
    let descriptor = server_descriptor(&alice, &a.server_key, true, 1);
    join_channel(&server, &mut b, &mut bob, &old).await;
    a.ok("chat.sealed.send", sealed("server", &old, 1, 1)).await;
    let mut restarted = Device::new(1, 11);
    let d = successor(&restarted, &descriptor, 1, &old, 2);
    restarted.create(&d, &a.server_key);
    let next = a
        .ok("mls.group.create", json!({"conversation":"server","group_info":b64(&restarted.group_info())}))
        .await["group"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(a.fails("chat.sealed.send", sealed("server", &old, 1, 2)).await, "stale");
    a.ok("chat.sealed.send", sealed("server", &next, 0, 3)).await;
    let groups = b.ok("mls.groups", json!({})).await;
    assert_eq!(
        (groups["groups"][0]["group"].clone(), groups["groups"][0]["incarnation"].clone()),
        (json!(next), json!(1))
    );
    let chain = b.ok("mls.group_history", json!({"conversation":"server"})).await;
    assert_eq!(chain["incarnations"].as_array().unwrap().len(), 2);
    let history = b.ok("chat.sealed.history", json!({"conversation":"server"})).await;
    assert_eq!(history["sealed"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn server_chat_history_routing_resumes_after_restart_and_a_rejoin_cancels_it() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("vc.sqlite3");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.data_dir = Some(dir.path().into());
        c.encrypted_server_chat = true;
    })
    .await;
    let mut alice = Device::new(1, 11);
    let mut bob = Device::new(2, 21);
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    let request = create_server_group(&mut a, &mut alice, true, 1).await;
    let group = a.ok("mls.group.create", request).await["group"].as_str().unwrap().to_owned();
    join_channel(&server, &mut b, &mut bob, &group).await;
    let message = b.ok("chat.sealed.send", sealed("server", &group, 1, 1)).await;
    b.ok("mls.history.request", json!({"group":group})).await;
    let previous = a.next_event("mls.history.wanted").await;
    a.close().await;
    b.close().await;
    drop(server);

    let mut config = base_config();
    config.database = Some(database.clone());
    config.data_dir = Some(dir.path().into());
    let server = TestServer { running: vc_server::start(config).await.unwrap(), admin_token: String::new() };
    let mut a = connect_device(&server, &alice).await;
    let mut b = connect_device(&server, &bob).await;
    assert!(a.welcome.server.server_chat.encrypted);
    let wanted = a.next_event("mls.history.wanted").await;
    assert_eq!(wanted["group"], group);
    assert!(wanted["assignment"].is_string());
    assert_ne!(wanted["assignment"], previous["assignment"]);
    assert_eq!(b.ok("chat.sealed.history", json!({"conversation":"server"})).await["sealed"], json!([message]));

    // Replacing the recipient's leaf cancels its old request and assignments durably.
    join_channel(&server, &mut b, &mut bob, &group).await;
    assert_eq!(a.ok("mls.history.wanted", json!({})).await["wanted"], json!([]));
    assert!(
        vc_server::store::Store::open(&database)
            .unwrap()
            .mls_channel_requests(None, Some(&group), now())
            .unwrap()
            .is_empty()
    );
}

fn voice_descriptor(device: &Device, channel: u32, parent: &str, server: &str, nonce: u8) -> SignedDescriptor {
    let mut d = channel_descriptor(device, channel, false, server, nonce).descriptor;
    d.kind = Kind::Voice;
    d.conversation = format!("voice:{channel}");
    d.channel_group_id = Some(parent.into());
    SignedDescriptor::sign(d, device.credential.clone(), &device.signer.0)
}
async fn participation(client: &mut Client, channel: u32) -> Value {
    client.ok("channel.join", json!({"channel":channel})).await;
    client.ok("voice.state", json!({})).await["voice"].clone()
}
fn voice_binding(state: &Value) -> Value {
    json!({"call":state["call"],"participation":state["participation"]})
}
async fn join_voice(
    server: &TestServer,
    client: &mut Client,
    device: &mut Device,
    group: &str,
    state: &Value,
) -> (Vec<u8>, Vec<u8>) {
    let info = client.ok("mls.group_info", json!({"group":group})).await;
    let tree = reqwest::Client::new()
        .get(format!("{}/api/mls/{group}/tree", server.http()))
        .bearer_auth(&client.welcome.file_access)
        .send()
        .await
        .unwrap();
    assert_eq!(tree.status(), 200);
    let joined = device
        .join_external(&B64.decode(info["group_info"].as_str().unwrap()).unwrap(), Some(&tree.bytes().await.unwrap()));
    let mut request = update_request(group, info["epoch"].as_u64().unwrap(), &joined);
    request["voice"] = voice_binding(state);
    client.ok("mls.commit", request).await;
    joined
}

#[tokio::test]
async fn voice_four_devices_creation_join_takeover_removal_and_new_call() {
    let server = TestServer::start_with(|c| c.max_clients = 4).await;
    let mut parent = Device::new(1, 11);
    let mut devices = [Device::new(1, 11), Device::new(1, 12), Device::new(2, 21), Device::new(2, 22)];
    let mut clients = Vec::new();
    for device in &devices {
        clients.push(connect_device(&server, device).await);
    }
    clients[0].ok("token.redeem", json!({"token":server.admin_token})).await;
    let channel = channel_id(&clients[0].create_channel(json!({"name":"voice","encrypted":true})).await);
    assert_eq!(clients[0].fails("channel.join", json!({"channel":channel})).await, "forbidden");
    assert!(clients[0].ok("voice.state", json!({})).await["voice"].is_null());
    let parent_id = create_channel_group(&mut clients[0], &mut parent, channel, true, 60).await.unwrap();
    for (i, client) in clients.iter_mut().enumerate().skip(1) {
        let mut parent_device = Device::new(if i == 1 { 1 } else { 2 }, [11, 12, 21, 22][i]);
        join_channel(&server, client, &mut parent_device, &parent_id).await;
    }
    let mut states = Vec::new();
    for client in &mut clients {
        states.push(participation(client, channel).await);
    }
    assert!(states.iter().all(|s| s["call"] == states[0]["call"]));
    assert_eq!(participation(&mut clients[0], channel).await, states[0], "duplicate entry is stable");
    let d = voice_descriptor(&devices[0], channel, &parent_id, &clients[0].server_key, 61);
    devices[0].create(&d, &clients[0].server_key);
    let create = json!({"conversation":d.descriptor.conversation,"group_info":b64(&devices[0].group_info()),"voice":voice_binding(&states[0])});
    assert_eq!(clients[1].fails("mls.group.create", create.clone()).await, "stale");
    let group = clients[0].ok("mls.group.create", create.clone()).await["group"].as_str().unwrap().to_owned();
    assert_eq!(
        clients[0].ok("mls.group.create", create).await["group"],
        group,
        "lost creation reply retries identically"
    );
    let alternate = voice_descriptor(&devices[1], channel, &parent_id, &clients[1].server_key, 62);
    devices[1].create(&alternate, &clients[1].server_key);
    assert_eq!(clients[1].fails("mls.group.create",json!({"conversation":alternate.descriptor.conversation,"group_info":b64(&devices[1].group_info()),"voice":voice_binding(&states[1])})).await,"conflict");
    for i in 1..4 {
        devices[i] = Device::new(if i == 1 { 1 } else { 2 }, [11, 12, 21, 22][i]);
        let joined = join_voice(&server, &mut clients[i], &mut devices[i], &group, &states[i]).await;
        devices[0].process(&joined.0);
    }
    let listed = clients[0].ok("mls.groups", json!({})).await;
    assert!(listed["groups"].as_array().unwrap().iter().all(|g| g["group"] != group));
    assert_eq!(clients[0].fails("mls.history.request", json!({"group":group})).await, "bad_request");
    assert_eq!(
        clients[0].fails("mls.group_history", json!({"conversation":format!("voice:{channel}")})).await,
        "bad_request"
    );
    // Full-server replacement succeeds only after authentication and detaches the old leaf.
    let mut replacement = connect_device(&server, &devices[1]).await;
    let replaced = clients[1].next_event("disconnected").await;
    assert_eq!(replaced["reason"]["kind"], "replaced");
    let state = participation(&mut replacement, channel).await;
    assert_eq!(state["call"], states[1]["call"]);
    assert_ne!(state["participation"], states[1]["participation"]);
    let pending = clients[0].ok("mls.log", json!({"group":group,"since":3})).await;
    assert_eq!(pending["pending"].as_array().unwrap().len(), 1);
    let mut fresh = Device::new(1, 12);
    let joined = join_voice(&server, &mut replacement, &mut fresh, &group, &state).await;
    devices[0].process(&joined.0);
    // A member update from the wrong participation cannot submit a valid signature.
    let update = devices[0].update();
    let mut request = update_request(&group, 4, &update);
    request["voice"] = voice_binding(&states[2]);
    assert_eq!(clients[2].fails("mls.commit", request.clone()).await, "forbidden");
    request["voice"] = voice_binding(&states[0]);
    clients[0].ok("mls.commit", request).await;
    // Leaving creates a proposal; a log replay carries the accepted commit bytes.
    clients[2].ok("channel.leave", json!({})).await;
    let log = clients[0].ok("mls.log", json!({"group":group,"since":4})).await;
    assert_eq!(log["entries"][0]["commit"], b64(&update.0));
    assert_eq!(log["pending"].as_array().unwrap().len(), 1);
    let outsider = reqwest::Client::new()
        .get(format!("{}/api/mls/{group}/tree", server.http()))
        .bearer_auth(&clients[2].welcome.file_access)
        .send()
        .await
        .unwrap();
    assert_eq!(outsider.status(), 404);
    // A competing update leaves the detached device in the tree: removal is reissued at the new epoch.
    let previous_proposal = log["pending"][0].clone();
    let competing = devices[0].update();
    let mut request = update_request(&group, 5, &competing);
    request["voice"] = voice_binding(&states[0]);
    clients[0].ok("mls.commit", request).await;
    let pending = clients[0].ok("mls.log", json!({"group":group,"since":6})).await;
    assert_ne!(pending["pending"][0], previous_proposal);
    devices[0].process(&B64.decode(pending["pending"][0].as_str().unwrap()).unwrap());
    let removed = devices[0].commit_pending();
    let mut request = update_request(&group, 6, &removed);
    request["voice"] = voice_binding(&states[0]);
    clients[0].ok("mls.commit", request).await;
    assert!(clients[0].ok("mls.log", json!({"group":group,"since":7})).await["pending"].as_array().unwrap().is_empty());
    // A device that discarded/pruned its old state replaces its own leaf in this same group.
    let mut pruned = Device::new(2, 22);
    let rejoined = join_voice(&server, &mut clients[3], &mut pruned, &group, &states[3]).await;
    devices[0].process(&rejoined.0);
    assert_eq!(clients[3].ok("voice.state", json!({})).await["voice"]["group"], group);
    // Catch up the parent, then remove this device there. Its voice mapping is cut off immediately.
    let parent_log = clients[0].ok("mls.log", json!({"group":parent_id,"since":0})).await;
    for entry in parent_log["entries"].as_array().unwrap() {
        parent.process(&B64.decode(entry["commit"].as_str().unwrap()).unwrap());
    }
    let leaf = parent
        .group
        .as_ref()
        .unwrap()
        .members()
        .find(|m| m.signature_key == key(22).verifying_key().as_bytes())
        .unwrap()
        .index;
    let pg = parent.group.as_mut().unwrap();
    pg.set_aad(commit_aad(now()));
    let (commit, _, _) = pg.remove_members(&parent.provider, &parent.signer, &[leaf]).unwrap();
    pg.merge_pending_commit(&parent.provider).unwrap();
    let removed = (wire(commit), parent.group_info());
    clients[0].ok("mls.commit", update_request(&parent_id, 3, &removed)).await;
    assert!(clients[3].ok("voice.state", json!({})).await["voice"].is_null());
    assert_eq!(clients[3].fails("channel.join", json!({"channel":channel})).await, "forbidden");
    assert_eq!(
        clients[0].ok("mls.log", json!({"group":group,"since":8})).await["pending"].as_array().unwrap().len(),
        1
    );
    for i in [0, 3] {
        clients[i].ok("channel.leave", json!({})).await;
    }
    replacement.ok("channel.leave", json!({})).await;
    let next = participation(&mut clients[0], channel).await;
    assert_ne!(next["call"], states[0]["call"]);
    assert!(next["group"].is_null());
    assert_eq!(clients[0].fails("mls.group_info", json!({"group":group})).await, "not_found");
    // Every participant receives its own bound closure, even after parent deletion.
    let second = participation(&mut replacement, channel).await;
    let third = participation(&mut clients[2], channel).await;
    clients[0].ok("channel.delete", json!({"channel":channel})).await;
    let closed = clients[0].next_event("voice.closed").await;
    assert_eq!(closed["reason"], "channel_deleted");
    assert_eq!(closed["participation"], next["participation"]);
    for (client, state) in [(&mut replacement, second), (&mut clients[2], third)] {
        let closed = client.next_event("voice.closed").await;
        assert_eq!(closed["reason"], "channel_deleted");
        assert_eq!(closed["call"], state["call"]);
        assert_eq!(closed["participation"], state["participation"]);
        assert!(client.ok("voice.state", json!({})).await["voice"].is_null());
    }
    assert!(clients[0].ok("voice.state", json!({})).await["voice"].is_null());
}

#[tokio::test]
async fn voice_parent_restart_and_process_restart_require_fresh_participation() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("vc.sqlite3");
    let server = TestServer::start_with(|c| {
        c.database = Some(database.clone());
        c.data_dir = Some(dir.path().into());
    })
    .await;
    let mut parent = Device::new(1, 11);
    let mut a = connect_device(&server, &parent).await;
    a.ok("token.redeem", json!({"token":server.admin_token})).await;
    let channel = channel_id(&a.create_channel(json!({"name":"voice-restart","encrypted":true})).await);
    let parent_id = create_channel_group(&mut a, &mut parent, channel, true, 70).await.unwrap();
    let state = participation(&mut a, channel).await;
    let mut voice = Device::new(1, 11);
    let d = voice_descriptor(&voice, channel, &parent_id, &a.server_key, 71);
    voice.create(&d, &a.server_key);
    let group=a.ok("mls.group.create",json!({"conversation":d.descriptor.conversation,"group_info":b64(&voice.group_info()),"voice":voice_binding(&state)})).await["group"].clone();
    // A valid successor to the parent ends the call, rather than modifying its descriptor.
    let mut successor = channel_descriptor(&parent, channel, true, &a.server_key, 72).descriptor;
    successor.incarnation = 1;
    successor.previous_group_id = Some(parent_id);
    let successor = SignedDescriptor::sign(successor, parent.credential.clone(), &parent.signer.0);
    let mut restarted = Device::new(1, 11);
    restarted.create(&successor, &a.server_key);
    a.ok(
        "mls.group.create",
        json!({"conversation":successor.descriptor.conversation,"group_info":b64(&restarted.group_info())}),
    )
    .await;
    let closed = a.next_event("voice.closed").await;
    assert_eq!(closed["reason"], "channel_restarted");
    assert_eq!(closed["call"], state["call"]);
    assert!(a.ok("voice.state", json!({})).await["voice"].is_null());
    let fresh = participation(&mut a, channel).await;
    assert_ne!(fresh["call"], state["call"]);
    assert_eq!(fresh["channel_group"], b64(&successor.group_id()));
    // Abrupt core shutdown while still in voice never restores its call state.
    drop(a);
    drop(server);
    let mut config = base_config();
    config.database = Some(database);
    config.data_dir = Some(dir.path().into());
    let server = TestServer { running: vc_server::start(config).await.unwrap(), admin_token: String::new() };
    let mut a = connect_device(&server, &restarted).await;
    assert!(a.ok("voice.state", json!({})).await["voice"].is_null());
    let after = participation(&mut a, channel).await;
    assert_ne!(after["call"], fresh["call"]);
    assert!(after["group"].is_null());
    assert_eq!(a.fails("mls.group_info", json!({"group":group})).await, "not_found");
}

#[tokio::test]
async fn rejected_same_device_authentication_does_not_evict_the_current_participation() {
    let server = TestServer::start_with(|c| c.max_clients = 1).await;
    let device = Device::new(1, 11);
    let mut a = connect_device(&server, &device).await;
    let state = participation(&mut a, 1).await;
    let expired = certificate(&key(1), &key(11), now() - 86_400_000, now() - 1);
    let attempt = Client::connect_with(&server, key(11), |raw, key| {
        let mut hello = raw.hello(key, "expired", None);
        set_certificate(&mut hello, expired, key);
        hello
    })
    .await;
    assert!(attempt.is_err());
    assert_eq!(a.ok("voice.state", json!({})).await["voice"], state);
}

#[tokio::test]
async fn voice_parent_leaf_expiry_ends_an_idle_participation_without_expiring_the_connection() {
    let server = TestServer::start().await;
    let mut parent = Device::new(1, 11);
    let mut a = connect_device(&server, &parent).await;
    a.ok("token.redeem", json!({"token":server.admin_token})).await;
    let channel = channel_id(&a.create_channel(json!({"name":"voice-expiry","encrypted":true})).await);
    parent.credential = Credential::Account(
        serde_json::from_value(certificate(&key(1), &key(11), now() - 1000, now() + 2000)).unwrap(),
    );
    create_channel_group(&mut a, &mut parent, channel, true, 81).await.unwrap();
    participation(&mut a, channel).await;
    // No new request wakes the core: its certificate deadline must emit the exit itself.
    loop {
        if a.next_event("voice.state").await["voice"].is_null() {
            break;
        }
    }
    assert!(a.ok("voice.state", json!({})).await["voice"].is_null());
    assert_eq!(a.fails("channel.join", json!({"channel":channel})).await, "forbidden");
    // This connection's newer certificate is still valid, so plaintext voice remains usable.
    assert!(!participation(&mut a, 1).await["encrypted"].as_bool().unwrap());
}

#[tokio::test]
async fn voice_leaf_expiry_detaches_only_that_mapping_and_proposes_removal() {
    let server = TestServer::start().await;
    let mut parent = Device::new(1, 11);
    let mut other_parent = Device::new(2, 21);
    let mut a = connect_device(&server, &parent).await;
    let mut b = connect_device(&server, &other_parent).await;
    a.ok("token.redeem", json!({"token":server.admin_token})).await;
    let channel = channel_id(&a.create_channel(json!({"name":"voice-leaf-expiry","encrypted":true})).await);
    let parent_id = create_channel_group(&mut a, &mut parent, channel, true, 82).await.unwrap();
    join_channel(&server, &mut b, &mut other_parent, &parent_id).await;
    let a_state = participation(&mut a, channel).await;
    let b_state = participation(&mut b, channel).await;
    let mut voice = Device::new(1, 11);
    voice.credential = Credential::Account(
        serde_json::from_value(certificate(&key(1), &key(11), now() - 1000, now() + 2000)).unwrap(),
    );
    let d = voice_descriptor(&voice, channel, &parent_id, &a.server_key, 83);
    voice.create(&d, &a.server_key);
    let group = a.ok("mls.group.create", json!({"conversation":d.descriptor.conversation,"group_info":b64(&voice.group_info()),"voice":voice_binding(&a_state)})).await["group"].as_str().unwrap().to_owned();
    let mut listener = Device::new(2, 21);
    join_voice(&server, &mut b, &mut listener, &group, &b_state).await;
    loop {
        if a.next_event("voice.state").await["voice"].is_null() {
            break;
        }
    }
    let state = b.ok("voice.state", json!({})).await["voice"].clone();
    assert_eq!(state["call"], b_state["call"]);
    assert_eq!(state["group"], group);
    assert_eq!(b.ok("mls.log", json!({"group":group,"since":1})).await["pending"].as_array().unwrap().len(), 1);
    // The parent and connection certificates remain current: fresh participation is allowed.
    let fresh = participation(&mut a, channel).await;
    assert_ne!(fresh["participation"], a_state["participation"]);
}

#[tokio::test]
async fn voice_opaque_srtp_and_unknown_ssrc_churn_preserve_the_admitted_stream() {
    let server = TestServer::start().await;
    let mut first = Device::new(1, 11);
    let mut second = Device::new(2, 21);
    let mut a = connect_device(&server, &first).await;
    let mut b = connect_device(&server, &second).await;
    a.ok("token.redeem", json!({"token":server.admin_token})).await;
    let channel = channel_id(&a.create_channel(json!({"name":"opaque","encrypted":true})).await);
    let parent = create_channel_group(&mut a, &mut first, channel, true, 91).await.unwrap();
    join_channel(&server, &mut b, &mut second, &parent).await;
    let a_state = participation(&mut a, channel).await;
    let b_state = participation(&mut b, channel).await;
    first = Device::new(1, 11);
    second = Device::new(2, 21);
    let d = voice_descriptor(&first, channel, &parent, &a.server_key, 92);
    first.create(&d, &a.server_key);
    let group=a.ok("mls.group.create",json!({"conversation":d.descriptor.conversation,"group_info":b64(&first.group_info()),"voice":voice_binding(&a_state)})).await["group"].as_str().unwrap().to_owned();
    let joined = join_voice(&server, &mut b, &mut second, &group, &b_state).await;
    first.process(&joined.0);
    let source = common::rtc::Peer::start(&mut a, 1).await;
    let mut receiver = common::rtc::Peer::start(&mut b, 1).await;
    let ciphertext = vec![0xff; 1033];
    source.send(65535, u32::MAX - 959, ciphertext.clone()).await;
    let initial = receiver.receive().await;
    assert_eq!(initial.payload, ciphertext);
    assert_eq!(initial.ticks, 48_000);
    // Each unknown stream has valid SRTP and the negotiated mic MID. A first
    // packet must never choose the admitted SSRC or allocate its ordering state.
    for ssrc in 100..356u32 {
        if ssrc == source.ssrc {
            continue;
        }
        source
            .tx
            .send(common::rtc::Send { ssrc, sequence: 1, ticks: 0, payload: vec![0xf8, 77], level: -20 })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
    receiver.silent().await;
    let (reply, rx) = tokio::sync::oneshot::channel();
    server
        .running
        .plane
        .webrtc
        .send(vc_server::media::webrtc::MediaCmd::Inspect {
            session: a.welcome.session,
            probe_ssrcs: (100..356).collect(),
            reply,
        })
        .await
        .unwrap();
    assert_eq!(
        rx.await.unwrap().unwrap().registered_probe_ssrcs,
        0,
        "unknown SSRC replaced negotiated transport state"
    );
    source.send(65536, 0, ciphertext.clone()).await;
    let next = receiver.receive().await;
    assert_eq!(next.payload, ciphertext);
    assert_eq!(next.ticks, 48_960);
    assert_eq!(next.generation, initial.generation);
    // Inspect actual str0m receive state, not just what survived the post-SRTP gate.
    let snapshot = async {
        loop {
            let (reply, rx) = tokio::sync::oneshot::channel();
            server
                .running
                .plane
                .webrtc
                .send(vc_server::media::webrtc::MediaCmd::Inspect {
                    session: a.welcome.session,
                    probe_ssrcs: (100..356).collect(),
                    reply,
                })
                .await
                .unwrap();
            let state = rx.await.unwrap().unwrap();
            if state.counters.rtp_unknown_ssrc > 0 && state.rtcp_reports > 0 {
                break state;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), snapshot)
        .await
        .expect("unknown churn and SRTCP sender report");
    assert_eq!(snapshot.registered_probe_ssrcs, 0, "unknown SRTP allocated str0m transport state");

    source.send(65537, 960, vec![0xff; 1034]).await;
    receiver.silent().await;
    source.send(65538, 960, vec![]).await;
    receiver.silent().await;
    source.send(65539, 960, vec![0xff, 0]).await;
    assert_eq!(receiver.receive().await.payload, [0xff, 0]);
    // Queued old-call audio cannot route after a move, including a move back.
    a.ok("channel.join", json!({"channel":1})).await;
    source.send(65540, 1920, vec![0xff, 0]).await;
    receiver.silent().await;
}
