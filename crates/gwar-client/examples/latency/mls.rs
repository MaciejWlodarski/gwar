//! `LATENCY_SFRAME=1`: both latency clients join a real encrypted call. The
//! sender creates an encrypted channel, its channel MLS group and the call's
//! voice group; the receiver joins both by external commit (tree from the
//! server's tree endpoint). Each engine then installs the voice epoch's MLS
//! exporter keys through the production control path (`KeyChannel`).

use anyhow::{Context, bail};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ed25519_dalek::{Signer as _, SigningKey};
use gwar_client::{Connected, Identity, voice::keys::KeyChannel};
use gwar_mls::{
    Credential, DESCRIPTOR_EXTENSION, Descriptor, Kind, SUITE, SignedDescriptor,
    ds::commit_aad,
    signer::{Ed25519Signer, server_credential},
};
use gwar_proto::{DeviceCertificate, Request};
use openmls::prelude::{
    tls_codec::{Deserialize as _, Serialize as _},
    *,
};
use openmls_rust_crypto::OpenMlsRustCrypto;
use serde_json::{Value, json};

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as i64
}

fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// One device: its connection identity, certificate and MLS state.
pub struct Device {
    provider: OpenMlsRustCrypto,
    signer: Ed25519Signer,
    credential: Credential,
    group: Option<MlsGroup>,
    pub identity: Identity,
    pub certificate: DeviceCertificate,
}

impl Device {
    pub fn new() -> Self {
        let (account, device) =
            (SigningKey::from_bytes(&rand::random::<[u8; 32]>()), SigningKey::from_bytes(&rand::random::<[u8; 32]>()));
        let (account_key, device_key) =
            (b64(account.verifying_key().as_bytes()), b64(device.verifying_key().as_bytes()));
        let (issued_at, expires_at) = (now() - 1000, now() + 29 * 86_400_000);
        let statement = format!("gwar device v1\n{account_key}\n{device_key}\n{issued_at}\n{expires_at}");
        let certificate = json!({"account_key": account_key, "device_key": device_key, "issued_at": issued_at,
            "expires_at": expires_at, "signature": b64(&account.sign(statement.as_bytes()).to_bytes())});
        Self {
            provider: OpenMlsRustCrypto::default(),
            signer: Ed25519Signer(device.clone()),
            credential: Credential::Account(serde_json::from_value(certificate.clone()).unwrap()),
            group: None,
            identity: Identity::import(&b64(device.as_bytes())).unwrap(),
            certificate: serde_json::from_value(certificate).unwrap(),
        }
    }

    fn with_key(&self) -> CredentialWithKey {
        CredentialWithKey {
            credential: BasicCredential::new(self.credential.encode().into_bytes()).into(),
            signature_key: self.signer.0.verifying_key().to_bytes().to_vec().into(),
        }
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

    fn group_info(&self) -> Vec<u8> {
        let group = self.group.as_ref().unwrap();
        let tree = group.epoch().as_u64() == 0;
        wire(group.export_group_info(self.provider.crypto(), &self.signer, tree).unwrap())
    }

    fn join_external(&mut self, group_info: &[u8], tree: &[u8]) -> Vec<u8> {
        let MlsMessageBodyIn::GroupInfo(info) = MlsMessageIn::tls_deserialize_exact(group_info).unwrap().extract()
        else {
            panic!("not a group info")
        };
        let join = MlsGroupJoinConfig::builder()
            .use_ratchet_tree_extension(true)
            .wire_format_policy(PURE_PLAINTEXT_WIRE_FORMAT_POLICY)
            .build();
        let (mut group, bundle) = MlsGroup::external_commit_builder()
            .with_ratchet_tree(RatchetTreeIn::tls_deserialize_exact(tree).unwrap())
            .with_config(join)
            .with_aad(commit_aad(now()))
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
        wire(bundle.into_commit())
    }

    fn process(&mut self, message: &[u8]) {
        let message = MlsMessageIn::tls_deserialize_exact(message).unwrap().try_into_protocol_message().unwrap();
        let group = self.group.as_mut().unwrap();
        match group.process_message(&self.provider, message).unwrap().into_content() {
            ProcessedMessageContent::StagedCommitMessage(staged) => {
                group.merge_staged_commit(&self.provider, *staged).unwrap()
            }
            _ => panic!("unexpected message"),
        }
    }

    fn descriptor(&self, kind: Kind, channel: u32, server: &str, parent: Option<&str>) -> SignedDescriptor {
        let conversation = if kind == Kind::Voice { format!("voice:{channel}") } else { format!("channel:{channel}") };
        SignedDescriptor::sign(
            Descriptor {
                server_key: server.into(),
                conversation,
                kind,
                incarnation: 0,
                previous_group_id: None,
                private: false,
                // Encrypted channels keep history by default; a voice group never does.
                history: kind == Kind::Channel,
                founding_owner: None,
                channel_group_id: parent.map(str::to_owned),
                creator_uid: self.credential.uid(),
                created_at: now() - 500,
                creation_nonce: b64(&rand::random::<[u8; 16]>()),
            },
            self.credential.clone(),
            &self.signer.0,
        )
    }

    /// The voice epoch's install body: every leaf's exporter, own leaf marked.
    pub fn install_controls(&self, binding: &Value) -> Vec<Value> {
        let group = self.group.as_ref().unwrap();
        let members: Vec<Value> = group
            .members()
            .map(|m| {
                let leaf = m.index.u32();
                let key =
                    group.export_secret(self.provider.crypto(), "gwar voice v1", &leaf.to_be_bytes(), 32).unwrap();
                let credential = Credential::parse(m.credential.serialized_content()).unwrap();
                json!({ "leaf": leaf, "device": credential.device_key(), "key": b64(&key) })
            })
            .collect();
        let epoch = group.epoch().as_u64().to_string();
        let own = group.own_leaf_index().u32();
        [
            json!({ "action": "supersede" }),
            json!({ "action": "install", "own_leaf": own, "members": members, "removed": [] }),
            json!({ "action": "activate", "send_until": null }),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, mut body)| {
            body["binding"] = binding.clone();
            body["serial"] = json!((i + 1).to_string());
            body["epoch"] = json!(epoch);
            body
        })
        .collect()
    }
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

fn wire(message: impl Into<MlsMessageOut>) -> Vec<u8> {
    message.into().tls_serialize_detached().unwrap()
}

async fn request(client: &Connected, op: &str, d: Value) -> anyhow::Result<Value> {
    let request: Request =
        serde_json::from_value(json!({ "op": op, "d": d })).with_context(|| format!("{op} request"))?;
    client.connection.request(request).await.with_context(|| op.to_owned())
}

async fn tree(http: std::net::SocketAddr, client: &Connected, group: &str) -> anyhow::Result<Vec<u8>> {
    let reply = reqwest::Client::new()
        .get(format!("http://{http}/api/mls/{group}/tree"))
        .bearer_auth(&client.welcome.file_access)
        .send()
        .await?;
    if !reply.status().is_success() {
        bail!("tree endpoint: {}", reply.status());
    }
    Ok(reply.bytes().await?.to_vec())
}

/// Joins `group` by external commit and returns the commit.
async fn join(
    http: std::net::SocketAddr,
    client: &Connected,
    device: &mut Device,
    group: &str,
    voice: Option<&Value>,
) -> anyhow::Result<Vec<u8>> {
    let info = request(client, "mls.group_info", json!({ "group": group })).await?;
    let tree = tree(http, client, group).await?;
    let commit = device.join_external(&B64.decode(info["group_info"].as_str().context("group info")?)?, &tree);
    let mut body = json!({ "group": group, "epoch": info["epoch"], "commit": b64(&commit), "group_info": b64(&device.group_info()) });
    if let Some(voice) = voice {
        body["voice"] = json!({ "call": voice["call"], "participation": voice["participation"] });
    }
    request(client, "mls.commit", body).await?;
    Ok(commit)
}

/// Sets up the encrypted call; returns each side's voice state (`voice.state`).
pub async fn encrypted_call(
    http: std::net::SocketAddr,
    admin_token: &str,
    (a, sender): (&Connected, &mut Device),
    (b, receiver): (&Connected, &mut Device),
) -> anyhow::Result<(Value, Value)> {
    request(a, "token.redeem", json!({ "token": admin_token })).await?;
    let created = request(a, "channel.create", json!({ "name": "latency", "encrypted": true })).await?;
    let channel = created["id"].as_u64().context("channel id")? as u32;
    let server = a.connection.server_key.clone();
    let mut parent = sender.clone_signer();
    let d = parent.descriptor(Kind::Channel, channel, &server, None);
    parent.create(&d, &server);
    let created = request(
        a,
        "mls.group.create",
        json!({ "conversation": d.descriptor.conversation, "group_info": b64(&parent.group_info()) }),
    )
    .await?;
    let parent_group = created["group"].as_str().context("parent group")?.to_owned();
    let mut receiver_parent = receiver.clone_signer();
    join(http, b, &mut receiver_parent, &parent_group, None).await?;
    let mut states = Vec::new();
    for client in [a, b] {
        request(client, "channel.join", json!({ "channel": channel })).await?;
        states.push(request(client, "voice.state", json!({})).await?["voice"].clone());
    }
    let d = sender.descriptor(Kind::Voice, channel, &server, Some(&parent_group));
    sender.create(&d, &server);
    let created = request(a, "mls.group.create", json!({ "conversation": d.descriptor.conversation,
        "group_info": b64(&sender.group_info()), "voice": { "call": states[0]["call"], "participation": states[0]["participation"] } })).await?;
    let voice_group = created["group"].as_str().context("voice group")?.to_owned();
    let commit = join(http, b, receiver, &voice_group, Some(&states[1])).await?;
    sender.process(&commit);
    let (mut a_state, mut b_state) = (states.remove(0), states.remove(0));
    a_state["group"] = json!(voice_group);
    b_state["group"] = json!(voice_group);
    Ok((a_state, b_state))
}

impl Device {
    /// The same device keys with fresh MLS state (a second group membership).
    fn clone_signer(&self) -> Self {
        Self {
            provider: OpenMlsRustCrypto::default(),
            signer: Ed25519Signer(self.signer.0.clone()),
            credential: self.credential.clone(),
            group: None,
            identity: self.identity.clone(),
            certificate: self.certificate.clone(),
        }
    }
}

/// Installs and activates the voice epoch through the engine's key channel.
pub async fn activate(keys: &KeyChannel, device: &Device, binding: &Value) -> anyhow::Result<()> {
    for body in device.install_controls(binding) {
        let reply = keys.control(body).await;
        if reply.result != Some(gwar_client::voice::sframe::Outcome::Applied) {
            bail!("key control failed: {reply:?}");
        }
    }
    Ok(())
}
