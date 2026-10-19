//! History bundles: epoch keys a member device seals to another device's HPKE
//! key, in chunks (docs/e2e-build.md, "History bundles"). This module is the
//! envelope; which epochs to share and how imports are kept is the agent's.

use crate::crypto::Crypto;
use ed25519_dalek::{Signer as _, SigningKey};
use openmls_traits::{
    crypto::OpenMlsCrypto,
    types::{HpkeAeadType, HpkeCiphertext, HpkeConfig, HpkeKdfType, HpkeKemType},
};
use sha2::{Digest, Sha256};

use crate::encoding::{b64, decimal, fixed, netstrings, parse_netstrings};

/// At most this many epochs per chunk.
pub const MAX_CHUNK_EPOCHS: u64 = 1000;
/// Decoded size limit of a chunk on the wire.
pub const MAX_CHUNK_BYTES: usize = 48 * 1024;

const ENTRY: usize = 8 + 32;
const HPKE: HpkeConfig = HpkeConfig(HpkeKemType::DhKem25519, HpkeKdfType::HkdfSha256, HpkeAeadType::AesGcm128);

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HistoryError {
    #[error("invalid entries")]
    Entries,
    #[error("invalid header")]
    Header,
    #[error("the signing key is not the header's sender")]
    Sender,
    #[error("chunk too large")]
    TooLarge,
    #[error("cannot open the chunk")]
    Open,
}

/// A device's X25519 key pair for history bundles, derived from 32 random bytes.
pub fn hpke_key_pair(ikm: &[u8; 32]) -> ([u8; 32], [u8; 32]) {
    let pair = Crypto::default().derive_hpke_keypair(HPKE, ikm).expect("X25519 key derivation");
    let private = pair.private.as_ref().try_into().expect("32-byte X25519 private key");
    let public = pair.public.as_slice().try_into().expect("32-byte X25519 public key");
    (private, public)
}

/// The published device HPKE key, signed by the device key:
/// `gwar device hpke v1\n{device_key}\n{hpke_key}\n{created_at}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceHpkeKey {
    pub device_key: String,
    pub hpke_key: String,
    pub created_at: i64,
    pub signature: String,
}

impl DeviceHpkeKey {
    fn statement(device_key: &str, hpke_key: &str, created_at: i64) -> String {
        format!("gwar device hpke v1\n{device_key}\n{hpke_key}\n{created_at}")
    }

    pub fn sign(device: &SigningKey, hpke_public: &[u8; 32], created_at: i64) -> Self {
        let device_key = b64(device.verifying_key().as_bytes());
        let hpke_key = b64(hpke_public);
        let signature = b64(&device.sign(Self::statement(&device_key, &hpke_key, created_at).as_bytes()).to_bytes());
        Self { device_key, hpke_key, created_at, signature }
    }

    /// Canonical fields and a valid signature by `device_key`.
    pub fn verify(&self) -> bool {
        fixed::<32>(&self.device_key).is_some()
            && fixed::<32>(&self.hpke_key).is_some()
            && (0..=gwar_proto::MAX_SAFE_INTEGER).contains(&self.created_at)
            && gwar_proto::signed_by(
                &self.device_key,
                &Self::statement(&self.device_key, &self.hpke_key, self.created_at),
                &self.signature,
            )
    }
}

/// What a chunk says about itself; also the HPKE `aad`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    pub server_key: String,
    pub group_id: [u8; 32],
    pub recipient_device_key: String,
    pub recipient_hpke_key: String,
    pub sender_device_key: String,
    pub first_epoch: u64,
    pub last_epoch: u64,
    pub created_at: i64,
}

const HEADER: &str = "gwar history chunk v1";

impl Header {
    pub fn encode(&self) -> String {
        [
            HEADER,
            &self.server_key,
            &b64(&self.group_id),
            &self.recipient_device_key,
            &self.recipient_hpke_key,
            &self.sender_device_key,
            &self.first_epoch.to_string(),
            &self.last_epoch.to_string(),
            &self.created_at.to_string(),
        ]
        .join("\n")
    }

    pub fn parse(text: &str) -> Option<Self> {
        let fields: Vec<&str> = text.split('\n').collect();
        let [label, server_key, group_id, recipient, recipient_hpke, sender, first, last, created_at] = fields[..]
        else {
            return None;
        };
        for key in [server_key, recipient, recipient_hpke, sender] {
            fixed::<32>(key)?;
        }
        let header = Self {
            server_key: server_key.to_owned(),
            group_id: fixed(group_id)?,
            recipient_device_key: recipient.to_owned(),
            recipient_hpke_key: recipient_hpke.to_owned(),
            sender_device_key: sender.to_owned(),
            first_epoch: decimal(first)?,
            last_epoch: decimal(last)?,
            created_at: decimal(created_at)? as i64,
        };
        (label == HEADER && header.spans_valid_range()).then_some(header)
    }

    fn spans_valid_range(&self) -> bool {
        self.first_epoch <= self.last_epoch && self.last_epoch - self.first_epoch < MAX_CHUNK_EPOCHS
    }

    fn info(&self) -> Vec<u8> {
        [b"gwar history v1".as_slice(), &self.group_id].concat()
    }

    fn signed(&self, enc: &[u8], ciphertext: &[u8]) -> String {
        let hash = Sha256::new().chain_update(enc).chain_update(ciphertext).finalize();
        format!("{}\n{}", self.encode(), b64(&hash))
    }
}

/// A parsed chunk. Its recipient checks [`Chunk::is_for`], then
/// [`Chunk::verify`], then opens it; the server checks the first two.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub header: Header,
    enc: [u8; 32],
    ciphertext: Vec<u8>,
    signature: [u8; 64],
}

impl Chunk {
    /// Seals `entries` (epoch, epoch key), sorted, unique and within the
    /// header's range, signed by the sender's device key.
    pub fn seal(header: Header, entries: &[(u64, [u8; 32])], sender: &SigningKey) -> Result<Vec<u8>, HistoryError> {
        let text = header.encode();
        if Header::parse(&text).as_ref() != Some(&header) {
            return Err(HistoryError::Header);
        }
        if !valid_entries(&header, entries.iter().map(|(epoch, _)| *epoch)) {
            return Err(HistoryError::Entries);
        }
        if b64(sender.verifying_key().as_bytes()) != header.sender_device_key {
            return Err(HistoryError::Sender);
        }
        let recipient = fixed::<32>(&header.recipient_hpke_key).ok_or(HistoryError::Header)?;
        let plaintext: Vec<u8> =
            entries.iter().flat_map(|(epoch, key)| epoch.to_be_bytes().into_iter().chain(*key)).collect();
        let sealed = Crypto::default()
            .hpke_seal(HPKE, &recipient, &header.info(), text.as_bytes(), &plaintext)
            .map_err(|_| HistoryError::Open)?;
        let (enc, ciphertext) = (sealed.kem_output.as_slice(), sealed.ciphertext.as_slice());
        let signature = sender.sign(header.signed(enc, ciphertext).as_bytes()).to_bytes();
        let bytes = netstrings(&[text.as_bytes(), enc, ciphertext, &signature]);
        if bytes.len() > MAX_CHUNK_BYTES {
            return Err(HistoryError::TooLarge);
        }
        Ok(bytes)
    }

    /// The wire form: netstrings of the header, `enc`, the ciphertext and the signature.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_CHUNK_BYTES {
            return None;
        }
        let [header, enc, ciphertext, signature] = parse_netstrings(bytes, 4)?[..] else {
            return None;
        };
        let header = Header::parse(std::str::from_utf8(header).ok()?)?;
        Some(Self {
            header,
            enc: enc.try_into().ok()?,
            ciphertext: ciphertext.to_vec(),
            signature: signature.try_into().ok()?,
        })
    }

    /// The header names this server, group and recipient device with its current HPKE key.
    pub fn is_for(&self, server_key: &str, group_id: &[u8; 32], device_key: &str, hpke_key: &str) -> bool {
        let h = &self.header;
        h.server_key == server_key
            && &h.group_id == group_id
            && h.recipient_device_key == device_key
            && h.recipient_hpke_key == hpke_key
    }

    /// Signed by the header's sender device key.
    pub fn verify(&self) -> bool {
        gwar_proto::signed_by(
            &self.header.sender_device_key,
            &self.header.signed(&self.enc, &self.ciphertext),
            &b64(&self.signature),
        )
    }

    /// The entries, checked against the header. Call after [`Chunk::verify`].
    pub fn open(&self, hpke_private: &[u8; 32]) -> Result<Vec<(u64, [u8; 32])>, HistoryError> {
        let sealed =
            HpkeCiphertext { kem_output: self.enc.to_vec().into(), ciphertext: self.ciphertext.clone().into() };
        let plaintext = Crypto::default()
            .hpke_open(HPKE, &sealed, hpke_private, &self.header.info(), self.header.encode().as_bytes())
            .map_err(|_| HistoryError::Open)?;
        let (whole, rest) = plaintext.as_chunks::<ENTRY>();
        if !rest.is_empty() {
            return Err(HistoryError::Entries);
        }
        let entries: Vec<(u64, [u8; 32])> = whole
            .iter()
            .map(|e| (u64::from_be_bytes(e[..8].try_into().unwrap()), e[8..].try_into().unwrap()))
            .collect();
        if !valid_entries(&self.header, entries.iter().map(|(epoch, _)| *epoch)) {
            return Err(HistoryError::Entries);
        }
        Ok(entries)
    }
}

/// At least one epoch, strictly increasing, all within the header's range.
fn valid_entries(header: &Header, epochs: impl Iterator<Item = u64>) -> bool {
    let mut previous = None;
    let mut count = 0;
    for epoch in epochs {
        if epoch < header.first_epoch || epoch > header.last_epoch || previous.is_some_and(|p| epoch <= p) {
            return false;
        }
        previous = Some(epoch);
        count += 1;
    }
    count > 0 && header.spans_valid_range()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::tests::{key, public};

    fn header(recipient_hpke: &[u8; 32]) -> Header {
        Header {
            server_key: public(&key(200)),
            group_id: [7; 32],
            recipient_device_key: public(&key(21)),
            recipient_hpke_key: b64(recipient_hpke),
            sender_device_key: public(&key(11)),
            first_epoch: 3,
            last_epoch: 5,
            created_at: 1_800_000_000_000,
        }
    }

    #[test]
    fn device_hpke_keys_are_signed_by_the_device() {
        let (_, hpke) = hpke_key_pair(&[1; 32]);
        let published = DeviceHpkeKey::sign(&key(21), &hpke, 1_800_000_000_000);
        assert!(published.verify());
        let mut other = published.clone();
        other.hpke_key = b64(&hpke_key_pair(&[2; 32]).1);
        assert!(!other.verify());
        other = published.clone();
        other.created_at += 1;
        assert!(!other.verify());
    }

    #[test]
    fn seals_and_opens_for_the_named_recipient_only() {
        let (private, hpke) = hpke_key_pair(&[1; 32]);
        let entries = [(3, [30; 32]), (5, [50; 32])];
        let bytes = Chunk::seal(header(&hpke), &entries, &key(11)).unwrap();
        let chunk = Chunk::parse(&bytes).unwrap();
        assert!(chunk.is_for(&public(&key(200)), &[7; 32], &public(&key(21)), &b64(&hpke)));
        assert!(!chunk.is_for(&public(&key(200)), &[8; 32], &public(&key(21)), &b64(&hpke)));
        assert!(chunk.verify());
        assert_eq!(chunk.open(&private).unwrap(), entries);
        assert_eq!(chunk.open(&hpke_key_pair(&[2; 32]).0), Err(HistoryError::Open));
    }

    #[test]
    fn rejects_bad_entries_senders_and_tampering() {
        let (private, hpke) = hpke_key_pair(&[1; 32]);
        let h = header(&hpke);
        for bad in
            [&[][..], &[(2, [0; 32])], &[(6, [0; 32])], &[(4, [0; 32]), (4, [1; 32])], &[(5, [0; 32]), (4, [1; 32])]]
        {
            assert_eq!(Chunk::seal(h.clone(), bad, &key(11)), Err(HistoryError::Entries));
        }
        assert_eq!(Chunk::seal(h.clone(), &[(3, [0; 32])], &key(12)), Err(HistoryError::Sender));
        let wide = Header { last_epoch: h.first_epoch + MAX_CHUNK_EPOCHS, ..h.clone() };
        assert_eq!(Chunk::seal(wide.clone(), &[(3, [0; 32])], &key(11)), Err(HistoryError::Header));
        for bad in [
            Header { created_at: -1, ..h.clone() },
            Header { server_key: "x".into(), ..h.clone() },
            Header { recipient_hpke_key: "x".into(), ..h.clone() },
        ] {
            assert_eq!(Chunk::seal(bad, &[(3, [0; 32])], &key(11)), Err(HistoryError::Header));
        }
        assert!(Header::parse(&wide.encode()).is_none());

        let bytes = Chunk::seal(h.clone(), &[(4, [40; 32])], &key(11)).unwrap();
        // A header changed after signing fails the signature, and opening (aad).
        let mut chunk = Chunk::parse(&bytes).unwrap();
        chunk.header.created_at += 1;
        assert!(!chunk.verify());
        assert_eq!(chunk.open(&private), Err(HistoryError::Open));
        // Any flipped ciphertext byte fails the signature.
        let mut chunk = Chunk::parse(&bytes).unwrap();
        chunk.ciphertext[0] ^= 1;
        assert!(!chunk.verify());
        let mut trailing = bytes.clone();
        trailing.push(b'0');
        assert!(Chunk::parse(&trailing).is_none());
    }

    #[test]
    fn opens_a_fixed_vector() {
        // Sealed once with the keys above; pins the wire format and the HPKE use.
        let (private, hpke) = hpke_key_pair(&[1; 32]);
        let chunk = Chunk::parse(&base64_decode(B64_VECTOR)).unwrap();
        assert_eq!(chunk.header, header(&hpke));
        assert!(chunk.verify());
        assert_eq!(chunk.open(&private).unwrap(), [(3, [30; 32]), (5, [50; 32])]);
    }

    fn base64_decode(value: &str) -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(value).unwrap()
    }

    const B64_VECTOR: &str = concat!(
        "MjU5Omd3YXIgaGlzdG9yeSBjaHVuayB2MQpsX19JZzhnTDduSTM3NVhadWJjRDFLMWo1Z29oNWdXR2RvSzNXNHNfUXdNCkJ3Y0hC",
        "d2NIQndjSEJ3Y0hCd2NIQndjSEJ3Y0hCd2NIQndjSEJ3Y0hCd2MKMVVJSDJobEpkOXowYXR2LXdyd3VkYlV0V29wQ0dFX3RfY0FB",
        "SlBEajZObwpRWVVqSVA4MmRKWDZVaXlVeTRPdk9SNU9pUUdEa25KYnZ5Q1kzWk1ieENRClpyNS1NeXg2UlRNeXZaMEtmMzJ3VmZY",
        "Rjd4b0dyYVp0bUxPZnRvRU1Sem8KMwo1CjE4MDAwMDAwMDAwMDAsMzI6M5-BItdTLDD97sMKwwfbvgu8IZ_bi0-H3WKa_Ot6pUYs",
        "OTY6QF50of3HvXF2Y3RtKZ38Usf2QBqbnnXBAlrWsanABGuyCVye2LAcwSNs7J2Xu2aH4CzFlfXJN6ALYJVjgQHZKl1Sn4OE1arQ",
        "-qERA-BacycwnAUkMP2eIQgEIQtWYi5MLDY0OpSQ4xeLjR6SkRdkwAgVjivCCF3dwr7NvmdpjSB_35H7synxKr6Uo5t1NQB95qof",
        "y2s_knJgfnE28xdI1lTMYAgs",
    );
}
