//! The server's observer against real OpenMLS clients.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signer as _, SigningKey};
use gwar_mls::{
    Credential, DESCRIPTOR_EXTENSION, Descriptor, Kind, SUITE, SignedDescriptor,
    ds::{self, CommitSender, DsError, Observer, commit_aad},
    signer::{Ed25519Signer, server_credential},
};
use openmls::prelude::{
    tls_codec::{Deserialize as _, Serialize as _},
    *,
};
use openmls_rust_crypto::OpenMlsRustCrypto;
use vc_proto::DeviceCertificate;

/// The test's clock (Unix ms) for commit times.
const NOW: i64 = 1_800_000_000_000;

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn uid(account: u8) -> String {
    vc_proto::uid_for_key(key(account).verifying_key().as_bytes())
}

fn server() -> Ed25519Signer {
    Ed25519Signer(key(200))
}

fn server_key() -> String {
    b64(server().0.verifying_key().as_bytes())
}

const DAY: i64 = 86_400_000;

fn certified(account: u8, device: u8) -> Credential {
    certified_until(account, device, NOW + 29 * DAY)
}

fn certified_until(account: u8, device: u8, expires_at: i64) -> Credential {
    let mut c = DeviceCertificate {
        account_key: b64(key(account).verifying_key().as_bytes()),
        device_key: b64(key(device).verifying_key().as_bytes()),
        issued_at: expires_at - 30 * DAY,
        expires_at,
        signature: String::new(),
    };
    c.signature = b64(&key(account).sign(vc_proto::device_statement(&c).as_bytes()).to_bytes());
    Credential::Account(c)
}

fn capabilities() -> Capabilities {
    Capabilities::new(
        None,
        Some(&[SUITE]),
        Some(&[ExtensionType::Unknown(DESCRIPTOR_EXTENSION)]),
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

    fn key_package(&self) -> Vec<u8> {
        self.key_package_for(Lifetime::default())
    }

    fn key_package_for(&self, lifetime: Lifetime) -> Vec<u8> {
        let bundle = KeyPackage::builder()
            .key_package_lifetime(lifetime)
            .leaf_node_capabilities(capabilities())
            .build(SUITE, &self.provider, &self.signer, self.with_key())
            .unwrap();
        bundle.key_package().tls_serialize_detached().unwrap()
    }

    fn create(&mut self, descriptor: &SignedDescriptor, external_sender: &str) {
        self.create_requiring(descriptor, external_sender, &[CredentialType::Basic]);
    }

    fn create_requiring(
        &mut self,
        descriptor: &SignedDescriptor,
        external_sender: &str,
        credentials: &[CredentialType],
    ) {
        let extensions = Extensions::from_vec(vec![
            Extension::RequiredCapabilities(RequiredCapabilitiesExtension::new(
                &[ExtensionType::Unknown(DESCRIPTOR_EXTENSION)],
                &[],
                credentials,
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

    fn group_info(&mut self) -> Vec<u8> {
        let group = self.group.as_ref().unwrap();
        wire(group.export_group_info(self.provider.crypto(), &self.signer, true).unwrap())
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
        group.set_aad(commit_aad(NOW));
        let (commit, welcome, _) = group.add_members(provider, signer, &packages).unwrap();
        group.merge_pending_commit(provider).unwrap();
        let (commit, welcome) = (wire(commit), wire(welcome));
        (commit, welcome, self.group_info())
    }

    fn update(&mut self) -> (Vec<u8>, Vec<u8>) {
        let (provider, signer) = (&self.provider, &self.signer);
        let group = self.group.as_mut().unwrap();
        group.set_aad(commit_aad(NOW));
        let commit = group.self_update(provider, signer, LeafNodeParameters::default()).unwrap().into_commit();
        group.merge_pending_commit(provider).unwrap();
        (wire(commit), self.group_info())
    }

    fn join_external(&mut self, group_info: &[u8]) -> (Vec<u8>, Vec<u8>) {
        let MlsMessageBodyIn::GroupInfo(info) = MlsMessageIn::tls_deserialize_exact(group_info).unwrap().extract()
        else {
            panic!("not a group info")
        };
        let (mut group, bundle) = MlsGroup::external_commit_builder()
            .with_config(join_config())
            .with_aad(commit_aad(NOW))
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
        group.set_aad(commit_aad(NOW));
        let (commit, _, _) = group.commit_to_pending_proposals(provider, signer).unwrap();
        group.merge_pending_commit(provider).unwrap();
        (wire(commit), self.group_info())
    }
}

fn dm_descriptor(creator: &Device, other_account: u8) -> SignedDescriptor {
    let (a, b) = (creator.credential.uid(), uid(other_account));
    let conversation = if a < b { format!("dm:{a}:{b}") } else { format!("dm:{b}:{a}") };
    let descriptor = Descriptor {
        server_key: server_key(),
        conversation,
        kind: Kind::Dm,
        incarnation: 0,
        previous_group_id: None,
        private: false,
        history: true,
        founding_owner: None,
        channel_group_id: None,
        creator_uid: creator.credential.uid(),
        created_at: 2_000,
        creation_nonce: b64(&[9; 16]),
    };
    SignedDescriptor::sign(descriptor, creator.credential.clone(), &creator.signer.0)
}

/// Alice (account 1, device 11) opens a direct message with Bob (account 2).
fn dm() -> (Device, Observer) {
    let mut alice = Device::new(1, 11);
    let descriptor = dm_descriptor(&alice, 2);
    alice.create(&descriptor, &server_key());
    let observer = Observer::create(&alice.group_info(), &server_key()).unwrap();
    (alice, observer)
}

#[test]
fn key_packages_name_their_device() {
    let bob = Device::new(2, 21);
    let info = ds::key_package(&bob.key_package()).unwrap();
    assert_eq!(info.uid, uid(2));
    assert_eq!(info.device_key, bob.credential.device_key());
    assert!(!info.last_resort);
    assert!(ds::key_package(b"garbage").is_err());
}

#[test]
fn follows_adds_updates_and_external_joins() {
    let (mut alice, observer) = dm();
    assert_eq!(observer.epoch(), 0);
    let bob = Device::new(2, 21);
    let (commit, welcome, info) = alice.add(&[bob.key_package()]);
    let (observer, accepted) = observer.commit(&commit, &info, Some(&welcome), NOW, 0).unwrap();
    assert_eq!(accepted.epoch, 1);
    assert_eq!(accepted.sender, CommitSender::Member(0));
    assert_eq!(accepted.welcome_recipients, vec![bob.credential.device_key().to_owned()]);
    assert_eq!(accepted.added.len(), 1);

    let (commit, info) = alice.update();
    let (observer, accepted) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    assert_eq!(accepted.epoch, 2);

    // Bob's second device joins by itself.
    let mut phone = Device::new(2, 22);
    let (commit, info) = phone.join_external(&alice.group_info());
    let (observer, accepted) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    assert_eq!(accepted.sender, CommitSender::NewMember);
    assert_eq!(accepted.added[0].device_key, phone.credential.device_key());
    assert_eq!(observer.leaves().len(), 3);

    // The saved state comes back identical.
    let restored = Observer::load(&observer.save(), &server_key()).unwrap();
    assert_eq!(restored.leaves(), observer.leaves());
    assert_eq!(restored.epoch(), 3);
}

#[test]
fn a_second_commit_for_the_same_epoch_is_stale() {
    let (mut alice, observer) = dm();
    let mut bob = Device::new(2, 21);
    let (commit, welcome, info) = alice.add(&[bob.key_package()]);
    let (observer, _) = observer.commit(&commit, &info, Some(&welcome), NOW, 0).unwrap();
    let MlsMessageBodyIn::Welcome(welcome) = MlsMessageIn::tls_deserialize_exact(&welcome).unwrap().extract() else {
        panic!("not a welcome")
    };
    bob.group = Some(
        StagedWelcome::new_from_welcome(&bob.provider, &join_config(), welcome, None)
            .unwrap()
            .into_group(&bob.provider)
            .unwrap(),
    );
    let (alice_commit, alice_info) = alice.update();
    let (bob_commit, bob_info) = bob.update();
    let saved = observer.save();
    let (observer, _) = observer.commit(&alice_commit, &alice_info, None, NOW, 0).unwrap();
    assert_eq!(observer.commit(&bob_commit, &bob_info, None, NOW, 0).err(), Some(DsError::Stale));
    // The same commit cannot be applied twice either.
    let (observer, _) =
        Observer::load(&saved, &server_key()).unwrap().commit(&alice_commit, &alice_info, None, NOW, 0).unwrap();
    assert_eq!(observer.commit(&alice_commit, &alice_info, None, NOW, 0).err(), Some(DsError::Stale));
}

#[test]
fn rejects_outsiders_mismatched_group_infos_and_stale_epochs() {
    let (mut alice, observer) = dm();
    let saved = observer.save();

    // Carol is not a participant of Alice and Bob's conversation.
    let carol = Device::new(3, 31);
    let (commit, welcome, info) = alice.add(&[carol.key_package()]);
    assert_eq!(
        observer.commit(&commit, &info, Some(&welcome), NOW, 0).err(),
        Some(DsError::Invalid("not a participant"))
    );

    // A fresh start: a commit whose uploaded GroupInfo is from another epoch.
    let (mut alice, observer) = dm();
    let stale_info = alice.group_info();
    let (commit, welcome, _) = alice.add(&[Device::new(2, 21).key_package()]);
    assert_eq!(
        observer.commit(&commit, &stale_info, Some(&welcome), NOW, 0).err(),
        Some(DsError::Invalid("group info does not match the commit"))
    );

    // A missing Welcome for an added device.
    let (mut alice, observer) = dm();
    let (commit, _, info) = alice.add(&[Device::new(2, 21).key_package()]);
    assert_eq!(observer.commit(&commit, &info, None, NOW, 0).err(), Some(DsError::Invalid("missing welcome")));
    assert!(Observer::load(&saved, &server_key()).is_ok());
    assert!(Observer::load(&saved[..saved.len() - 1], &server_key()).is_err());
}

#[test]
fn server_removal_proposals_are_committed_by_reference() {
    let (mut alice, observer) = dm();
    let bob = Device::new(2, 21);
    let (commit, welcome, info) = alice.add(&[bob.key_package()]);
    let (mut observer, _) = observer.commit(&commit, &info, Some(&welcome), NOW, 0).unwrap();
    let proposal = observer.propose_remove(&server(), 1).unwrap();
    alice.process(&proposal);
    let (commit, info) = alice.commit_pending();
    let (observer, accepted) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    assert_eq!(accepted.removed.len(), 1);
    assert_eq!(accepted.removed[0].device_key, bob.credential.device_key());
    assert_eq!(observer.leaves().len(), 1);
}

#[test]
fn rejects_groups_without_the_server_as_external_sender() {
    let mut alice = Device::new(1, 11);
    let descriptor = dm_descriptor(&alice, 2);
    alice.create(&descriptor, &b64(key(201).verifying_key().as_bytes()));
    assert_eq!(Observer::create(&alice.group_info(), &server_key()).err(), Some(DsError::Invalid("external senders")));
}

#[test]
fn rejects_groups_that_do_not_require_basic_credentials() {
    let mut alice = Device::new(1, 11);
    let descriptor = dm_descriptor(&alice, 2);
    alice.create_requiring(&descriptor, &server_key(), &[]);
    assert_eq!(
        Observer::create(&alice.group_info(), &server_key()).err(),
        Some(DsError::Invalid("required capabilities"))
    );
}

#[test]
fn commit_times_follow_the_server_clock_and_never_go_back() {
    let (mut alice, observer) = dm();
    let saved = observer.save();
    let (commit, info) = alice.update();
    let load = || Observer::load(&saved, &server_key()).unwrap();
    let skew = ds::MAX_CLOCK_SKEW_MS;
    assert_eq!(load().commit(&commit, &info, None, NOW + skew + 1, 0).err(), Some(DsError::Expired));
    assert_eq!(load().commit(&commit, &info, None, NOW - skew - 1, 0).err(), Some(DsError::Early));
    assert_eq!(load().commit(&commit, &info, None, NOW, NOW + 1).err(), Some(DsError::Invalid("commit time")));
    let (_, accepted) = load().commit(&commit, &info, None, NOW + skew, NOW).unwrap();
    assert_eq!(accepted.committed_at, NOW);
    assert_eq!(ds::commit_time(b"gwar commit v1\n01"), None);
}

#[test]
fn an_old_commit_is_expired_even_when_its_key_packages_ran_out() {
    let (mut alice, observer) = dm();
    let saved = observer.save();
    let bob = Device::new(2, 21);
    let (commit, welcome, info) = alice.add(&[bob.key_package_for(Lifetime::new(1))]);
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let load = || Observer::load(&saved, &server_key()).unwrap();
    assert_eq!(load().commit(&commit, &info, Some(&welcome), NOW, 0).err(), Some(DsError::Invalid("commit")));
    let late = NOW + ds::MAX_CLOCK_SKEW_MS + 1;
    assert_eq!(load().commit(&commit, &info, Some(&welcome), late, 0).err(), Some(DsError::Expired));
}

#[test]
fn introduced_leaves_must_be_certified_at_the_commit_time() {
    let (mut alice, observer) = dm();
    let mut bob = Device::new(2, 21);
    bob.credential = certified_until(2, 21, NOW);
    let (commit, welcome, info) = alice.add(&[bob.key_package()]);
    assert_eq!(
        observer.commit(&commit, &info, Some(&welcome), NOW, 0).err(),
        Some(DsError::Invalid("certificate at commit time"))
    );
}

#[test]
fn an_external_join_replaces_only_the_joiners_own_leaf() {
    let (mut alice, observer) = dm();
    let mut phone = Device::new(2, 22);
    let (commit, info) = phone.join_external(&alice.group_info());
    let (observer, _) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    alice.process(&commit);
    let saved = observer.save();

    // The phone lost its state and joins again: its old leaf goes.
    let mut again = Device::new(2, 22);
    let (commit, info) = again.join_external(&alice.group_info());
    let (observer, accepted) = observer.commit(&commit, &info, None, NOW, 0).unwrap();
    assert_eq!(accepted.removed.len(), 1);
    assert_eq!(accepted.removed[0].device_key, phone.credential.device_key());
    assert_eq!(observer.leaves().len(), 2);

    // Alice's account certifying the same device key cannot take Bob's leaf.
    let mut impostor = Device::new(1, 22);
    let (commit, info) = impostor.join_external(&alice.group_info());
    assert_eq!(
        Observer::load(&saved, &server_key()).unwrap().commit(&commit, &info, None, NOW, 0).err(),
        Some(DsError::Invalid("external removal"))
    );
}
