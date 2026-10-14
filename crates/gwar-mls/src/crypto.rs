//! Restrict the provider to Gwar's suite before entering RustCrypto. Fixed
//! algorithms also let the wasm linker remove the unused signature suites.

use crate::SUITE;
use crate::mls_storage::Storage;
use ed25519_dalek::Signer as _;
use hmac::Mac;
use openmls::prelude::tls_codec::SecretVLBytes;
use openmls_rust_crypto::RustCrypto;
use openmls_traits::random::OpenMlsRand;
use openmls_traits::{OpenMlsProvider, crypto::OpenMlsCrypto, types::*};
use sha2::{Digest, Sha256};

#[derive(Default)]
pub(crate) struct Provider {
    crypto: Crypto,
    random: RustCrypto,
    storage: Storage,
}
impl OpenMlsProvider for Provider {
    type CryptoProvider = Crypto;
    type RandProvider = RustCrypto;
    type StorageProvider = Storage;
    fn crypto(&self) -> &Crypto {
        &self.crypto
    }
    fn rand(&self) -> &RustCrypto {
        &self.random
    }
    fn storage(&self) -> &Storage {
        &self.storage
    }
}
#[derive(Default)]
pub(crate) struct Crypto(RustCrypto);
const HPKE: HpkeConfig = HpkeConfig(HpkeKemType::DhKem25519, HpkeKdfType::HkdfSha256, HpkeAeadType::AesGcm128);
impl OpenMlsCrypto for Crypto {
    fn supports(&self, ciphersuite: Ciphersuite) -> Result<(), CryptoError> {
        if ciphersuite == SUITE { Ok(()) } else { Err(CryptoError::UnsupportedCiphersuite) }
    }
    fn supported_ciphersuites(&self) -> Vec<Ciphersuite> {
        vec![SUITE]
    }
    fn hkdf_extract(&self, hash_type: HashType, salt: &[u8], ikm: &[u8]) -> Result<SecretVLBytes, CryptoError> {
        if hash_type != HashType::Sha2_256 {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        Ok(hkdf::Hkdf::<Sha256>::extract(Some(salt), ikm).0.to_vec().into())
    }
    fn hmac(&self, hash_type: HashType, key: &[u8], message: &[u8]) -> Result<SecretVLBytes, CryptoError> {
        if hash_type != HashType::Sha2_256 {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        let mut mac = hmac::Hmac::<Sha256>::new_from_slice(key).map_err(|_| CryptoError::InvalidLength)?;
        mac.update(message);
        Ok(mac.finalize().into_bytes().to_vec().into())
    }
    fn hkdf_expand(
        &self,
        hash_type: HashType,
        prk: &[u8],
        info: &[u8],
        okm_len: usize,
    ) -> Result<SecretVLBytes, CryptoError> {
        if hash_type != HashType::Sha2_256 {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        let hkdf = hkdf::Hkdf::<Sha256>::from_prk(prk).map_err(|_| CryptoError::InvalidLength)?;
        let mut output = vec![0; okm_len];
        hkdf.expand(info, &mut output).map_err(|_| CryptoError::InvalidLength)?;
        Ok(output.into())
    }
    fn hash(&self, hash_type: HashType, data: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if hash_type != HashType::Sha2_256 {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        Ok(Sha256::digest(data).to_vec())
    }
    fn aead_encrypt(
        &self,
        alg: AeadType,
        key: &[u8],
        data: &[u8],
        nonce: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        if alg != AeadType::Aes128Gcm {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        self.0.aead_encrypt(AeadType::Aes128Gcm, key, data, nonce, aad)
    }
    fn aead_decrypt(
        &self,
        alg: AeadType,
        key: &[u8],
        ct_tag: &[u8],
        nonce: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        if alg != AeadType::Aes128Gcm {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        self.0.aead_decrypt(AeadType::Aes128Gcm, key, ct_tag, nonce, aad)
    }
    fn signature_key_gen(&self, alg: SignatureScheme) -> Result<(Vec<u8>, Vec<u8>), CryptoError> {
        if alg != SignatureScheme::ED25519 {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        let seed = self.0.random_array().map_err(|_| CryptoError::InsufficientRandomness)?;
        let key = ed25519_dalek::SigningKey::from_bytes(&seed);
        Ok((key.to_bytes().to_vec(), key.verifying_key().to_bytes().to_vec()))
    }
    fn verify_signature(
        &self,
        alg: SignatureScheme,
        data: &[u8],
        pk: &[u8],
        signature: &[u8],
    ) -> Result<(), CryptoError> {
        if alg != SignatureScheme::ED25519 {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        let key = ed25519_dalek::VerifyingKey::try_from(pk).map_err(|_| CryptoError::CryptoLibraryError)?;
        let signature = ed25519_dalek::Signature::from_slice(signature).map_err(|_| CryptoError::CryptoLibraryError)?;
        key.verify_strict(data, &signature).map_err(|_| CryptoError::InvalidSignature)
    }
    fn sign(&self, alg: SignatureScheme, data: &[u8], key: &[u8]) -> Result<Vec<u8>, CryptoError> {
        if alg != SignatureScheme::ED25519 {
            return Err(CryptoError::UnsupportedCiphersuite);
        }
        let key = ed25519_dalek::SigningKey::try_from(key).map_err(|_| CryptoError::CryptoLibraryError)?;
        Ok(key.sign(data).to_bytes().to_vec())
    }
    fn hpke_seal(
        &self,
        config: HpkeConfig,
        pk_r: &[u8],
        info: &[u8],
        aad: &[u8],
        ptxt: &[u8],
    ) -> Result<HpkeCiphertext, CryptoError> {
        let (kem_output, ciphertext) =
            hpke(config)?.seal(&pk_r.into(), info, aad, ptxt, None, None, None).map_err(|e| match e {
                hpke::HpkeError::InvalidInput => CryptoError::InvalidLength,
                _ => CryptoError::CryptoLibraryError,
            })?;
        Ok(HpkeCiphertext { kem_output: kem_output.into(), ciphertext: ciphertext.into() })
    }

    fn hpke_open(
        &self,
        config: HpkeConfig,
        input: &HpkeCiphertext,
        sk_r: &[u8],
        info: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, CryptoError> {
        hpke(config)?
            .open(input.kem_output.as_slice(), &sk_r.into(), info, aad, input.ciphertext.as_slice(), None, None, None)
            .map_err(|_| CryptoError::HpkeDecryptionError)
    }

    fn hpke_setup_sender_and_export(
        &self,
        config: HpkeConfig,
        pk_r: &[u8],
        info: &[u8],
        exporter_context: &[u8],
        exporter_length: usize,
    ) -> Result<(Vec<u8>, ExporterSecret), CryptoError> {
        let (kem_output, context) = hpke(config)?
            .setup_sender(&pk_r.into(), info, None, None, None)
            .map_err(|_| CryptoError::SenderSetupError)?;
        let exported_secret =
            context.export(exporter_context, exporter_length).map_err(|_| CryptoError::ExporterError)?;
        Ok((kem_output, exported_secret.into()))
    }

    fn hpke_setup_receiver_and_export(
        &self,
        config: HpkeConfig,
        enc: &[u8],
        sk_r: &[u8],
        info: &[u8],
        exporter_context: &[u8],
        exporter_length: usize,
    ) -> Result<ExporterSecret, CryptoError> {
        let context = hpke(config)?
            .setup_receiver(enc, &sk_r.into(), info, None, None, None)
            .map_err(|_| CryptoError::ReceiverSetupError)?;
        let exported_secret =
            context.export(exporter_context, exporter_length).map_err(|_| CryptoError::ExporterError)?;
        Ok(exported_secret.into())
    }

    fn derive_hpke_keypair(&self, config: HpkeConfig, ikm: &[u8]) -> Result<HpkeKeyPair, CryptoError> {
        let kp = hpke(config)?
            .derive_key_pair(ikm)
            .map_err(|e| match e {
                hpke::HpkeError::InvalidInput => CryptoError::InvalidLength,
                _ => CryptoError::CryptoLibraryError,
            })?
            .into_keys();
        Ok(HpkeKeyPair { private: kp.0.as_slice().into(), public: kp.1.as_slice().into() })
    }
}

fn hpke(config: HpkeConfig) -> Result<hpke::Hpke<crate::hpke::Backend>, CryptoError> {
    if config.0 != HPKE.0 || config.1 != HPKE.1 || config.2 != HPKE.2 {
        return Err(CryptoError::UnsupportedCiphersuite);
    }
    use hpke_rs_crypto::types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm};
    Ok(hpke::Hpke::new(hpke::Mode::Base, KemAlgorithm::DhKem25519, KdfAlgorithm::HkdfSha256, AeadAlgorithm::Aes128Gcm))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn single_suite_provider_matches_rustcrypto() {
        let ours = Crypto::default();
        let upstream = RustCrypto::default();
        let hash = HashType::Sha2_256;
        assert_eq!(ours.hash(hash, b"message").unwrap(), upstream.hash(hash, b"message").unwrap());
        assert_eq!(ours.hmac(hash, b"key", b"message").unwrap(), upstream.hmac(hash, b"key", b"message").unwrap());
        let prk = ours.hkdf_extract(hash, b"salt", b"input").unwrap();
        assert_eq!(prk, upstream.hkdf_extract(hash, b"salt", b"input").unwrap());
        assert_eq!(
            ours.hkdf_expand(hash, prk.as_slice(), b"info", 64).unwrap(),
            upstream.hkdf_expand(hash, prk.as_slice(), b"info", 64).unwrap()
        );
        let (sk, pk) = ours.signature_key_gen(SignatureScheme::ED25519).unwrap();
        let sig = ours.sign(SignatureScheme::ED25519, b"message", &sk).unwrap();
        upstream.verify_signature(SignatureScheme::ED25519, b"message", &pk, &sig).unwrap();
        ours.verify_signature(SignatureScheme::ED25519, b"message", &pk, &sig).unwrap();
        assert!(ours.verify_signature(SignatureScheme::ED25519, b"tampered", &pk, &sig).is_err());
        let pair = ours.derive_hpke_keypair(HPKE, &[7; 32]).unwrap();
        let other = upstream.derive_hpke_keypair(HPKE, &[7; 32]).unwrap();
        assert_eq!(pair.public, other.public);
        assert_eq!(pair.private.as_ref(), other.private.as_ref());
        let sealed = ours.hpke_seal(HPKE, &pair.public, b"info", b"aad", b"message").unwrap();
        assert_eq!(upstream.hpke_open(HPKE, &sealed, pair.private.as_ref(), b"info", b"aad").unwrap(), b"message");
        let sealed = upstream.hpke_seal(HPKE, &pair.public, b"info", b"aad", b"message").unwrap();
        assert_eq!(ours.hpke_open(HPKE, &sealed, pair.private.as_ref(), b"info", b"aad").unwrap(), b"message");
        assert!(ours.hpke_open(HPKE, &sealed, pair.private.as_ref(), b"info", b"tampered").is_err());
        assert!(ours.hpke_seal(HPKE, &[0; 32], b"info", b"aad", b"message").is_err());
        assert!(ours.hash(HashType::Sha2_512, b"message").is_err());
        assert!(ours.supports(Ciphersuite::MLS_128_DHKEMP256_AES128GCM_SHA256_P256).is_err());
    }
}
