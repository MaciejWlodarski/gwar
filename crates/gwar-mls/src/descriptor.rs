//! The immutable conversation descriptor in every group's GroupContext
//! (docs/e2e.md "The descriptor", docs/e2e-build.md "Descriptor").

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use vc_proto::Uid;

use crate::credential::Credential;
use crate::encoding::{b64, decimal, fixed, netstrings, parse_netstrings};

const HEADER: &str = "gwar conversation v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Dm,
    Channel,
    Server,
    Voice,
}

impl Kind {
    pub(crate) fn separate_tree(self) -> bool {
        matches!(self, Self::Channel | Self::Server | Self::Voice)
    }

    fn as_str(self) -> &'static str {
        match self {
            Kind::Dm => "dm",
            Kind::Channel => "channel",
            Kind::Server => "server",
            Kind::Voice => "voice",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "dm" => Kind::Dm,
            "channel" => Kind::Channel,
            "server" => Kind::Server,
            "voice" => Kind::Voice,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Descriptor {
    pub server_key: String,
    pub conversation: String,
    pub kind: Kind,
    pub incarnation: u64,
    pub previous_group_id: Option<String>,
    pub private: bool,
    pub history: bool,
    pub founding_owner: Option<Uid>,
    pub channel_group_id: Option<String>,
    pub creator_uid: Uid,
    pub created_at: i64,
    pub creation_nonce: String,
}

fn dash(value: &Option<String>) -> &str {
    value.as_deref().unwrap_or("-")
}

fn optional(value: &str) -> Option<String> {
    (value != "-").then(|| value.to_owned())
}

/// The two uids of `dm:{a}:{b}`, distinct and sorted.
pub fn dm_participants(conversation: &str) -> Option<[&str; 2]> {
    let mut parts = conversation.split(':');
    let (Some("dm"), Some(a), Some(b), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    (a < b && fixed::<20>(a).is_some() && fixed::<20>(b).is_some()).then_some([a, b])
}

fn conversation_kind(conversation: &str) -> Option<Kind> {
    let id = |rest: &str| decimal(rest).is_some_and(|n| n <= u32::MAX as u64);
    if conversation == "server" {
        Some(Kind::Server)
    } else if dm_participants(conversation).is_some() {
        Some(Kind::Dm)
    } else if conversation.strip_prefix("channel:").is_some_and(id) {
        Some(Kind::Channel)
    } else if conversation.strip_prefix("voice:").is_some_and(id) {
        Some(Kind::Voice)
    } else {
        None
    }
}

impl Descriptor {
    pub fn statement(&self) -> String {
        format!(
            "{HEADER}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.server_key,
            self.conversation,
            self.kind.as_str(),
            self.incarnation,
            dash(&self.previous_group_id),
            u8::from(self.private),
            u8::from(self.history),
            dash(&self.founding_owner),
            dash(&self.channel_group_id),
            self.creator_uid,
            self.created_at,
            self.creation_nonce,
        )
    }

    /// `SHA-256(statement)`, the MLS group id.
    pub fn group_id(&self) -> [u8; 32] {
        Sha256::digest(self.statement().as_bytes()).into()
    }

    /// Parses a statement and checks every field and the rules between them.
    pub fn parse(statement: &str) -> Option<Self> {
        let fields: Vec<&str> = statement.split('\n').collect();
        let [
            header,
            server_key,
            conversation,
            kind,
            incarnation,
            previous,
            private,
            history,
            founding_owner,
            channel_group,
            creator_uid,
            created_at,
            nonce,
        ] = fields[..]
        else {
            return None;
        };
        let flag = |value: &str| match value {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let d = Descriptor {
            server_key: server_key.into(),
            conversation: conversation.into(),
            kind: Kind::parse(kind)?,
            incarnation: decimal(incarnation)?,
            previous_group_id: optional(previous),
            private: flag(private)?,
            history: flag(history)?,
            founding_owner: optional(founding_owner),
            channel_group_id: optional(channel_group),
            creator_uid: creator_uid.into(),
            created_at: decimal(created_at)? as i64,
            creation_nonce: nonce.into(),
        };
        let valid = header == HEADER
            && fixed::<32>(&d.server_key).is_some()
            && conversation_kind(&d.conversation) == Some(d.kind)
            && (d.incarnation == 0) == d.previous_group_id.is_none()
            && d.previous_group_id.as_deref().is_none_or(|id| fixed::<32>(id).is_some())
            && (d.kind == Kind::Voice) == d.channel_group_id.is_some()
            && d.channel_group_id.as_deref().is_none_or(|id| fixed::<32>(id).is_some())
            && (d.kind == Kind::Channel && d.private) == d.founding_owner.is_some()
            && d.founding_owner.as_deref().is_none_or(|uid| fixed::<20>(uid).is_some())
            && (!d.private || d.kind == Kind::Channel)
            && (d.kind != Kind::Dm || d.history)
            && (d.kind != Kind::Voice || (!d.history && d.incarnation == 0))
            && fixed::<20>(&d.creator_uid).is_some()
            && dm_participants(&d.conversation).is_none_or(|uids| uids.contains(&d.creator_uid.as_str()))
            && fixed::<16>(&d.creation_nonce).is_some();
        (valid && d.statement() == statement).then_some(d)
    }
}

/// The descriptor with its creator's credential and device signature: the
/// bytes of GroupContext extension `0xff00`.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedDescriptor {
    pub descriptor: Descriptor,
    pub creator: Credential,
    pub signature: String,
}

impl SignedDescriptor {
    /// Signs with the creator's device key, which must be the credential's.
    pub fn sign(descriptor: Descriptor, creator: Credential, device: &SigningKey) -> Self {
        assert_eq!(creator.device_key(), b64(device.verifying_key().as_bytes()));
        assert_eq!(creator.uid(), descriptor.creator_uid);
        let signature = b64(&device.sign(descriptor.statement().as_bytes()).to_bytes());
        Self { descriptor, creator, signature }
    }

    pub fn encode(&self) -> Vec<u8> {
        netstrings(&[
            self.descriptor.statement().as_bytes(),
            self.creator.encode().as_bytes(),
            self.signature.as_bytes(),
        ])
    }

    /// Parses and verifies: a valid descriptor, a well-formed creator credential
    /// of `creator_uid`, and the creator device's signature.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let fields = parse_netstrings(bytes, 3)?;
        let statement = std::str::from_utf8(fields[0]).ok()?;
        let descriptor = Descriptor::parse(statement)?;
        let creator = Credential::parse(fields[1])?;
        let signature = std::str::from_utf8(fields[2]).ok()?.to_owned();
        (creator.uid() == descriptor.creator_uid && vc_proto::signed_by(creator.device_key(), statement, &signature))
            .then_some(Self { descriptor, creator, signature })
    }

    pub fn group_id(&self) -> [u8; 32] {
        self.descriptor.group_id()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::credential::tests::{certified, key, public};

    fn uid(seed: u8) -> Uid {
        vc_proto::uid_for_key(key(seed).verifying_key().as_bytes())
    }

    fn dm() -> (Descriptor, Credential) {
        let creator = certified(&key(1), &key(2), 1_000);
        let (a, b) = (uid(1), uid(9));
        let conversation = if a < b { format!("dm:{a}:{b}") } else { format!("dm:{b}:{a}") };
        let d = Descriptor {
            server_key: public(&key(7)),
            conversation,
            kind: Kind::Dm,
            incarnation: 0,
            previous_group_id: None,
            private: false,
            history: true,
            founding_owner: None,
            channel_group_id: None,
            creator_uid: uid(1),
            created_at: 2_000,
            creation_nonce: b64(&[5; 16]),
        };
        (d, creator)
    }

    #[test]
    fn round_trips_and_hashes_the_statement() {
        let (d, creator) = dm();
        assert_eq!(Descriptor::parse(&d.statement()), Some(d.clone()));
        let signed = SignedDescriptor::sign(d.clone(), creator, &key(2));
        assert_eq!(SignedDescriptor::parse(&signed.encode()), Some(signed.clone()));
        assert_eq!(signed.group_id(), <[u8; 32]>::from(Sha256::digest(d.statement().as_bytes())));
    }

    #[test]
    fn rejects_inconsistent_descriptors() {
        let (d, _) = dm();
        let reject = |change: &dyn Fn(&mut Descriptor)| {
            let mut bad = d.clone();
            change(&mut bad);
            assert_eq!(Descriptor::parse(&bad.statement()), None, "{}", bad.statement());
        };
        reject(&|d| d.kind = Kind::Channel);
        reject(&|d| d.history = false);
        reject(&|d| d.private = true);
        reject(&|d| d.incarnation = 1);
        reject(&|d| d.previous_group_id = Some(b64(&[1; 32])));
        reject(&|d| d.channel_group_id = Some(b64(&[1; 32])));
        reject(&|d| d.founding_owner = Some(uid(1)));
        reject(&|d| d.creator_uid = uid(3));
        reject(&|d| d.creation_nonce = b64(&[1; 15]));
        reject(&|d| {
            let [a, b] = dm_participants(&d.conversation).map(|p| p.map(str::to_owned)).unwrap();
            d.conversation = format!("dm:{b}:{a}");
        });
        reject(&|d| {
            d.kind = Kind::Channel;
            d.conversation = "channel:01".into();
        });
        let mut channel = d.clone();
        channel.kind = Kind::Channel;
        channel.conversation = "channel:4".into();
        channel.history = false;
        assert!(Descriptor::parse(&channel.statement()).is_some());
        channel.private = true;
        assert!(Descriptor::parse(&channel.statement()).is_none(), "a private channel names its founding owner");
        channel.founding_owner = Some(uid(1));
        assert!(Descriptor::parse(&channel.statement()).is_some());
        let mut voice = d.clone();
        voice.kind = Kind::Voice;
        voice.conversation = "voice:4".into();
        assert!(Descriptor::parse(&voice.statement()).is_none(), "a voice call names its channel group");
        voice.channel_group_id = Some(b64(&[2; 32]));
        voice.history = false;
        assert!(Descriptor::parse(&voice.statement()).is_some());
    }

    #[test]
    fn rejects_a_wrong_signer_or_tampered_signature() {
        let (d, creator) = dm();
        let signed = SignedDescriptor::sign(d.clone(), creator, &key(2));
        let mut tampered = signed.clone();
        tampered.descriptor.created_at += 1;
        assert_eq!(SignedDescriptor::parse(&tampered.encode()), None);
        // A credential of another account cannot vouch for this creator uid.
        let other = SignedDescriptor { creator: certified(&key(3), &key(2), 1_000), ..signed.clone() };
        assert_eq!(SignedDescriptor::parse(&other.encode()), None);
        let mut bytes = signed.encode();
        bytes.push(b'x');
        assert_eq!(SignedDescriptor::parse(&bytes), None);
    }
}
