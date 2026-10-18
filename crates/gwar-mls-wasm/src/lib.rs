//! Byte arrays and plain objects at the browser boundary; no UI or persistence.

use gwar_mls::{
    Agent as Core, Credential, Revocations, SignedDescriptor,
    agent::{Changes, Reconcile, Stores},
    history::DeviceHpkeKey,
};
use serde::{Deserialize, Serialize};
use wasm_bindgen::prelude::*;

#[wasm_bindgen(
    inline_js = "export function encode(value) { return JSON.stringify(value, (_, v) => { if (typeof v === 'number' && !Number.isSafeInteger(v)) throw new Error('expected a safe integer'); return ArrayBuffer.isView(v) ? Array.from(v) : v; }); }"
)]
extern "C" {
    #[wasm_bindgen(catch)]
    fn encode(value: &JsValue) -> Result<String, JsValue>;
}
fn input<T: serde::de::DeserializeOwned>(value: JsValue) -> Result<T, JsValue> {
    serde_json::from_slice(encode(&value)?.as_bytes()).map_err(error)
}
fn output<T: Serialize>(value: T) -> Result<JsValue, JsValue> {
    js_sys::JSON::parse(&serde_json::to_string(&value).map_err(error)?)
}
fn error(e: impl std::fmt::Display) -> JsValue {
    js_sys::Error::new(&e.to_string()).into()
}
fn seed(bytes: &[u8]) -> Result<[u8; 32], JsValue> {
    bytes.try_into().map_err(|_| error("expected a 32-byte seed"))
}
fn credential(bytes: &[u8]) -> Result<Credential, JsValue> {
    Credential::parse(bytes).ok_or_else(|| error("invalid credential"))
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum OpenResult {
    Ok(Vec<u8>),
    Reason(OpenFailure),
}
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum OpenFailure {
    NoKey,
    CannotOpen,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revoked {
    account_key: String,
    device_key: String,
    revoked_at: i64,
}
#[derive(Default, Deserialize)]
#[serde(transparent)]
struct View(Vec<Revoked>);
impl Revocations for View {
    fn revoked_at(&self, account: &str, device: &str) -> Option<i64> {
        self.0.iter().filter(|r| r.account_key == account && r.device_key == device).map(|r| r.revoked_at).min()
    }
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    Inventory {
        server: String,
    },
    SealMessage {
        context: gwar_mls::chat::Context,
        plaintext: Vec<u8>,
        now: i64,
        revocations: View,
    },
    OpenMessage {
        context: gwar_mls::chat::Context,
        ciphertext: Vec<u8>,
    },
    KeyPackages {
        count: usize,
        now: i64,
    },
    LastResortKeyPackage {
        now: i64,
    },
    Create {
        descriptor: Vec<u8>,
    },
    Settle {
        server: String,
        group: [u8; 32],
        winner: Option<[u8; 32]>,
    },
    Welcome {
        server: String,
        id: String,
        bytes: Vec<u8>,
    },
    ReplaceStaleWelcome {
        server: String,
        group: [u8; 32],
        id: String,
        bytes: Vec<u8>,
    },
    JoinExternal {
        server: String,
        bytes: Vec<u8>,
        tree: Option<Vec<u8>>,
        now: i64,
        revocations: View,
    },
    Commit {
        server: String,
        group: [u8; 32],
        adds: Vec<Vec<u8>>,
        removes: Vec<u32>,
        now: i64,
        revocations: View,
    },
    Receive {
        server: String,
        group: [u8; 32],
        commit: Vec<u8>,
        proposals: Vec<Vec<u8>>,
    },
    Proposal {
        server: String,
        group: [u8; 32],
        bytes: Vec<u8>,
    },
    Pending {
        server: String,
        group: [u8; 32],
        pending: bool,
    },
    Reconcile {
        server: String,
        group: [u8; 32],
        result: Reconcile,
    },
    Gate {
        server: String,
        group: [u8; 32],
        now: i64,
        revocations: View,
    },
    SendingKey {
        server: String,
        group: [u8; 32],
        now: i64,
        revocations: View,
    },
    ShareHistory {
        server: String,
        group: [u8; 32],
        recipient: DeviceHpkeKey,
        now: i64,
        revocations: View,
    },
    ImportHistory {
        server: String,
        group: [u8; 32],
        bytes: Vec<u8>,
    },
    DeviceHpkeKey,
    Group {
        server: String,
        group: [u8; 32],
    },
    Epoch {
        server: String,
        group: [u8; 32],
        #[serde(with = "gwar_mls::agent::epoch_number")]
        epoch: u64,
    },
    Outbox {
        server: String,
        group: [u8; 32],
    },
    Forget {
        server: String,
        group: [u8; 32],
    },
    AcceptChain {
        server: String,
        conversation: String,
        chain: Vec<Incarnation>,
    },
    Behind {
        server: String,
        group: [u8; 32],
        bytes: Vec<u8>,
        tree: Option<Vec<u8>>,
    },
    VoicePending {
        server: String,
        group: [u8; 32],
        #[serde(with = "gwar_mls::agent::epoch_number")]
        epoch: u64,
        proposals: Vec<Vec<u8>>,
    },
}
/// Voice queries that read the separately validated parent channel agent.
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum ParentRequest {
    VoiceKeys { server: String, group: [u8; 32], now: i64, revocations: View },
    VoiceParentRemovals { server: String, group: [u8; 32], synchronized: bool },
}
#[derive(Serialize)]
struct VoiceKey {
    leaf: u32,
    device: String,
    key: Vec<u8>,
}
#[derive(Serialize)]
struct VoiceKeys {
    epoch: String,
    own_leaf: u32,
    members: Vec<VoiceKey>,
    send_until: Option<i64>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Incarnation {
    group: [u8; 32],
    descriptor: Vec<u8>,
}

#[wasm_bindgen]
pub struct Agent {
    inner: Core,
}
#[wasm_bindgen]
impl Agent {
    #[wasm_bindgen(constructor)]
    pub fn new(seed_bytes: &[u8], credential_bytes: &[u8], now: f64) -> Result<Agent, JsValue> {
        if !now.is_finite() || now.fract() != 0.0 || !(1.0..=9_007_199_254_740_991.0).contains(&now) {
            return Err(error("invalid time"));
        }
        Ok(Self { inner: Core::new(seed(seed_bytes)?, credential(credential_bytes)?, now as i64).map_err(error)? })
    }
    /// `records` uses the Changes shape, containing every stored key/value pair.
    pub fn load(seed_bytes: &[u8], credential_bytes: &[u8], records: JsValue) -> Result<Agent, JsValue> {
        let records: Changes = input(records)?;
        let mut stores = Stores::default();
        records.apply(&mut stores);
        Ok(Self { inner: Core::load(seed(seed_bytes)?, credential(credential_bytes)?, stores).map_err(error)? })
    }
    /// A volatile voice agent: its store lives only in the caller's memory.
    pub fn new_voice(seed_bytes: &[u8], credential_bytes: &[u8], now: f64) -> Result<Agent, JsValue> {
        if !now.is_finite() || now.fract() != 0.0 || !(1.0..=9_007_199_254_740_991.0).contains(&now) {
            return Err(error("invalid time"));
        }
        Ok(Self {
            inner: Core::new_voice(seed(seed_bytes)?, credential(credential_bytes)?, now as i64).map_err(error)?,
        })
    }
    pub fn load_voice(seed_bytes: &[u8], credential_bytes: &[u8], records: JsValue) -> Result<Agent, JsValue> {
        let records: Changes = input(records)?;
        let mut stores = Stores::default();
        records.apply(&mut stores);
        Ok(Self { inner: Core::load_voice(seed(seed_bytes)?, credential(credential_bytes)?, stores).map_err(error)? })
    }
    /// Exporter keys are returned only to the trusted local controller; never log them.
    pub fn execute_with_parent(&self, request: JsValue, parent: &Agent) -> Result<JsValue, JsValue> {
        let text = encode(&request)?;
        match serde_json::from_slice(text.as_bytes()).map_err(error)? {
            ParentRequest::VoiceKeys { server, group, now, revocations } => {
                let keys = self.inner.voice_keys(&server, &group, &parent.inner, now, &revocations).map_err(error)?;
                output(VoiceKeys {
                    epoch: keys.epoch.to_string(),
                    own_leaf: keys.own_leaf,
                    members: keys
                        .members
                        .into_iter()
                        .map(|m| VoiceKey { leaf: m.leaf, device: m.device, key: m.key.to_vec() })
                        .collect(),
                    send_until: keys.send_until,
                })
            }
            ParentRequest::VoiceParentRemovals { server, group, synchronized } => {
                output(self.inner.voice_parent_removals(&server, &group, &parent.inner, synchronized).map_err(error)?)
            }
        }
    }
    /// The server's owner chain against this device's pin (docs/e2e-build.md, section 6, 6b).
    /// Plain parameters, not a `Request` variant: each serde variant costs bundle size.
    pub fn check_owner(
        &mut self,
        server: &str,
        genesis: &str,
        statements: Vec<String>,
        signatures: Vec<String>,
        invite_owner: Option<String>,
    ) -> Result<JsValue, JsValue> {
        let links = statements.into_iter().zip(signatures).collect();
        output(self.inner.check_owner(server, genesis, links, invite_owner.as_deref()).map_err(error)?)
    }
    /// Accepts an owner reset the user confirmed; clears the old generation's checkpoints.
    pub fn accept_generation(
        &mut self,
        server: &str,
        genesis: &str,
        statements: Vec<String>,
        signatures: Vec<String>,
    ) -> Result<(), JsValue> {
        let links = statements.into_iter().zip(signatures).collect();
        self.inner.accept_generation(server, genesis, links).map_err(error)
    }
    pub fn accept_managers(
        &mut self,
        server: &str,
        anchor: &[u8],
        list: &[u8],
        now: f64,
        revocations: JsValue,
    ) -> Result<JsValue, JsValue> {
        let revocations: View = input(revocations)?;
        output(self.inner.accept_managers(server, anchor, list, now as i64, &revocations).map_err(error)?)
    }
    pub fn owner_view(&self, server: &str) -> Result<JsValue, JsValue> {
        output(self.inner.owner_view(server).map_err(error)?)
    }
    /// The next confirmer anchor's statement, for the owner's account key to sign.
    pub fn next_anchor(
        &self,
        server: &str,
        confirmers: Vec<String>,
        signed_at: f64,
        server_anchor: Option<Vec<u8>>,
    ) -> Result<String, JsValue> {
        self.inner.next_anchor(server, &confirmers, signed_at as i64, server_anchor.as_deref()).map_err(error)
    }
    /// The next manager list's statement, under `anchor` or the accepted anchor.
    /// `managers` `None` renews the server's (verified) or the accepted list.
    pub fn next_list(
        &self,
        server: &str,
        anchor: Option<Vec<u8>>,
        managers: Option<Vec<String>>,
        signed_at: f64,
        server_anchor: Option<Vec<u8>>,
        server_list: Option<Vec<u8>>,
    ) -> Result<String, JsValue> {
        self.inner
            .next_list(
                server,
                anchor.as_deref(),
                managers.as_deref(),
                signed_at as i64,
                server_anchor.as_deref(),
                server_list.as_deref(),
            )
            .map_err(error)
    }
    /// The statement of a link moving ownership, for the owner's account key to sign.
    pub fn transfer_statement(
        &self,
        server: &str,
        account_key: &str,
        new_owner: &str,
        server_anchor: Option<Vec<u8>>,
        signed_at: f64,
    ) -> Result<String, JsValue> {
        self.inner
            .transfer_statement(server, account_key, new_owner, server_anchor.as_deref(), signed_at as i64)
            .map_err(error)
    }
    /// Signs a `gwar managers v1` statement with this device.
    pub fn sign_manager_list(&self, statement: &str) -> Result<Vec<u8>, JsValue> {
        let list = gwar_mls::owner::ManagerList::parse(statement).ok_or_else(|| error("invalid manager list"))?;
        self.inner.sign_manager_list(list).map_err(error)
    }
    pub fn take_changes(&mut self) -> Result<JsValue, JsValue> {
        output(self.inner.take_changes())
    }
    pub fn execute(&mut self, request: JsValue) -> Result<JsValue, JsValue> {
        let a = &mut self.inner;
        let text = encode(&request)?;
        match serde_json::from_slice(text.as_bytes()).map_err(error)? {
            Request::Inventory { server } => output(a.inventory(&server).map_err(error)?),
            Request::SealMessage { context, plaintext, now, revocations } => {
                output(gwar_mls::chat::seal(a, &context, &plaintext, now, &revocations).map_err(error)?)
            }
            Request::OpenMessage { context, ciphertext } => {
                // Missing keys are an expected read result, not a poisoned mutation.
                match gwar_mls::chat::open(a, &context, &ciphertext) {
                    Ok(plaintext) => output(OpenResult::Ok(plaintext)),
                    Err(gwar_mls::chat::ChatError::NoKey) => output(OpenResult::Reason(OpenFailure::NoKey)),
                    Err(gwar_mls::chat::ChatError::CannotOpen | gwar_mls::chat::ChatError::Context) => {
                        output(OpenResult::Reason(OpenFailure::CannotOpen))
                    }
                    Err(e) => Err(error(e)),
                }
            }
            Request::KeyPackages { count, now } => output(a.key_packages(count, now).map_err(error)?),
            Request::LastResortKeyPackage { now } => output(a.last_resort_key_package(now).map_err(error)?),
            Request::Create { descriptor } => output(
                a.create(SignedDescriptor::parse(&descriptor).ok_or_else(|| error("invalid descriptor"))?)
                    .map_err(error)?,
            ),
            Request::Settle { server, group, winner } => {
                output(a.settle(&server, &group, winner.as_ref()).map_err(error)?)
            }
            Request::Welcome { server, id, bytes } => output(a.welcome(&server, &id, &bytes).map_err(error)?),
            Request::ReplaceStaleWelcome { server, group, id, bytes } => {
                output(a.replace_stale_welcome(&server, &group, &id, &bytes).map_err(error)?)
            }
            Request::JoinExternal { server, bytes, tree, now, revocations } => {
                output(a.join_external(&server, &bytes, tree.as_deref(), now, &revocations).map_err(error)?)
            }
            Request::Commit { server, group, adds, removes, now, revocations } => {
                output(a.commit(&server, &group, &adds, &removes, now, &revocations).map_err(error)?)
            }
            Request::Receive { server, group, commit, proposals } => {
                output(a.receive(&server, &group, &commit, &proposals).map_err(error)?)
            }
            Request::Proposal { server, group, bytes } => output(a.proposal(&server, &group, &bytes).map_err(error)?),
            Request::Pending { server, group, pending } => output(a.pending(&server, &group, pending).map_err(error)?),
            Request::Reconcile { server, group, result } => {
                output(a.reconcile(&server, &group, result).map_err(error)?)
            }
            Request::Gate { server, group, now, revocations } => {
                output(a.gate(&server, &group, now, &revocations).map_err(error)?)
            }
            Request::SendingKey { server, group, now, revocations } => output(
                a.sending_key(&server, &group, now, &revocations)
                    .map(|(epoch, key)| (epoch.to_string(), key))
                    .map_err(error)?,
            ),
            Request::ShareHistory { server, group, recipient, now, revocations } => {
                output(a.share_history(&server, &group, &recipient, now, &revocations).map_err(error)?)
            }
            Request::ImportHistory { server, group, bytes } => {
                output(a.import_history(&server, &group, &bytes).map_err(error)?)
            }
            Request::DeviceHpkeKey => output(a.device_hpke_key()),
            Request::Group { server, group } => output(a.group(&server, &group)),
            Request::Epoch { server, group, epoch } => output(a.epoch(&server, &group, epoch)),
            Request::Outbox { server, group } => output(a.outbox(&server, &group)),
            Request::Forget { server, group } => output(a.forget(&server, &group).map_err(error)?),
            Request::AcceptChain { server, conversation, chain } => {
                let chain: Vec<_> = chain.into_iter().map(|i| (i.group, i.descriptor)).collect();
                output(a.accept_chain(&server, &conversation, &chain).map_err(error)?)
            }
            Request::Behind { server, group, bytes, tree } => {
                output(a.behind(&server, &group, &bytes, tree.as_deref()).map_err(error)?)
            }
            Request::VoicePending { server, group, epoch, proposals } => {
                output(a.voice_pending(&server, &group, epoch, &proposals).map_err(error)?)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn optional_tree_uses_the_existing_byte_array_shape() {
        for operation in ["join_external", "behind"] {
            for tree in [None, Some(serde_json::Value::Null), Some(json!([1, 2, 255]))] {
                let mut fields = json!({"server":"s", "bytes":[3,4]});
                if operation == "behind" {
                    fields["group"] = json!(vec![0; 32]);
                } else {
                    fields["now"] = json!(123);
                    fields["revocations"] = json!([]);
                }
                if let Some(tree) = &tree {
                    fields["tree"] = tree.clone();
                }
                let request: Request = serde_json::from_value(json!({operation:fields})).unwrap();
                let parsed = match request {
                    Request::JoinExternal { tree, .. } | Request::Behind { tree, .. } => tree,
                    _ => unreachable!(),
                };
                assert_eq!(parsed, tree.filter(|v| !v.is_null()).map(|_| vec![1, 2, 255]));
            }
        }
    }
}

/// The envelope of a confirmer anchor signed by the owner's account key, verified.
#[wasm_bindgen]
pub fn anchor_envelope(statement: &str, account_key: &str, signature: &str) -> Result<Vec<u8>, JsValue> {
    gwar_mls::owner::SignedAnchor::from_parts(statement, account_key, signature)
        .map(|a| a.encode())
        .ok_or_else(|| error("invalid confirmer anchor"))
}
