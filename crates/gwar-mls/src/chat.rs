//! Version 1 chat envelope. Authenticity of the v3 message signature remains
//! the caller's responsibility after opening (including revision/revocation checks).
use crate::{
    Agent, Revocations,
    agent::AgentError,
    encoding::{b64, fixed},
};
use openmls_rust_crypto::RustCrypto;
use openmls_traits::{crypto::OpenMlsCrypto, random::OpenMlsRand, types::AeadType};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Context {
    pub server_key: String,
    pub conversation: String,
    pub group_id: [u8; 32],
    #[serde(with = "crate::agent::epoch_number")]
    pub epoch: u64,
    pub message_id: [u8; 16],
    pub revision: u32,
}
// Only the authenticated context belongs in the crypto layer. The caller parses
// and validates the full plaintext and verifies the author's v3 signature.
#[derive(Deserialize)]
struct PlaintextContext {
    signature: SignatureContext,
}
#[derive(Deserialize)]
struct SignatureContext {
    server_key: String,
    conversation: String,
    group_id: String,
    epoch: String,
    message_id: String,
    revision: u32,
}
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ChatError {
    #[error("no epoch key yet")]
    NoKey,
    #[error("cannot open message")]
    CannotOpen,
    #[error("message context mismatch")]
    Context,
    #[error(transparent)]
    Agent(#[from] AgentError),
}
impl Context {
    fn aad(&self) -> Result<String, ChatError> {
        if fixed::<32>(&self.server_key).is_none() || self.conversation.contains(['\r', '\n']) {
            return Err(ChatError::Context);
        }
        Ok(format!(
            "gwar chat ciphertext v1\n{}\n{}\n{}\n{}\n{}\n{}",
            self.server_key,
            self.conversation,
            b64(&self.group_id),
            self.epoch,
            b64(&self.message_id),
            self.revision
        ))
    }
    fn matches(&self, bytes: &[u8]) -> bool {
        let Ok(p) = serde_json::from_slice::<PlaintextContext>(bytes) else { return false };
        let s = &p.signature;
        s.server_key == self.server_key
            && s.conversation == self.conversation
            && s.group_id == b64(&self.group_id)
            && s.epoch == self.epoch.to_string()
            && s.message_id == b64(&self.message_id)
            && s.revision == self.revision
    }
    fn key(&self, epoch_key: &[u8; 32]) -> [u8; 32] {
        let mut info = b"gwar message key v1".to_vec();
        info.extend(self.message_id);
        info.extend(self.revision.to_be_bytes());
        let mut key = [0; 32];
        hkdf::Hkdf::<Sha256>::new(Some(&[]), epoch_key).expand(&info, &mut key).expect("32-byte HKDF");
        key
    }
}
/// Sealing always recomputes the agent's sending gate and checks the signed epoch.
pub fn seal(
    agent: &Agent,
    context: &Context,
    plaintext: &[u8],
    now: i64,
    revocations: &dyn Revocations,
) -> Result<Vec<u8>, ChatError> {
    let (epoch, key) = agent.sending_key(&context.server_key, &context.group_id, now, revocations)?;
    if epoch != context.epoch || !context.matches(plaintext) || !group_matches(agent, context) {
        return Err(ChatError::Context);
    }
    let nonce = RustCrypto::default().random_array().map_err(|_| ChatError::CannotOpen)?;
    seal_bytes(context, &key, plaintext, &nonce)
}
fn group_matches(agent: &Agent, c: &Context) -> bool {
    agent
        .group(&c.server_key, &c.group_id)
        .and_then(|g| crate::SignedDescriptor::parse(&g.descriptor))
        .is_some_and(|d| d.descriptor.conversation == c.conversation)
}
fn seal_bytes(c: &Context, key: &[u8; 32], plaintext: &[u8], nonce: &[u8; 12]) -> Result<Vec<u8>, ChatError> {
    let sealed = RustCrypto::default()
        .aead_encrypt(AeadType::Aes256Gcm, &c.key(key), plaintext, nonce, c.aad()?.as_bytes())
        .map_err(|_| ChatError::CannotOpen)?;
    let mut bytes = nonce.to_vec();
    bytes.extend(sealed);
    Ok(bytes)
}
pub fn open(agent: &Agent, context: &Context, ciphertext: &[u8]) -> Result<Vec<u8>, ChatError> {
    if agent.group(&context.server_key, &context.group_id).is_none() {
        return Err(ChatError::NoKey);
    }
    if !group_matches(agent, context) {
        return Err(ChatError::Context);
    }
    let key = agent
        .epoch(&context.server_key, &context.group_id, context.epoch)
        .and_then(|e| e.key())
        .ok_or(ChatError::NoKey)?;
    open_bytes(context, key, ciphertext)
}
fn open_bytes(c: &Context, key: &[u8; 32], ciphertext: &[u8]) -> Result<Vec<u8>, ChatError> {
    if ciphertext.len() < 28 {
        return Err(ChatError::CannotOpen);
    }
    let bytes = RustCrypto::default()
        .aead_decrypt(AeadType::Aes256Gcm, &c.key(key), &ciphertext[12..], &ciphertext[..12], c.aad()?.as_bytes())
        .map_err(|_| ChatError::CannotOpen)?;
    if !c.matches(&bytes) {
        return Err(ChatError::Context);
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vc_proto::{Attachment, MessageSignature};
    #[derive(Clone, Serialize)]
    struct Plaintext {
        text: String,
        mentions: Vec<String>,
        attachments: Vec<Attachment>,
        signature: MessageSignature,
    }
    fn fixture() -> (Context, Plaintext) {
        let c = Context {
            server_key: b64(&[1; 32]),
            conversation: "dm:a:b".into(),
            group_id: [2; 32],
            epoch: 7,
            message_id: [3; 16],
            revision: 9,
        };
        let p = Plaintext {
            text: "hello".into(),
            mentions: vec![],
            attachments: vec![],
            signature: MessageSignature {
                server_key: c.server_key.clone(),
                conversation: c.conversation.clone(),
                group_id: b64(&c.group_id),
                epoch: "7".into(),
                message_id: b64(&c.message_id),
                revision: 9,
                ..Default::default()
            },
        };
        (c, p)
    }
    #[test]
    fn envelope_and_all_aad_fields() {
        let (c, p) = fixture();
        let bytes = seal_bytes(&c, &[4; 32], &serde_json::to_vec(&p).unwrap(), &[5; 12]).unwrap();
        assert_eq!(open_bytes(&c, &[4; 32], &bytes).unwrap(), serde_json::to_vec(&p).unwrap());
        for mutate in [
            (|c: &mut Context| c.server_key = b64(&[9; 32])) as fn(&mut Context),
            |c| c.conversation.push('x'),
            |c| c.group_id[0] ^= 1,
            |c| c.epoch += 1,
            |c| c.message_id[0] ^= 1,
            |c| c.revision += 1,
        ] {
            let mut altered = c.clone();
            mutate(&mut altered);
            assert!(open_bytes(&altered, &[4; 32], &bytes).is_err());
        }
        assert!(open_bytes(&c, &[6; 32], &bytes).is_err());
        for length in 0..bytes.len() {
            assert!(open_bytes(&c, &[4; 32], &bytes[..length]).is_err());
        }
        for index in [0, 12, bytes.len() - 1] {
            let mut altered = bytes.clone();
            altered[index] ^= 1;
            assert!(open_bytes(&c, &[4; 32], &altered).is_err());
        }
    }
    #[test]
    fn authenticated_inner_fields_must_match() {
        let (c, p) = fixture();
        for mutate in [
            (|s: &mut MessageSignature| s.server_key.push('x')) as fn(&mut MessageSignature),
            |s| s.conversation.push('x'),
            |s| s.group_id.push('x'),
            |s| s.epoch.push('0'),
            |s| s.message_id.push('x'),
            |s| s.revision += 1,
        ] {
            let mut altered = p.clone();
            mutate(&mut altered.signature);
            let bytes = seal_bytes(&c, &[4; 32], &serde_json::to_vec(&altered).unwrap(), &[5; 12]).unwrap();
            assert_eq!(open_bytes(&c, &[4; 32], &bytes).unwrap_err(), ChatError::Context);
        }
    }
    #[test]
    fn preserves_plaintext_json_bytes_and_only_parses_signature_context() {
        let (c, p) = fixture();
        let mut value = serde_json::to_value(p).unwrap();
        value["text"] = serde_json::json!("hello 👋");
        value["future_field"] = serde_json::json!({"opaque": [1, 2, 3]});
        let bytes = serde_json::to_vec_pretty(&value).unwrap();
        assert!(c.matches(&bytes));
        let sealed = seal_bytes(&c, &[4; 32], &bytes, &[5; 12]).unwrap();
        assert_eq!(open_bytes(&c, &[4; 32], &sealed).unwrap(), bytes);
        for invalid in [b"{}".as_slice(), b"{invalid", b"null", b"[]"] {
            assert!(!c.matches(invalid));
            let sealed = seal_bytes(&c, &[4; 32], invalid, &[5; 12]).unwrap();
            assert_eq!(open_bytes(&c, &[4; 32], &sealed).unwrap_err(), ChatError::Context);
        }
    }
    #[test]
    fn fixed_vector() {
        // Independently generated using Node HKDF-SHA256 and AES-256-GCM.
        let (c, p) = fixture();
        assert_eq!(b64(&c.key(&[4; 32])), "294h1ajzcxw_a65VfFeDd9hcAJ24xYbbj2UnSZGEZN8");
        const VECTOR: &str = "BQUFBQUFBQUFBQUF_k3btA2LY0uV3vrhoaCzTaEQ17AyoRzC4q74YwSiRjHsTvkxsojyauKhgzvvBtn69HYmsgFJjHIwwIn8Jl7kgS1jPjXSA6GpXnMiBmXKwsQSlodyHtjrpi820c9AHLr-77PCQ8Jak_MpxvO2G3qBqinzbAcqdq1sJ197KlsAuqZjaL9HaGAuHf--5ZH-4QgCsShuT105vdXVBie1417LoHvlPMpIuCtP-RAYn0c5kAHYzF5lhwU6kgvr7bJG9v5sgLjGU8vmApa6GKyaDrKeAOwSn5gwZdd2pTn4wQmaMoGU6hde8hnnWq5qox10KKt3zxKZgbKmDfGM1N_7qZdqmKbvIldphWQOSIxiaiTCLPgOmv2wkFTM65FD2JH_E2oZTF08TDqoaKZHPwUzgHXtzCkzNMfILSiQQIDuGbmZYlHZrpKGxnjaXbNaREfRRqhnkj7l48h136_6j_J_18yVGeEg5NTkSgUEqsR2zRsaU_BSVQMD0UEkPTRiD8_-_qMLaKVRUriO9E1V6n7O2YPwu5XQuMmJknfPzP5V37sL506keEP8PUjygacwhhXl1JLBRw";
        assert_eq!(b64(&seal_bytes(&c, &[4; 32], &serde_json::to_vec(&p).unwrap(), &[5; 12]).unwrap()), VECTOR);
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(VECTOR).unwrap();
        assert_eq!(open_bytes(&c, &[4; 32], &bytes).unwrap(), serde_json::to_vec(&p).unwrap());
    }
}
