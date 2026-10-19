//! The observer on a private channel's group (docs/e2e-build.md, section 6, 6c): the `0xff01`
//! access lists, the membership rule and `GroupContextExtensions` commits.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signer as _, SigningKey};
use gwar_mls::{
    Credential, DESCRIPTOR_EXTENSION, Descriptor, Kind, SUITE, SignedDescriptor,
    access::{ACCESS_EXTENSION, Access, ChannelMembers, SignedMembers},
    ds::{DsError, Observer, commit_aad},
    owner::{Anchor, Genesis, ManagerList, OwnerChain, SignedAnchor, SignedList},
    signer::{Ed25519Signer, server_credential},
};
use gwar_proto::DeviceCertificate;
use openmls::prelude::{
    tls_codec::{Deserialize as _, Serialize as _},
    *,
};
use openmls_rust_crypto::OpenMlsRustCrypto;

const NOW: i64 = 1_800_000_000_000;
const DAY: i64 = 86_400_000;

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}
fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}
fn uid(account: u8) -> String {
    gwar_proto::uid_for_key(key(account).verifying_key().as_bytes())
}
fn server_key() -> String {
    b64(key(200).verifying_key().as_bytes())
}
fn certified(account: u8, device: u8) -> Credential {
    let mut c = DeviceCertificate {
        account_key: b64(key(account).verifying_key().as_bytes()),
        device_key: b64(key(device).verifying_key().as_bytes()),
        issued_at: NOW - DAY,
        expires_at: NOW + 29 * DAY,
        signature: String::new(),
    };
    c.signature = b64(&key(account).sign(gwar_proto::device_statement(&c).as_bytes()).to_bytes());
    Credential::Account(c)
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

/// Account 1 owns the server.
fn chain() -> OwnerChain {
    OwnerChain::new(Genesis { server_key: server_key(), generation: 1, owner_uid: uid(1) })
}

struct Device {
    provider: OpenMlsRustCrypto,
    signer: Ed25519Signer,
    credential: Credential,
    group: Option<MlsGroup>,
}

impl Device {
    fn new(account: u8, device: u8) -> Self {
        Self {
            provider: OpenMlsRustCrypto::default(),
            signer: Ed25519Signer(key(device)),
            credential: certified(account, device),
            group: None,
        }
    }
    fn with_key(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential: BasicCredential::new(self.credential.encode().into_bytes()).into(),
            signature_key: self.signer.0.verifying_key().to_bytes().to_vec().into(),
        }
    }
    fn group_info(&self) -> Vec<u8> {
        let group = self.group.as_ref().unwrap();
        wire(group.export_group_info(self.provider.crypto(), &self.signer, false).unwrap())
    }
    fn group_info_with_tree(&self) -> Vec<u8> {
        let group = self.group.as_ref().unwrap();
        wire(group.export_group_info(self.provider.crypto(), &self.signer, true).unwrap())
    }
    /// Signs a member list with this device.
    fn members(&self, group: &[u8; 32], seq: u64, uids: &[u8]) -> SignedMembers {
        let mut members: Vec<String> = uids.iter().map(|&a| uid(a)).collect();
        members.sort();
        let m = ChannelMembers {
            server_key: server_key(),
            generation: 1,
            group_id: b64(group),
            seq,
            members,
            signer_uid: self.credential.uid(),
            signed_at: NOW,
        };
        let device = key(match &self.credential {
            Credential::Account(c) => device_seed(&c.device_key),
            _ => unreachable!(),
        });
        SignedMembers::sign(m, self.credential.clone(), &device)
    }
}

fn device_seed(device_key: &str) -> u8 {
    (0..=255u8).find(|&s| b64(key(s).verifying_key().as_bytes()) == device_key).unwrap()
}

/// The owner's anchor (no confirmers) and a list naming `managers`, signed by the owner's device 11.
fn pair(managers: &[u8]) -> (SignedAnchor, SignedList) {
    let chain = chain();
    let anchor = SignedAnchor::sign(
        Anchor {
            server_key: server_key(),
            generation: 1,
            owner_head: chain.head(),
            anchor_seq: 1,
            confirmers: vec![],
            signed_at: NOW,
        },
        &key(1),
    );
    let mut managers: Vec<String> = managers.iter().map(|&a| uid(a)).collect();
    managers.sort();
    let list = ManagerList {
        server_key: server_key(),
        generation: 1,
        anchor_hash: anchor.hash(),
        seq: 1,
        managers,
        signer_uid: uid(1),
        signed_at: NOW,
    };
    (anchor, SignedList::sign(list, certified(1, 11), &key(11)))
}

fn descriptor(owner: &Device) -> SignedDescriptor {
    let d = Descriptor {
        server_key: server_key(),
        conversation: "channel:7".into(),
        kind: Kind::Channel,
        incarnation: 0,
        previous_group_id: None,
        private: true,
        history: true,
        founding_owner: Some(uid(1)),
        channel_group_id: None,
        creator_uid: owner.credential.uid(),
        created_at: 2_000,
        creation_nonce: b64(&[9; 16]),
    };
    SignedDescriptor::sign(d, owner.credential.clone(), &owner.signer.0)
}

fn extensions(d: &SignedDescriptor, access: &Access, extra: Option<Extension>) -> Extensions<GroupContext> {
    let mut list = vec![
        Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
            &[ExtensionType::Unknown(DESCRIPTOR_EXTENSION), ExtensionType::Unknown(ACCESS_EXTENSION)],
            &[],
            &[CredentialType::Basic],
        )),
        Extension::Unknown(DESCRIPTOR_EXTENSION, UnknownExtension(d.encode())),
        Extension::ExternalSenders(vec![ExternalSender::new(
            B64.decode(server_key()).unwrap().into(),
            BasicCredential::new(server_credential(&server_key())).into(),
        )]),
        Extension::Unknown(ACCESS_EXTENSION, UnknownExtension(access.encode())),
    ];
    list.extend(extra);
    Extensions::from_vec(list).unwrap()
}

/// The owner's device creates a private channel admitting `members` and `managers`.
fn private_channel(members: &[u8], managers: &[u8]) -> (Device, SignedDescriptor, Observer) {
    let mut owner = Device::new(1, 11);
    let d = descriptor(&owner);
    let (anchor, list) = pair(managers);
    let access = Access::new(anchor, list, owner.members(&d.group_id(), 1, members));
    let config = MlsGroupCreateConfig::builder()
        .ciphersuite(SUITE)
        .capabilities(capabilities())
        .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
        .with_group_context_extensions(extensions(&d, &access, None))
        .build();
    owner.group = Some(
        MlsGroup::new_with_group_id(
            &owner.provider,
            &owner.signer,
            &config,
            GroupId::from_slice(&d.group_id()),
            owner.with_key(),
        )
        .unwrap(),
    );
    let observer = Observer::create_with(&owner.group_info_with_tree(), &server_key(), Some(chain())).unwrap();
    (owner, d, observer)
}

fn join(device: &mut Device, observer: &Observer, owner: &Device) -> (Vec<u8>, Vec<u8>) {
    let info = owner.group_info();
    let MlsMessageBodyIn::GroupInfo(info) = MlsMessageIn::tls_deserialize_exact(&info).unwrap().extract() else {
        panic!()
    };
    let (_, tree) = observer.tree().unwrap();
    let tree = RatchetTreeIn::tls_deserialize_exact(&tree).unwrap();
    let (mut group, bundle) = MlsGroup::external_commit_builder()
        .with_config(MlsGroupJoinConfig::builder().wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY).build())
        .with_ratchet_tree(tree)
        .with_aad(commit_aad(NOW))
        .build_group(&device.provider, info, device.with_key())
        .unwrap()
        .leaf_node_parameters(LeafNodeParameters::builder().with_capabilities(capabilities()).build())
        .load_psks(device.provider.storage())
        .unwrap()
        .build(device.provider.rand(), device.provider.crypto(), &device.signer, |_| true)
        .unwrap()
        .finalize(&device.provider)
        .unwrap();
    if group.pending_commit().is_some() {
        group.merge_pending_commit(&device.provider).unwrap();
    }
    device.group = Some(group);
    (wire(bundle.into_commit()), device.group_info())
}

fn process(device: &mut Device, commit: &[u8]) {
    let message = MlsMessageIn::tls_deserialize_exact(commit).unwrap().try_into_protocol_message().unwrap();
    let group = device.group.as_mut().unwrap();
    let ProcessedMessageContent::StagedCommitMessage(staged) =
        group.process_message(&device.provider, message).unwrap().into_content()
    else {
        panic!("not a commit")
    };
    group.merge_staged_commit(&device.provider, *staged).unwrap();
}

/// A `GroupContextExtensions` commit by `device` with new lists, removing `remove`.
fn change(device: &mut Device, extensions: Extensions<GroupContext>, remove: &[u32]) -> (Vec<u8>, Vec<u8>) {
    let (provider, signer) = (&device.provider, &device.signer);
    let group = device.group.as_mut().unwrap();
    group.set_aad(commit_aad(NOW));
    let bundle = group
        .commit_builder()
        .propose_group_context_extensions(extensions)
        .unwrap()
        .propose_removals(remove.iter().map(|&i| LeafNodeIndex::new(i)))
        .load_psks(provider.storage())
        .unwrap()
        .build(provider.rand(), provider.crypto(), signer, |_| true)
        .unwrap()
        .stage_commit(provider)
        .unwrap();
    let (commit, _, _) = bundle.into_contents();
    group.merge_pending_commit(provider).unwrap();
    (wire(commit), device.group_info())
}

#[test]
fn a_private_channel_needs_its_owner_record_and_admits_only_its_lists() {
    let (mut owner, _, observer) = private_channel(&[2], &[]);
    assert!(Observer::create(&owner.group_info_with_tree(), &server_key()).is_err(), "fails closed without the chain");
    // A listed member joins; an outsider does not.
    let mut bob = Device::new(2, 22);
    let (commit, info) = join(&mut bob, &observer, &owner);
    let observer = Observer::load(&observer.save(), &server_key()).unwrap().with_owner(Some(chain()));
    let (observer, _) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    process(&mut owner, &commit);
    let mut eve = Device::new(3, 33);
    let (commit, info) = join(&mut eve, &observer, &owner);
    assert_eq!(
        observer.commit(&commit, &info, None, NOW, 0).err(),
        Some(DsError::Invalid("not admitted to the private channel"))
    );
}

/// The owner's channel with Bob (account 2) joined; the observer has followed the join.
fn with_bob() -> (Device, SignedDescriptor, Observer) {
    let (mut owner, d, observer) = private_channel(&[2], &[]);
    let mut bob = Device::new(2, 22);
    let (commit, info) = join(&mut bob, &observer, &owner);
    let (observer, _) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    process(&mut owner, &commit);
    (owner, d, observer)
}

fn bob_leaf(observer: &Observer) -> u32 {
    observer.leaves().into_iter().find(|l| l.uid == uid(2)).unwrap().index
}

#[test]
fn new_lists_that_drop_someone_must_remove_their_leaf_in_the_same_commit() {
    let (mut owner, d, observer) = with_bob();
    let (anchor, list) = pair(&[]);
    let without_bob = Access::new(anchor, list, owner.members(&d.group_id(), 2, &[]));
    let (commit, info) = change(&mut owner, extensions(&d, &without_bob, None), &[]);
    assert_eq!(
        observer.commit(&commit, &info, None, NOW, 0).err(),
        Some(DsError::Invalid("not admitted to the private channel"))
    );
}

#[test]
fn new_lists_with_the_removal_are_accepted() {
    let (mut owner, d, observer) = with_bob();
    let bob = bob_leaf(&observer);
    let (anchor, list) = pair(&[]);
    let without_bob = Access::new(anchor, list, owner.members(&d.group_id(), 2, &[]));
    let (commit, info) = change(&mut owner, extensions(&d, &without_bob, None), &[bob]);
    let (observer, accepted) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    assert_eq!(accepted.removed.iter().map(|l| l.uid.clone()).collect::<Vec<_>>(), vec![uid(2)]);
    assert_eq!(observer.access().unwrap().members().members().members, Vec::<String>::new());
}

#[test]
fn lists_from_a_signer_without_authority_are_refused() {
    let (mut owner, d, observer) = private_channel(&[2], &[]);
    let bob = Device::new(2, 22);
    let (anchor, list) = pair(&[]);
    let forged = Access::new(anchor, list, bob.members(&d.group_id(), 2, &[2]));
    let (commit, info) = change(&mut owner, extensions(&d, &forged, None), &[]);
    assert_eq!(observer.commit(&commit, &info, None, NOW, 0).err(), Some(DsError::Invalid("access lists")));
}

#[test]
fn a_manager_changes_the_lists() {
    // Bob is a manager: he may sign a member list, and the change is accepted from him.
    let (mut owner, d, observer) = private_channel(&[], &[2]);
    let mut bob = Device::new(2, 22);
    let (commit, info) = join(&mut bob, &observer, &owner);
    let (observer, _) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    process(&mut owner, &commit);
    let (anchor, list) = pair(&[2]);
    let with_carol = Access::new(anchor, list, bob.members(&d.group_id(), 2, &[3]));
    let (commit, info) = change(&mut bob, extensions(&d, &with_carol, None), &[]);
    let (observer, _) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    assert_eq!(observer.access().unwrap().members().members().members, vec![uid(3)]);
}
