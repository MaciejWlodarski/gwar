//! Who an encrypted private channel admits (docs/e2e-build.md, section 6, "Encrypted private
//! channels (6c)"): the GroupContext extension `0xff01` carries the confirmer anchor, the manager
//! list and the channel's signed member list, bound to the group.

use ed25519_dalek::{Signer, SigningKey};
use vc_proto::Uid;

use crate::credential::{Credential, Revocations};
use crate::encoding::{b64, decimal, fixed, netstrings, parse_netstrings};
use crate::owner::{OwnerChain, SignedAnchor, SignedList, statement_hash};

const MEMBERS: &str = "gwar channel members v1";

/// The GroupContext extension of a private channel's group (private-use range).
pub const ACCESS_EXTENSION: u16 = 0xff01;
pub const MAX_MEMBERS: usize = 1000;
/// The whole extension, which leaves room under the 128 KiB commit and GroupInfo limits.
pub const MAX_ACCESS_BYTES: usize = 48 * 1024;

fn uid_list(uids: &[Uid]) -> String {
    if uids.is_empty() { "-".into() } else { uids.join(",") }
}

fn parse_uids(value: &str) -> Option<Vec<Uid>> {
    if value == "-" {
        return Some(Vec::new());
    }
    let uids: Vec<Uid> = value.split(',').map(str::to_owned).collect();
    (uids.iter().all(|u| fixed::<20>(u).is_some()) && uids.windows(2).all(|w| w[0] < w[1])).then_some(uids)
}

/// The members of one private channel's group, signed by a device of the owner, a confirmer or
/// a manager.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelMembers {
    pub server_key: String,
    pub generation: u64,
    /// The group (base64url of its 32-byte id): a list is never valid for another group.
    pub group_id: String,
    /// 1 first in each group.
    pub seq: u64,
    pub members: Vec<Uid>,
    pub signer_uid: Uid,
    pub signed_at: i64,
}

impl ChannelMembers {
    pub fn statement(&self) -> String {
        format!(
            "{MEMBERS}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.server_key,
            self.generation,
            self.group_id,
            self.seq,
            uid_list(&self.members),
            self.signer_uid,
            self.signed_at,
        )
    }

    pub fn parse(statement: &str) -> Option<Self> {
        let [header, server_key, generation, group_id, seq, members, signer_uid, signed_at] =
            statement.split('\n').collect::<Vec<_>>()[..]
        else {
            return None;
        };
        let m = ChannelMembers {
            server_key: server_key.into(),
            generation: decimal(generation)?,
            group_id: group_id.into(),
            seq: decimal(seq)?,
            members: parse_uids(members)?,
            signer_uid: signer_uid.into(),
            signed_at: decimal(signed_at)? as i64,
        };
        let valid = header == MEMBERS
            && fixed::<32>(&m.server_key).is_some()
            && m.generation >= 1
            && fixed::<32>(&m.group_id).is_some()
            && m.seq >= 1
            && m.members.len() <= MAX_MEMBERS
            && fixed::<20>(&m.signer_uid).is_some();
        (valid && m.statement() == statement).then_some(m)
    }
}

/// A member list with its signer's credential and device signature. Built only by
/// [`SignedMembers::sign`] and [`SignedMembers::parse`].
#[derive(Debug, Clone, PartialEq)]
pub struct SignedMembers {
    members: ChannelMembers,
    signer: Credential,
    signature: String,
}

impl SignedMembers {
    pub fn members(&self) -> &ChannelMembers {
        &self.members
    }

    pub fn signer(&self) -> &Credential {
        &self.signer
    }

    /// Signs with the signer's device key; the credential must verify and be the signer's.
    pub fn sign(members: ChannelMembers, signer: Credential, device: &SigningKey) -> Self {
        assert_eq!(Credential::parse(signer.encode().as_bytes()).as_ref(), Some(&signer), "a verified credential");
        assert_eq!(signer.device_key(), b64(device.verifying_key().as_bytes()));
        assert_eq!(signer.uid(), members.signer_uid);
        let signature = b64(&device.sign(members.statement().as_bytes()).to_bytes());
        Self { members, signer, signature }
    }

    pub fn encode(&self) -> Vec<u8> {
        netstrings(&[self.members.statement().as_bytes(), self.signer.encode().as_bytes(), self.signature.as_bytes()])
    }

    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let fields = parse_netstrings(bytes, 3)?;
        let statement = std::str::from_utf8(fields[0]).ok()?;
        let members = ChannelMembers::parse(statement)?;
        let signer = Credential::parse(fields[1])?;
        let signature = std::str::from_utf8(fields[2]).ok()?.to_owned();
        (signer.uid() == members.signer_uid && vc_proto::signed_by(signer.device_key(), statement, &signature))
            .then_some(Self { members, signer, signature })
    }

    pub fn hash(&self) -> String {
        statement_hash(&self.members.statement())
    }
}

/// The `0xff01` extension: anchor, manager list and member list, each verified in itself.
#[derive(Debug, Clone, PartialEq)]
pub struct Access {
    anchor: SignedAnchor,
    list: SignedList,
    members: SignedMembers,
}

impl Access {
    pub fn new(anchor: SignedAnchor, list: SignedList, members: SignedMembers) -> Self {
        Self { anchor, list, members }
    }

    pub fn anchor(&self) -> &SignedAnchor {
        &self.anchor
    }

    pub fn list(&self) -> &SignedList {
        &self.list
    }

    pub fn members(&self) -> &SignedMembers {
        &self.members
    }

    pub fn encode(&self) -> Vec<u8> {
        netstrings(&[&self.anchor.encode(), &self.list.encode(), &self.members.encode()])
    }

    /// Three verified envelopes within the size bound.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() > MAX_ACCESS_BYTES {
            return None;
        }
        let fields = parse_netstrings(bytes, 3)?;
        Some(Self {
            anchor: SignedAnchor::parse(fields[0])?,
            list: SignedList::parse(fields[1])?,
            members: SignedMembers::parse(fields[2])?,
        })
    }

    /// Owners in whose time the anchor was theirs (history), or the current owner.
    fn owners(&self, chain: &OwnerChain, current: bool) -> Vec<Uid> {
        if current { vec![chain.owner()] } else { chain.span_owners(&self.anchor) }
    }

    /// May `uid` hold a leaf: an owner, a confirmer, a manager or a member.
    fn admits_with(&self, uid: &str, owners: &[Uid]) -> bool {
        owners.iter().any(|o| o == uid)
            || self.anchor.anchor().confirmers.iter().any(|c| c == uid)
            || self.list.list().managers.iter().any(|m| m == uid)
            || self.members.members().members.iter().any(|m| m == uid)
    }

    /// The membership rule on replay: "the owner" is any owner of the anchor's span.
    pub fn admits(&self, chain: &OwnerChain, uid: &str) -> bool {
        self.admits_with(uid, &self.owners(chain, false))
    }

    /// The membership rule for current authority: only the current owner.
    pub fn admits_now(&self, chain: &OwnerChain, uid: &str) -> bool {
        self.admits_with(uid, &self.owners(chain, true))
    }

    /// Everyone the extension admits (history), sorted.
    pub fn admitted(&self, chain: &OwnerChain) -> Vec<Uid> {
        let mut all: Vec<Uid> = self
            .owners(chain, false)
            .into_iter()
            .chain(self.anchor.anchor().confirmers.iter().cloned())
            .chain(self.list.list().managers.iter().cloned())
            .chain(self.members.members().members.iter().cloned())
            .collect();
        all.sort();
        all.dedup();
        all
    }

    /// The member list's signer is an owner (of the anchor's span, or only the current one), a
    /// confirmer or a manager of these lists.
    pub(crate) fn signer_authorized(&self, chain: &OwnerChain, current: bool) -> bool {
        let signer = &self.members.members().signer_uid;
        self.owners(chain, current).contains(signer)
            || self.anchor.anchor().confirmers.contains(signer)
            || self.list.list().managers.contains(signer)
    }

    /// Valid as history for `group` at `at` (a commit's time): the manager pair is valid, the member
    /// list names this group and is signed by an owner of the span, a confirmer or a manager, and
    /// both device signers were certified at `at`.
    pub fn valid(&self, chain: &OwnerChain, group: &[u8; 32], at: i64) -> bool {
        self.structure(chain, group)
            && self.list.signer().is_current(at, &())
            && self.members.signer().is_current(at, &())
    }

    /// Valid as history without a commit time: each signer certified when it signed (a member list
    /// may be carried under a later manager list, signed by a device certified only since).
    pub fn valid_when_signed(&self, chain: &OwnerChain, group: &[u8; 32]) -> bool {
        self.structure(chain, group)
            && self.list.signer().is_current(self.list.list().signed_at, &())
            && self.members.signer().is_current(self.members.members().signed_at, &())
    }

    fn structure(&self, chain: &OwnerChain, group: &[u8; 32]) -> bool {
        let m = self.members.members();
        chain.list_valid(&self.anchor, &self.list)
            && m.server_key == chain.genesis().server_key
            && m.generation == chain.generation()
            && m.group_id == b64(group)
            && self.signer_authorized(chain, false)
    }

    /// Current now ((a)–(c) without the checkpoints): the pair may be accepted as current and the
    /// member list's signer holds authority under it now, with a current certificate.
    pub fn current(&self, chain: &OwnerChain, group: &[u8; 32], now: i64, revocations: &dyn Revocations) -> bool {
        self.valid(chain, group, now)
            && chain.list_current(&self.anchor, &self.list, now, revocations)
            && self.signer_authorized(chain, true)
            && self.members.signer().is_current(now, revocations)
    }
}

#[cfg(test)]
mod tests;
