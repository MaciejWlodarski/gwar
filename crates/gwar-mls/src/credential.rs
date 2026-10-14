//! The MLS `BasicCredential` identity of a Gwar device (docs/e2e-build.md, "Credentials").

use vc_proto::{DeviceCertificate, Uid};

use crate::encoding::{decimal, fixed};

const HEADER: &str = "gwar credential v1";

/// A device, with the Connect certificate binding it to an account, or a
/// local identity whose device key is its own account.
#[derive(Debug, Clone, PartialEq)]
pub enum Credential {
    Local { device_key: String },
    Account(DeviceCertificate),
}

/// What the caller knows about revoked devices, from its Connect feed.
pub trait Revocations {
    /// When the account revoked this device, if it did.
    fn revoked_at(&self, account_key: &str, device_key: &str) -> Option<i64>;
}

impl Revocations for () {
    fn revoked_at(&self, _: &str, _: &str) -> Option<i64> {
        None
    }
}

impl Credential {
    pub fn device_key(&self) -> &str {
        match self {
            Credential::Local { device_key } => device_key,
            Credential::Account(c) => &c.device_key,
        }
    }

    pub fn uid(&self) -> Uid {
        let key = match self {
            Credential::Local { device_key } => device_key,
            Credential::Account(c) => &c.account_key,
        };
        vc_proto::uid_for_key(&fixed::<32>(key).expect("parsed credentials hold valid keys"))
    }

    pub fn encode(&self) -> String {
        match self {
            Credential::Local { device_key } => format!("{HEADER}\n-\n{device_key}\n-\n-\n-"),
            Credential::Account(c) => format!(
                "{HEADER}\n{}\n{}\n{}\n{}\n{}",
                c.account_key, c.device_key, c.issued_at, c.expires_at, c.signature
            ),
        }
    }

    /// Parses a well-formed credential: canonical encoding, valid keys and, for
    /// an account, a certificate signed by the account key lasting at most 90 days.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let text = std::str::from_utf8(bytes).ok()?;
        let fields: Vec<&str> = text.split('\n').collect();
        let [header, account_key, device_key, issued_at, expires_at, signature] = fields[..] else {
            return None;
        };
        if header != HEADER {
            return None;
        }
        fixed::<32>(device_key)?;
        let credential = if account_key == "-" {
            if [issued_at, expires_at, signature] != ["-"; 3] {
                return None;
            }
            Credential::Local { device_key: device_key.into() }
        } else {
            let c = DeviceCertificate {
                account_key: account_key.into(),
                device_key: device_key.into(),
                issued_at: decimal(issued_at)? as i64,
                expires_at: decimal(expires_at)? as i64,
                signature: signature.into(),
            };
            // Checks keys, signature encoding, timestamps and the 90-day limit.
            vc_proto::certificate_hash(Some(&c))?;
            if !vc_proto::signed_by(&c.account_key, &vc_proto::device_statement(&c), &c.signature) {
                return None;
            }
            Credential::Account(c)
        };
        (credential.encode().as_bytes() == bytes).then_some(credential)
    }

    /// Valid at `at`: the certificate covers it and the device was not revoked by then.
    pub fn is_current(&self, at: i64, revocations: &dyn Revocations) -> bool {
        match self {
            Credential::Local { .. } => true,
            Credential::Account(c) => {
                c.issued_at <= at
                    && at < c.expires_at
                    && revocations.revoked_at(&c.account_key, &c.device_key).is_none_or(|revoked| at < revoked)
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::*;
    use crate::encoding::b64;

    pub(crate) fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    pub(crate) fn public(key: &SigningKey) -> String {
        b64(key.verifying_key().as_bytes())
    }

    pub(crate) fn certified(account: &SigningKey, device: &SigningKey, issued_at: i64) -> Credential {
        let mut c = DeviceCertificate {
            account_key: public(account),
            device_key: public(device),
            issued_at,
            expires_at: issued_at + 30 * 86_400_000,
            signature: String::new(),
        };
        c.signature = b64(&account.sign(vc_proto::device_statement(&c).as_bytes()).to_bytes());
        Credential::Account(c)
    }

    struct Revoked(i64);
    impl Revocations for Revoked {
        fn revoked_at(&self, _: &str, _: &str) -> Option<i64> {
            Some(self.0)
        }
    }

    #[test]
    fn round_trips_and_derives_the_uid() {
        let (account, device) = (key(1), key(2));
        let local = Credential::Local { device_key: public(&device) };
        assert_eq!(Credential::parse(local.encode().as_bytes()), Some(local.clone()));
        assert_eq!(local.uid(), vc_proto::uid_for_key(device.verifying_key().as_bytes()));

        let account_credential = certified(&account, &device, 1_000);
        assert_eq!(Credential::parse(account_credential.encode().as_bytes()), Some(account_credential.clone()));
        assert_eq!(account_credential.uid(), vc_proto::uid_for_key(account.verifying_key().as_bytes()));
        assert_eq!(account_credential.device_key(), public(&device));
    }

    #[test]
    fn rejects_malformed_and_unsigned_credentials() {
        let (account, device) = (key(1), key(2));
        let good = certified(&account, &device, 1_000).encode();
        let mut forged = certified(&account, &device, 1_000);
        if let Credential::Account(c) = &mut forged {
            c.expires_at += 1;
        }
        let local = Credential::Local { device_key: public(&device) }.encode();
        for bad in [
            format!("{good}\n"),
            good.replace("gwar credential v1", "gwar credential v2"),
            good.replacen("\n1000\n", "\n01000\n", 1),
            forged.encode(),
            local.replacen("\n-\n-\n-", "\n1\n-\n-", 1),
            local.replace(&public(&device), "AAAA"),
        ] {
            assert_eq!(Credential::parse(bad.as_bytes()), None, "{bad}");
        }
        let mut long = certified(&account, &device, 1_000);
        if let Credential::Account(c) = &mut long {
            c.expires_at = c.issued_at + 91 * 86_400_000;
            c.signature = b64(&account.sign(vc_proto::device_statement(c).as_bytes()).to_bytes());
        }
        assert_eq!(Credential::parse(long.encode().as_bytes()), None);
    }

    #[test]
    fn currency_follows_the_certificate_and_revocations() {
        let c = certified(&key(1), &key(2), 1_000);
        let Credential::Account(cert) = &c else { unreachable!() };
        assert!(!c.is_current(999, &()));
        assert!(c.is_current(1_000, &()));
        assert!(!c.is_current(cert.expires_at, &()));
        assert!(c.is_current(4_999, &Revoked(5_000)));
        assert!(!c.is_current(5_000, &Revoked(5_000)));
        assert!(Credential::Local { device_key: public(&key(3)) }.is_current(i64::MAX, &Revoked(0)));
    }
}
