//! Encrypted private channels on the server (docs/e2e-build.md, section 6, 6c): admission from the
//! group's signed lists, the creator's authority and the live member list checkpoints.

mod common;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use common::*;
use ed25519_dalek::SigningKey;
use gwar_mls::{
    Credential, DESCRIPTOR_EXTENSION, Descriptor, Kind, SUITE, SignedDescriptor,
    access::{ACCESS_EXTENSION, Access, ChannelMembers, SignedMembers},
    ds::commit_aad,
    owner::{Anchor, Genesis, ManagerList, OwnerChain, SignedAnchor, SignedLink, SignedList},
    signer::{Ed25519Signer, server_credential},
};
use openmls::prelude::{
    tls_codec::{Deserialize as _, Serialize as _},
    *,
};
use openmls_rust_crypto::OpenMlsRustCrypto;
use serde_json::{Value, json};
use vc_server::core::now_ms;

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}
fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}
fn uid(seed: u8) -> String {
    uid_of(&key(seed))
}
fn capabilities() -> Capabilities {
    Capabilities::new(
        None,
        Some(&[SUITE]),
        Some(&[ExtensionType::Unknown(DESCRIPTOR_EXTENSION), ExtensionType::Unknown(ACCESS_EXTENSION)]),
        None,
        Some(&[CredentialType::Basic]),
    )
}
fn wire(message: impl Into<MlsMessageOut>) -> Vec<u8> {
    message.into().tls_serialize_detached().unwrap()
}

struct Device {
    provider: OpenMlsRustCrypto,
    signer: Ed25519Signer,
    credential: Credential,
    device: SigningKey,
    group: Option<MlsGroup>,
}

impl Device {
    fn new(account: u8, device: u8) -> Self {
        let credential = Credential::Account(
            serde_json::from_value(certificate(
                &key(account),
                &key(device),
                now_ms() - 1000,
                now_ms() + 29 * 86_400_000,
            ))
            .unwrap(),
        );
        Self {
            provider: OpenMlsRustCrypto::default(),
            signer: Ed25519Signer(key(device)),
            credential,
            device: key(device),
            group: None,
        }
    }
    fn with_key(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential: BasicCredential::new(self.credential.encode().into_bytes()).into(),
            signature_key: self.signer.0.verifying_key().to_bytes().to_vec().into(),
        }
    }
    fn group_info(&self, tree: bool) -> Vec<u8> {
        wire(self.group.as_ref().unwrap().export_group_info(self.provider.crypto(), &self.signer, tree).unwrap())
    }
    fn members(&self, group: &[u8; 32], seq: u64, accounts: &[u8]) -> SignedMembers {
        let mut members: Vec<String> = accounts.iter().map(|&a| uid(a)).collect();
        members.sort();
        let m = ChannelMembers {
            server_key: String::new(),
            generation: 1,
            group_id: b64(group),
            seq,
            members,
            signer_uid: self.credential.uid(),
            signed_at: now_ms(),
        };
        SignedMembers::sign(m, self.credential.clone(), &self.device)
    }
}

async fn connect(server: &TestServer, device: &Device) -> Client {
    let Credential::Account(cert) = &device.credential else { unreachable!() };
    Client::connect_with(server, device.signer.0.clone(), |raw, key| {
        let mut h = raw.hello(key, "device", None);
        set_certificate(&mut h, serde_json::to_value(cert).unwrap(), key);
        h
    })
    .await
    .unwrap()
}

async fn chain(client: &mut Client) -> OwnerChain {
    let page = client.ok("owner.chain", json!({"generation": 1, "after": 0})).await;
    let links = page["links"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| SignedLink::parse(l["statement"].as_str().unwrap(), l["signature"].as_str().unwrap()).unwrap())
        .collect();
    OwnerChain::verify(Genesis::parse(page["genesis"].as_str().unwrap()).unwrap(), links).unwrap()
}

/// The owner (account `account`, device `owner`) signs an anchor with `confirmers` and a list.
fn pair(chain: &OwnerChain, account: u8, owner: &Device, confirmers: &[u8]) -> (SignedAnchor, SignedList) {
    let server_key = chain.genesis().server_key.clone();
    let mut confirmers: Vec<String> = confirmers.iter().map(|&a| uid(a)).collect();
    confirmers.sort();
    let anchor = SignedAnchor::sign(
        Anchor {
            server_key: server_key.clone(),
            generation: 1,
            owner_head: chain.head(),
            anchor_seq: 1,
            confirmers,
            signed_at: now_ms(),
        },
        &key(account),
    );
    let list = ManagerList {
        server_key,
        generation: 1,
        anchor_hash: anchor.hash(),
        seq: 1,
        managers: vec![],
        signer_uid: owner.credential.uid(),
        signed_at: now_ms(),
    };
    (anchor.clone(), SignedList::sign(list, owner.credential.clone(), &owner.device))
}

fn with_server(mut members: SignedMembers, owner: &Device, server_key: &str) -> SignedMembers {
    let mut m = members.members().clone();
    m.server_key = server_key.to_owned();
    members = SignedMembers::sign(m, owner.credential.clone(), &owner.device);
    members
}

fn extensions(d: &SignedDescriptor, access: &Access, server_key: &str) -> Extensions<GroupContext> {
    Extensions::from_vec(vec![
        Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
            &[ExtensionType::Unknown(DESCRIPTOR_EXTENSION), ExtensionType::Unknown(ACCESS_EXTENSION)],
            &[],
            &[CredentialType::Basic],
        )),
        Extension::Unknown(DESCRIPTOR_EXTENSION, UnknownExtension(d.encode())),
        Extension::ExternalSenders(vec![ExternalSender::new(
            B64.decode(server_key).unwrap().into(),
            BasicCredential::new(server_credential(server_key)).into(),
        )]),
        Extension::Unknown(ACCESS_EXTENSION, UnknownExtension(access.encode())),
    ])
    .unwrap()
}

fn commit_request(group: &str, epoch: u64, commit: &[u8], info: &[u8]) -> Value {
    json!({"group": group, "epoch": epoch, "commit": b64(commit), "group_info": b64(info)})
}

/// A `GroupContextExtensions` commit with new lists, removing `remove`.
fn change(device: &mut Device, extensions: Extensions<GroupContext>, remove: &[u32]) -> (Vec<u8>, Vec<u8>) {
    let (provider, signer) = (&device.provider, &device.signer);
    let group = device.group.as_mut().unwrap();
    group.set_aad(commit_aad(now_ms()));
    let bundle = group
        .commit_builder()
        .propose_group_context_extensions(extensions)
        .unwrap()
        .propose_removals(remove.iter().map(|&i| LeafNodeIndex::new(i)))
        .load_psks(provider.storage())
        .unwrap()
        .create_group_info(true)
        .build(provider.rand(), provider.crypto(), signer, |_| true)
        .unwrap()
        .stage_commit(provider)
        .unwrap();
    let (commit, _, info) = bundle.into_contents();
    // The new epoch's GroupInfo, as the commit builder made it (no tree: a channel group).
    (wire(commit), wire(MlsMessageOut::from(info.expect("a group info"))))
}

#[tokio::test]
async fn an_encrypted_private_channel_admits_only_its_signed_members_and_never_goes_back() {
    let server = TestServer::start().await;
    let mut owner_device = Device::new(1, 11);
    let mut owner = connect(&server, &owner_device).await;
    owner.redeem(&server.admin_token).await;
    let server_key = owner.server_key.clone();
    let c = chain(&mut owner).await;
    let (anchor, list) = pair(&c, 1, &owner_device, &[]);
    owner.ok("managers.update", json!({"anchor": b64(&anchor.encode()), "list": b64(&list.encode())})).await;
    let mut bob_device = Device::new(2, 22);
    let mut bob = connect(&server, &bob_device).await;
    let mut eve = connect(&server, &Device::new(3, 33)).await;
    // Only someone who may sign its member list creates one.
    assert_eq!(
        bob.fails("channel.create", json!({"name": "p", "private": true, "encrypted": true})).await,
        "forbidden"
    );
    let channel = channel_id(&owner.create_channel(json!({"name": "p", "private": true, "encrypted": true})).await);
    assert_eq!(
        owner.fails("channel.update", json!({"channel": channel, "access": {"roles": [], "users": [uid(2)]}})).await,
        "bad_request",
        "its members are the signed list"
    );
    assert!(bob.drain_events("channel.created").await.iter().all(|e| e["id"] != channel));

    // The owner's device creates the group admitting Bob.
    let d = SignedDescriptor::sign(
        Descriptor {
            server_key: server_key.clone(),
            conversation: format!("channel:{channel}"),
            kind: Kind::Channel,
            incarnation: 0,
            previous_group_id: None,
            private: true,
            history: true,
            founding_owner: Some(uid(1)),
            channel_group_id: None,
            creator_uid: uid(1),
            created_at: now_ms() - 500,
            creation_nonce: b64(&[4; 16]),
        },
        owner_device.credential.clone(),
        &owner_device.device,
    );
    let gid = d.group_id();
    let l1 = with_server(owner_device.members(&gid, 1, &[2]), &owner_device, &server_key);
    let access = Access::new(anchor.clone(), list.clone(), l1.clone());
    let config = MlsGroupCreateConfig::builder()
        .ciphersuite(SUITE)
        .capabilities(capabilities())
        .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
        .with_group_context_extensions(extensions(&d, &access, &server_key))
        .build();
    owner_device.group = Some(
        MlsGroup::new_with_group_id(
            &owner_device.provider,
            &owner_device.signer,
            &config,
            GroupId::from_slice(&gid),
            owner_device.with_key(),
        )
        .unwrap(),
    );
    let group = owner
        .ok(
            "mls.group.create",
            json!({"conversation": format!("channel:{channel}"), "group_info": b64(&owner_device.group_info(true))}),
        )
        .await["group"]
        .as_str()
        .unwrap()
        .to_owned();
    // Bob is admitted (he sees the channel); Eve is not.
    assert_eq!(bob.next_event_where("channel.created", |e| e["id"] == channel).await["private"], true);
    assert!(matches!(eve.fails("mls.group_info", json!({"group": group})).await.as_str(), "forbidden" | "not_found"));
    assert!(eve.drain_events("channel.created").await.iter().all(|e| e["id"] != channel));
    // Nor does Eve find its group or its read marks: in `mls.groups` or a new Welcome.
    let conversation = format!("channel:{channel}");
    let listed =
        |groups: &Value| groups["groups"].as_array().unwrap().iter().any(|g| g["conversation"] == conversation);
    assert!(listed(&owner.ok("mls.groups", json!({})).await) && !listed(&eve.ok("mls.groups", json!({})).await));
    let eve_again = connect(&server, &Device::new(3, 34)).await;
    assert!(eve_again.welcome.conversations.iter().all(|c| c.conversation != conversation));
    let owner_again = connect(&server, &Device::new(1, 12)).await;
    assert!(owner_again.welcome.conversations.iter().any(|c| c.conversation == conversation));

    // Bob joins by external commit.
    let info = bob.ok("mls.group_info", json!({"group": group})).await;
    let epoch = info["epoch"].as_u64().unwrap();
    let tree = reqwest::Client::new()
        .get(format!("{}/api/mls/{group}/tree", server.http()))
        .bearer_auth(&bob.welcome.file_access)
        .send()
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let MlsMessageBodyIn::GroupInfo(gi) =
        MlsMessageIn::tls_deserialize_exact(B64.decode(info["group_info"].as_str().unwrap()).unwrap())
            .unwrap()
            .extract()
    else {
        panic!()
    };
    let (mut joined, bundle) = MlsGroup::external_commit_builder()
        .with_config(MlsGroupJoinConfig::builder().wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY).build())
        .with_ratchet_tree(RatchetTreeIn::tls_deserialize_exact(&tree).unwrap())
        .with_aad(commit_aad(now_ms()))
        .build_group(&bob_device.provider, gi, bob_device.with_key())
        .unwrap()
        .leaf_node_parameters(LeafNodeParameters::builder().with_capabilities(capabilities()).build())
        .load_psks(bob_device.provider.storage())
        .unwrap()
        .build(bob_device.provider.rand(), bob_device.provider.crypto(), &bob_device.signer, |_| true)
        .unwrap()
        .finalize(&bob_device.provider)
        .unwrap();
    if joined.pending_commit().is_some() {
        joined.merge_pending_commit(&bob_device.provider).unwrap();
    }
    bob_device.group = Some(joined);
    let join = wire(bundle.into_commit());
    bob.ok("mls.commit", commit_request(&group, epoch, &join, &bob_device.group_info(false))).await;
    {
        let message = MlsMessageIn::tls_deserialize_exact(&join).unwrap().try_into_protocol_message().unwrap();
        let g = owner_device.group.as_mut().unwrap();
        let ProcessedMessageContent::StagedCommitMessage(staged) =
            g.process_message(&owner_device.provider, message).unwrap().into_content()
        else {
            panic!()
        };
        g.merge_staged_commit(&owner_device.provider, *staged).unwrap();
    }

    // The owner drops Bob in the same commit as his leaf: Bob loses the channel.
    let bob_leaf = owner_device
        .group
        .as_ref()
        .unwrap()
        .members()
        .find(|m| {
            Credential::parse(BasicCredential::try_from(m.credential.clone()).unwrap().identity()).unwrap().uid()
                == uid(2)
        })
        .unwrap()
        .index
        .u32();
    let l2 = with_server(owner_device.members(&gid, 2, &[]), &owner_device, &server_key);
    let (commit, info) = change(
        &mut owner_device,
        extensions(&d, &Access::new(anchor.clone(), list.clone(), l2), &server_key),
        &[bob_leaf],
    );
    owner.ok("mls.commit", commit_request(&group, 1, &commit, &info)).await;
    owner_device.group.as_mut().unwrap().merge_pending_commit(&owner_device.provider).unwrap();
    assert_eq!(bob.next_event_where("channel.hidden", |e| e["channel"] == channel).await["channel"], channel);

    // Going back to the first list is refused, and so is a list under another (non-current) pair.
    let (commit, info) =
        change(&mut owner_device, extensions(&d, &Access::new(anchor.clone(), list.clone(), l1), &server_key), &[]);
    assert_eq!(owner.fails("mls.commit", commit_request(&group, 2, &commit, &info)).await, "conflict");
    owner_device.group.as_mut().unwrap().clear_pending_commit(owner_device.provider.storage()).unwrap();
    let (other_anchor, other_list) = pair(&c, 1, &owner_device, &[3]);
    let l3 = with_server(owner_device.members(&gid, 3, &[]), &owner_device, &server_key);
    let (commit, info) =
        change(&mut owner_device, extensions(&d, &Access::new(other_anchor, other_list, l3), &server_key), &[]);
    assert_eq!(owner.fails("mls.commit", commit_request(&group, 2, &commit, &info)).await, "conflict");
}
