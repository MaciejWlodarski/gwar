//! Ed25519 keys as OpenMLS signers, and reading Gwar credentials out of leaves.

use ed25519_dalek::{Signer as _, SigningKey};
use openmls::prelude::{BasicCredential, CredentialType, LeafNode};
use openmls_traits::{
    signatures::{Signer, SignerError},
    types::SignatureScheme,
};

use crate::{credential::Credential, encoding::fixed};

/// A device key (signing as the device's leaf) or the server key (signing
/// external proposals). MLS signs with its own labels, which no Gwar
/// statement starts with.
pub struct Ed25519Signer(pub SigningKey);

impl Signer for Ed25519Signer {
    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>, SignerError> {
        Ok(self.0.sign(payload).to_bytes().to_vec())
    }

    fn signature_scheme(&self) -> SignatureScheme {
        SignatureScheme::ED25519
    }
}

/// The well-formed Gwar credential of a leaf whose signature key is its device key.
pub fn leaf_credential(leaf: &LeafNode) -> Option<Credential> {
    if leaf.credential().credential_type() != CredentialType::Basic {
        return None;
    }
    let basic = BasicCredential::try_from(leaf.credential().clone()).ok()?;
    let credential = Credential::parse(basic.identity())?;
    (fixed::<32>(credential.device_key())?.as_slice() == leaf.signature_key().as_slice()).then_some(credential)
}

/// The external sender credential: `gwar server v1\n{server_key}`.
pub fn server_credential(server_key: &str) -> Vec<u8> {
    format!("gwar server v1\n{server_key}").into_bytes()
}
