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
    JoinExternal {
        server: String,
        bytes: Vec<u8>,
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
            Request::JoinExternal { server, bytes, now, revocations } => {
                output(a.join_external(&server, &bytes, now, &revocations).map_err(error)?)
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
        }
    }
}
