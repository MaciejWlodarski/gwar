//! Gwar's end-to-end encryption rules on top of OpenMLS: credentials, the
//! conversation descriptor and what the server and clients check
//! (docs/e2e.md, docs/e2e-build.md).

pub mod credential;
pub mod descriptor;
mod encoding;

pub use credential::{Credential, Revocations};
pub use descriptor::{Descriptor, Kind, SignedDescriptor};

/// The only ciphersuite Gwar uses.
pub const SUITE: openmls::prelude::Ciphersuite =
    openmls::prelude::Ciphersuite::MLS_128_DHKEMX25519_AES128GCM_SHA256_Ed25519;

/// GroupContext extension holding the [`SignedDescriptor`] (private-use range).
pub const DESCRIPTOR_EXTENSION: u16 = 0xff00;

pub mod ds;
pub mod signer;
