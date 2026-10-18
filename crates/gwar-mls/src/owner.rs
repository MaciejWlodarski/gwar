//! The server's owner and the managers of its private channels (docs/e2e-build.md,
//! section 6, "Owner chain (6b)" and "Managers (6b)"): the statements, the owner
//! chain, and when a confirmer anchor and a manager list are valid as history or
//! current.

use ed25519_dalek::{Signer, SigningKey};
use sha2::{Digest, Sha256};
use vc_proto::Uid;

use crate::credential::{Credential, Revocations};
use crate::encoding::{b64, decimal, fixed, netstrings, parse_netstrings};

const GENESIS: &str = "gwar owner genesis v1";
const LINK: &str = "gwar owner v1";
const ANCHOR: &str = "gwar confirmers v1";
const LIST: &str = "gwar managers v1";

pub const MAX_CONFIRMERS: usize = 32;
pub const MAX_MANAGERS: usize = 100;
/// An encoded anchor plus list.
pub const MAX_PAIR_BYTES: usize = 16 * 1024;
/// Links per `owner.chain` page.
pub const CHAIN_PAGE: usize = 64;

/// SHA-256 of a statement, base64url: how every statement is named.
pub fn statement_hash(statement: &str) -> String {
    b64(&Sha256::digest(statement.as_bytes()))
}

fn uid_list(uids: &[Uid]) -> String {
    if uids.is_empty() { "-".into() } else { uids.join(",") }
}

/// `-` or comma-separated uids, sorted and unique.
fn parse_uids(value: &str) -> Option<Vec<Uid>> {
    if value == "-" {
        return Some(Vec::new());
    }
    let uids: Vec<Uid> = value.split(',').map(str::to_owned).collect();
    let valid = uids.iter().all(|u| fixed::<20>(u).is_some()) && uids.windows(2).all(|w| w[0] < w[1]);
    valid.then_some(uids)
}

fn key_uid(key: &str) -> Option<Uid> {
    Some(vc_proto::uid_for_key(&fixed::<32>(key)?))
}

// ------------------------------------------------------------------ genesis

/// The server's word on its first owner of a generation, pinned on first contact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Genesis {
    pub server_key: String,
    pub generation: u64,
    pub owner_uid: Uid,
}

impl Genesis {
    pub fn statement(&self) -> String {
        format!("{GENESIS}\n{}\n{}\n{}", self.server_key, self.generation, self.owner_uid)
    }

    /// `H0`, link 0 of the chain.
    pub fn hash(&self) -> String {
        statement_hash(&self.statement())
    }

    pub fn parse(statement: &str) -> Option<Self> {
        let [header, server_key, generation, owner_uid] = statement.split('\n').collect::<Vec<_>>()[..] else {
            return None;
        };
        let g =
            Genesis { server_key: server_key.into(), generation: decimal(generation)?, owner_uid: owner_uid.into() };
        let valid = header == GENESIS
            && fixed::<32>(&g.server_key).is_some()
            && g.generation >= 1
            && fixed::<20>(&g.owner_uid).is_some();
        (valid && g.statement() == statement).then_some(g)
    }
}

// --------------------------------------------------------------------- links

/// One move of ownership, signed by the previous owner's account key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerLink {
    pub server_key: String,
    pub generation: u64,
    pub seq: u64,
    pub previous_link_hash: String,
    pub previous_owner_account_key: String,
    pub new_owner_uid: Uid,
    /// The confirmer anchor handed over, by hash.
    pub anchor_hash: Option<String>,
    pub signed_at: i64,
}

impl OwnerLink {
    pub fn statement(&self) -> String {
        format!(
            "{LINK}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.server_key,
            self.generation,
            self.seq,
            self.previous_link_hash,
            self.previous_owner_account_key,
            self.new_owner_uid,
            self.anchor_hash.as_deref().unwrap_or("-"),
            self.signed_at,
        )
    }

    pub fn hash(&self) -> String {
        statement_hash(&self.statement())
    }

    pub fn parse(statement: &str) -> Option<Self> {
        let [header, server_key, generation, seq, previous, key, new_owner, anchor, signed_at] =
            statement.split('\n').collect::<Vec<_>>()[..]
        else {
            return None;
        };
        let link = OwnerLink {
            server_key: server_key.into(),
            generation: decimal(generation)?,
            seq: decimal(seq)?,
            previous_link_hash: previous.into(),
            previous_owner_account_key: key.into(),
            new_owner_uid: new_owner.into(),
            anchor_hash: (anchor != "-").then(|| anchor.to_owned()),
            signed_at: decimal(signed_at)? as i64,
        };
        let valid = header == LINK
            && fixed::<32>(&link.server_key).is_some()
            && link.generation >= 1
            && link.seq >= 1
            && fixed::<32>(&link.previous_link_hash).is_some()
            && fixed::<32>(&link.previous_owner_account_key).is_some()
            && fixed::<20>(&link.new_owner_uid).is_some()
            && link.anchor_hash.as_deref().is_none_or(|h| fixed::<32>(h).is_some());
        (valid && link.statement() == statement).then_some(link)
    }

    /// The uid whose account key signs this link.
    pub fn previous_owner(&self) -> Uid {
        key_uid(&self.previous_owner_account_key).expect("parsed links hold valid keys")
    }
}

/// A link with the previous owner's raw Ed25519 signature (base64url). Built only by
/// [`SignedLink::sign`] and [`SignedLink::parse`], so its signature always matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedLink {
    link: OwnerLink,
    signature: String,
}

impl SignedLink {
    pub fn link(&self) -> &OwnerLink {
        &self.link
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// Signs with the previous owner's account key, which the link must name.
    pub fn sign(link: OwnerLink, account: &SigningKey) -> Self {
        assert_eq!(link.previous_owner_account_key, b64(account.verifying_key().as_bytes()));
        let signature = b64(&account.sign(link.statement().as_bytes()).to_bytes());
        Self { link, signature }
    }

    /// Parses a statement and its signature; the signature is checked against the key it names.
    pub fn parse(statement: &str, signature: &str) -> Option<Self> {
        let link = OwnerLink::parse(statement)?;
        vc_proto::signed_by(&link.previous_owner_account_key, statement, signature)
            .then(|| Self { link, signature: signature.to_owned() })
    }
}

// --------------------------------------------------------------------- chain

/// A verified owner chain of one generation: the genesis and every move since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerChain {
    genesis: Genesis,
    links: Vec<SignedLink>,
    /// `hashes[i]` names link `i` (0 = the genesis).
    hashes: Vec<String>,
}

impl OwnerChain {
    pub fn new(genesis: Genesis) -> Self {
        let hashes = vec![genesis.hash()];
        Self { genesis, links: Vec::new(), hashes }
    }

    /// Verifies a whole chain.
    pub fn verify(genesis: Genesis, links: Vec<SignedLink>) -> Option<Self> {
        let mut chain = Self::new(genesis);
        for link in links {
            chain.push(link)?;
        }
        Some(chain)
    }

    /// Appends a link that extends the head: next seq, previous hash, signed by the current owner.
    pub fn push(&mut self, signed: SignedLink) -> Option<()> {
        let link = &signed.link;
        let extends = link.server_key == self.genesis.server_key
            && link.generation == self.genesis.generation
            && link.seq == self.seq() + 1
            && link.previous_link_hash == self.head()
            && link.previous_owner() == self.owner()
            && vc_proto::signed_by(&link.previous_owner_account_key, &link.statement(), &signed.signature);
        if !extends {
            return None;
        }
        self.hashes.push(link.hash());
        self.links.push(signed);
        Some(())
    }

    pub fn genesis(&self) -> &Genesis {
        &self.genesis
    }

    pub fn links(&self) -> &[SignedLink] {
        &self.links
    }

    pub fn generation(&self) -> u64 {
        self.genesis.generation
    }

    pub fn seq(&self) -> u64 {
        self.links.len() as u64
    }

    /// The hash of the newest link (the genesis' when ownership never moved).
    pub fn head(&self) -> String {
        self.hashes.last().expect("a chain has a genesis").clone()
    }

    /// The current owner.
    pub fn owner(&self) -> Uid {
        self.owner_at_index(self.links.len())
    }

    fn owner_at_index(&self, i: usize) -> Uid {
        if i == 0 { self.genesis.owner_uid.clone() } else { self.links[i - 1].link.new_owner_uid.clone() }
    }

    /// Where a link hash sits in the chain (0 = the genesis).
    pub fn position(&self, hash: &str) -> Option<usize> {
        self.hashes.iter().position(|h| h == hash)
    }

    /// The owner from the link named `hash` on.
    pub fn owner_at(&self, hash: &str) -> Option<Uid> {
        self.position(hash).map(|i| self.owner_at_index(i))
    }

    /// The links after seq `after` (at most a page), and whether more follow.
    pub fn page(&self, after: u64) -> (&[SignedLink], bool) {
        let start = (after as usize).min(self.links.len());
        let end = (start + CHAIN_PAGE).min(self.links.len());
        (&self.links[start..end], end < self.links.len())
    }

    // ----------------------------------------------------- anchors and lists

    fn names(&self, anchor: &SignedAnchor) -> bool {
        anchor.anchor.server_key == self.genesis.server_key && anchor.anchor.generation == self.genesis.generation
    }

    /// The links over which an anchor stays the owner's: its own, then each later link that hands
    /// it over by hash. `None` when it does not name a link of this chain.
    fn span(&self, anchor: &SignedAnchor) -> Option<std::ops::RangeInclusive<usize>> {
        let start = self.position(&anchor.anchor.owner_head)?;
        let hash = anchor.hash();
        let handed = self.links[start..].iter().take_while(|l| l.link.anchor_hash.as_deref() == Some(&hash)).count();
        Some(start..=start + handed)
    }

    /// Valid as history: it names a link of this chain and that link's owner signed it, naming
    /// at most 32 confirmers and not themselves.
    pub fn anchor_valid(&self, anchor: &SignedAnchor) -> bool {
        self.names(anchor)
            && self.owner_at(&anchor.anchor.owner_head).is_some_and(|owner| {
                key_uid(&anchor.account_key).as_ref() == Some(&owner) && !anchor.anchor.confirmers.contains(&owner)
            })
            && anchor.anchor.confirmers.len() <= MAX_CONFIRMERS
    }

    /// Current: valid, and minted by the current owner or handed over by every later link.
    pub fn anchor_current(&self, anchor: &SignedAnchor) -> bool {
        self.anchor_valid(anchor) && self.span(anchor).is_some_and(|span| *span.end() == self.links.len())
    }

    /// Owners in whose time the anchor was theirs.
    pub(crate) fn span_owners(&self, anchor: &SignedAnchor) -> Vec<Uid> {
        self.span(anchor).map(|span| span.map(|i| self.owner_at_index(i)).collect()).unwrap_or_default()
    }

    /// Valid as history: its anchor is valid, it names it, and it is signed by a confirmer on it or
    /// by an owner of the anchor's span; bounds hold.
    pub fn list_valid(&self, anchor: &SignedAnchor, list: &SignedList) -> bool {
        let l = &list.list;
        let owners = self.span_owners(anchor);
        self.anchor_valid(anchor)
            && l.server_key == self.genesis.server_key
            && l.generation == self.genesis.generation
            && l.anchor_hash == anchor.hash()
            && (anchor.anchor.confirmers.contains(&l.signer_uid) || owners.contains(&l.signer_uid))
            && pair_within_bounds(anchor, list, owners.first().map(std::slice::from_ref).unwrap_or_default())
    }

    /// May be accepted as current now: valid, its anchor current, signed by the current owner or
    /// a confirmer, with a device certificate current now.
    pub fn list_current(
        &self,
        anchor: &SignedAnchor,
        list: &SignedList,
        now: i64,
        revocations: &dyn Revocations,
    ) -> bool {
        let signer = &list.list.signer_uid;
        self.list_valid(anchor, list)
            && self.anchor_current(anchor)
            && (*signer == self.owner() || anchor.anchor.confirmers.contains(signer))
            && !anchor.anchor.confirmers.contains(&self.owner())
            && !list.list.managers.contains(&self.owner())
            && list.signer.is_current(now, revocations)
    }
}

/// At most 32 confirmers and 100 managers, never the anchor's signing owner (`owners`; a later
/// owner is excluded only from current pairs, so history stays valid after a move), no one
/// both, at most 16 KiB.
fn pair_within_bounds(anchor: &SignedAnchor, list: &SignedList, owners: &[Uid]) -> bool {
    let (confirmers, managers) = (&anchor.anchor.confirmers, &list.list.managers);
    confirmers.len() <= MAX_CONFIRMERS
        && managers.len() <= MAX_MANAGERS
        && !confirmers.iter().chain(managers).any(|u| owners.contains(u))
        && !managers.iter().any(|m| confirmers.contains(m))
        && anchor.encode().len() + list.encode().len() <= MAX_PAIR_BYTES
}

// -------------------------------------------------------------------- anchor

/// Who may maintain the manager list, signed by the owner's account key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchor {
    pub server_key: String,
    pub generation: u64,
    /// The owner chain head it was signed under.
    pub owner_head: String,
    pub anchor_seq: u64,
    pub confirmers: Vec<Uid>,
    pub signed_at: i64,
}

impl Anchor {
    pub fn statement(&self) -> String {
        format!(
            "{ANCHOR}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.server_key,
            self.generation,
            self.owner_head,
            self.anchor_seq,
            uid_list(&self.confirmers),
            self.signed_at,
        )
    }

    pub fn parse(statement: &str) -> Option<Self> {
        let [header, server_key, generation, owner_head, seq, confirmers, signed_at] =
            statement.split('\n').collect::<Vec<_>>()[..]
        else {
            return None;
        };
        let a = Anchor {
            server_key: server_key.into(),
            generation: decimal(generation)?,
            owner_head: owner_head.into(),
            anchor_seq: decimal(seq)?,
            confirmers: parse_uids(confirmers)?,
            signed_at: decimal(signed_at)? as i64,
        };
        let valid = header == ANCHOR
            && fixed::<32>(&a.server_key).is_some()
            && a.generation >= 1
            && fixed::<32>(&a.owner_head).is_some()
            && a.anchor_seq >= 1;
        (valid && a.statement() == statement).then_some(a)
    }
}

/// An anchor with the signing owner's account key and its signature. Built only by
/// [`SignedAnchor::sign`] and [`SignedAnchor::parse`], so the signature always matches the key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedAnchor {
    anchor: Anchor,
    account_key: String,
    signature: String,
}

impl SignedAnchor {
    pub fn anchor(&self) -> &Anchor {
        &self.anchor
    }

    pub fn account_key(&self) -> &str {
        &self.account_key
    }

    pub fn sign(anchor: Anchor, account: &SigningKey) -> Self {
        let signature = b64(&account.sign(anchor.statement().as_bytes()).to_bytes());
        Self { anchor, account_key: b64(account.verifying_key().as_bytes()), signature }
    }

    pub fn encode(&self) -> Vec<u8> {
        netstrings(&[self.anchor.statement().as_bytes(), self.account_key.as_bytes(), self.signature.as_bytes()])
    }

    /// Parses three netstrings and checks the signature by the key they carry. Whose key it is,
    /// is the chain's question ([`OwnerChain::anchor_valid`]).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let fields = parse_netstrings(bytes, 3)?;
        let statement = std::str::from_utf8(fields[0]).ok()?;
        let anchor = Anchor::parse(statement)?;
        let account_key = std::str::from_utf8(fields[1]).ok()?.to_owned();
        let signature = std::str::from_utf8(fields[2]).ok()?.to_owned();
        (fixed::<32>(&account_key).is_some() && vc_proto::signed_by(&account_key, statement, &signature))
            .then_some(Self { anchor, account_key, signature })
    }

    pub fn hash(&self) -> String {
        statement_hash(&self.anchor.statement())
    }
}

// ---------------------------------------------------------------------- list

/// The plain managers, signed by a device of the owner or of a confirmer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerList {
    pub server_key: String,
    pub generation: u64,
    pub anchor_hash: String,
    pub seq: u64,
    pub managers: Vec<Uid>,
    pub signer_uid: Uid,
    pub signed_at: i64,
}

impl ManagerList {
    pub fn statement(&self) -> String {
        format!(
            "{LIST}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.server_key,
            self.generation,
            self.anchor_hash,
            self.seq,
            uid_list(&self.managers),
            self.signer_uid,
            self.signed_at,
        )
    }

    pub fn parse(statement: &str) -> Option<Self> {
        let [header, server_key, generation, anchor_hash, seq, managers, signer_uid, signed_at] =
            statement.split('\n').collect::<Vec<_>>()[..]
        else {
            return None;
        };
        let l = ManagerList {
            server_key: server_key.into(),
            generation: decimal(generation)?,
            anchor_hash: anchor_hash.into(),
            seq: decimal(seq)?,
            managers: parse_uids(managers)?,
            signer_uid: signer_uid.into(),
            signed_at: decimal(signed_at)? as i64,
        };
        let valid = header == LIST
            && fixed::<32>(&l.server_key).is_some()
            && l.generation >= 1
            && fixed::<32>(&l.anchor_hash).is_some()
            && l.seq >= 1
            && fixed::<20>(&l.signer_uid).is_some();
        (valid && l.statement() == statement).then_some(l)
    }
}

/// A list with its signer's credential and device signature. Built only by [`SignedList::sign`]
/// and [`SignedList::parse`], so the credential is the signer's and the signature matches.
#[derive(Debug, Clone, PartialEq)]
pub struct SignedList {
    list: ManagerList,
    signer: Credential,
    signature: String,
}

impl SignedList {
    pub fn list(&self) -> &ManagerList {
        &self.list
    }

    pub fn signer(&self) -> &Credential {
        &self.signer
    }

    /// Signs with the signer's device key, which must be the credential's.
    pub fn sign(list: ManagerList, signer: Credential, device: &SigningKey) -> Self {
        // Only a well-formed credential with a valid certificate signs, as `parse` would accept it.
        assert_eq!(Credential::parse(signer.encode().as_bytes()).as_ref(), Some(&signer), "a verified credential");
        assert_eq!(signer.device_key(), b64(device.verifying_key().as_bytes()));
        assert_eq!(signer.uid(), list.signer_uid);
        let signature = b64(&device.sign(list.statement().as_bytes()).to_bytes());
        Self { list, signer, signature }
    }

    pub fn encode(&self) -> Vec<u8> {
        netstrings(&[self.list.statement().as_bytes(), self.signer.encode().as_bytes(), self.signature.as_bytes()])
    }

    /// Parses and verifies: a valid list, a well-formed credential of `signer_uid`, the device's signature.
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let fields = parse_netstrings(bytes, 3)?;
        let statement = std::str::from_utf8(fields[0]).ok()?;
        let list = ManagerList::parse(statement)?;
        let signer = Credential::parse(fields[1])?;
        let signature = std::str::from_utf8(fields[2]).ok()?.to_owned();
        (signer.uid() == list.signer_uid && vc_proto::signed_by(signer.device_key(), statement, &signature))
            .then_some(Self { list, signer, signature })
    }

    pub fn hash(&self) -> String {
        statement_hash(&self.list.statement())
    }
}

// --------------------------------------------------------------- checkpoints

/// The newest accepted `(seq, hash)` of a statement series.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    pub seq: u64,
    pub hash: String,
}

impl Checkpoint {
    /// A lower seq never; the same seq only with the same statement.
    pub fn admits(&self, seq: u64, hash: &str) -> bool {
        seq > self.seq || (seq == self.seq && hash == self.hash)
    }
}

#[cfg(test)]
mod tests;
