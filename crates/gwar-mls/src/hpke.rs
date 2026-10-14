//! HPKE uses the same RustCrypto primitives, restricted to Gwar's suite.
use hpke_rs_crypto::Rng;
use hpke_rs_crypto::{
    HpkeCrypto,
    error::Error,
    types::{AeadAlgorithm, KdfAlgorithm, KemAlgorithm},
};
use hpke_rs_rust_crypto::HpkeRustCrypto;
use x25519_dalek::{PublicKey, StaticSecret};
#[derive(Debug)]
pub(super) struct Backend;
impl HpkeCrypto for Backend {
    type HpkePrng = <HpkeRustCrypto as HpkeCrypto>::HpkePrng;
    fn name() -> String {
        "Gwar RustCrypto".into()
    }
    fn supports_kdf(alg: KdfAlgorithm) -> Result<(), Error> {
        if alg != KdfAlgorithm::HkdfSha256 {
            return Err(Error::UnknownKdfAlgorithm);
        }
        Ok(())
    }
    fn supports_kem(alg: KemAlgorithm) -> Result<(), Error> {
        if alg != KemAlgorithm::DhKem25519 {
            return Err(Error::UnknownKemAlgorithm);
        }
        Ok(())
    }
    fn supports_aead(alg: AeadAlgorithm) -> Result<(), Error> {
        if alg != AeadAlgorithm::Aes128Gcm {
            return Err(Error::UnknownAeadAlgorithm);
        }
        Ok(())
    }
    fn prng() -> Self::HpkePrng {
        HpkeRustCrypto::prng()
    }
    fn kdf_extract(alg: KdfAlgorithm, salt: &[u8], ikm: &[u8]) -> Result<Vec<u8>, Error> {
        if alg != KdfAlgorithm::HkdfSha256 {
            return Err(Error::UnknownKdfAlgorithm);
        }
        Ok(hkdf::Hkdf::<sha2::Sha256>::extract(Some(salt), ikm).0.to_vec())
    }
    fn kdf_expand(alg: KdfAlgorithm, prk: &[u8], info: &[u8], output_size: usize) -> Result<Vec<u8>, Error> {
        if alg != KdfAlgorithm::HkdfSha256 {
            return Err(Error::UnknownKdfAlgorithm);
        }
        let hkdf = hkdf::Hkdf::<sha2::Sha256>::from_prk(prk).map_err(|_| Error::HpkeInvalidOutputLength)?;
        let mut output = vec![0; output_size];
        hkdf.expand(info, &mut output).map_err(|_| Error::HpkeInvalidOutputLength)?;
        Ok(output)
    }
    fn dh(alg: KemAlgorithm, pk: &[u8], sk: &[u8]) -> Result<Vec<u8>, Error> {
        if alg != KemAlgorithm::DhKem25519 {
            return Err(Error::UnknownKemAlgorithm);
        }
        let sk = StaticSecret::from(<[u8; 32]>::try_from(sk).map_err(|_| Error::KemInvalidSecretKey)?);
        let pk = PublicKey::from(<[u8; 32]>::try_from(pk).map_err(|_| Error::KemInvalidPublicKey)?);
        let shared = sk.diffie_hellman(&pk);
        if !shared.was_contributory() {
            return Err(Error::KemInvalidPublicKey);
        }
        Ok(shared.as_bytes().to_vec())
    }
    fn secret_to_public(alg: KemAlgorithm, sk: &[u8]) -> Result<Vec<u8>, Error> {
        if alg != KemAlgorithm::DhKem25519 {
            return Err(Error::UnknownKemAlgorithm);
        }
        let sk = StaticSecret::from(<[u8; 32]>::try_from(sk).map_err(|_| Error::KemInvalidSecretKey)?);
        Ok(PublicKey::from(&sk).as_bytes().to_vec())
    }
    fn kem_key_gen(alg: KemAlgorithm, prng: &mut Self::HpkePrng) -> Result<(Vec<u8>, Vec<u8>), Error> {
        if alg != KemAlgorithm::DhKem25519 {
            return Err(Error::UnknownKemAlgorithm);
        }
        let mut bytes = [0; 32];
        prng.fill_bytes(&mut bytes);
        let sk = StaticSecret::from(bytes);
        Ok((PublicKey::from(&sk).as_bytes().to_vec(), sk.to_bytes().to_vec()))
    }
    fn kem_key_gen_derand(alg: KemAlgorithm, seed: &[u8]) -> Result<(Vec<u8>, Vec<u8>), Error> {
        if alg != KemAlgorithm::DhKem25519 {
            return Err(Error::UnknownKemAlgorithm);
        }
        let _ = seed;
        Err(Error::UnsupportedKemOperation)
    }
    fn kem_encaps(alg: KemAlgorithm, pk_r: &[u8], prng: &mut Self::HpkePrng) -> Result<(Vec<u8>, Vec<u8>), Error> {
        if alg != KemAlgorithm::DhKem25519 {
            return Err(Error::UnknownKemAlgorithm);
        }
        let _ = (pk_r, prng);
        Err(Error::UnsupportedKemOperation)
    }
    fn kem_decaps(alg: KemAlgorithm, ct: &[u8], sk_r: &[u8]) -> Result<Vec<u8>, Error> {
        if alg != KemAlgorithm::DhKem25519 {
            return Err(Error::UnknownKemAlgorithm);
        }
        let _ = (ct, sk_r);
        Err(Error::UnsupportedKemOperation)
    }
    fn dh_validate_sk(alg: KemAlgorithm, sk: &[u8]) -> Result<Vec<u8>, Error> {
        if alg != KemAlgorithm::DhKem25519 {
            return Err(Error::UnknownKemAlgorithm);
        }
        let _ = sk;
        Err(Error::UnknownKemAlgorithm)
    }
    fn aead_seal(alg: AeadAlgorithm, key: &[u8], nonce: &[u8], aad: &[u8], msg: &[u8]) -> Result<Vec<u8>, Error> {
        if alg != AeadAlgorithm::Aes128Gcm {
            return Err(Error::UnknownAeadAlgorithm);
        }
        HpkeRustCrypto::aead_seal(AeadAlgorithm::Aes128Gcm, key, nonce, aad, msg)
    }
    fn aead_open(alg: AeadAlgorithm, key: &[u8], nonce: &[u8], aad: &[u8], msg: &[u8]) -> Result<Vec<u8>, Error> {
        if alg != AeadAlgorithm::Aes128Gcm {
            return Err(Error::UnknownAeadAlgorithm);
        }
        HpkeRustCrypto::aead_open(AeadAlgorithm::Aes128Gcm, key, nonce, aad, msg)
    }
}
