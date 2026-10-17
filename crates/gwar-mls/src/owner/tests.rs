use super::*;
use crate::credential::tests::{certified, key, public};

const NOW: i64 = 10_000;

fn uid(seed: u8) -> Uid {
    vc_proto::uid_for_key(key(seed).verifying_key().as_bytes())
}

fn server() -> String {
    public(&key(100))
}

fn genesis(owner: u8) -> Genesis {
    Genesis { server_key: server(), generation: 1, owner_uid: uid(owner) }
}

/// The current owner (account `from`) moves ownership to `to`, handing over `anchor`.
fn transfer(chain: &mut OwnerChain, from: u8, to: u8, anchor: Option<&SignedAnchor>) {
    let link = OwnerLink {
        server_key: server(),
        generation: 1,
        seq: chain.seq() + 1,
        previous_link_hash: chain.head(),
        previous_owner_account_key: public(&key(from)),
        new_owner_uid: uid(to),
        anchor_hash: anchor.map(SignedAnchor::hash),
        signed_at: NOW,
    };
    chain.push(SignedLink::sign(link, &key(from))).expect("extends");
}

fn anchor(owner: u8, head: &str, seq: u64, confirmers: &[u8]) -> SignedAnchor {
    let mut confirmers: Vec<Uid> = confirmers.iter().map(|&c| uid(c)).collect();
    confirmers.sort();
    let a = Anchor {
        server_key: server(),
        generation: 1,
        owner_head: head.into(),
        anchor_seq: seq,
        confirmers,
        signed_at: NOW,
    };
    SignedAnchor::sign(a, &key(owner))
}

/// A list signed by `signer`'s device (account `signer`, device `signer + 50`).
fn list(anchor: &SignedAnchor, seq: u64, managers: &[u8], signer: u8) -> SignedList {
    let mut managers: Vec<Uid> = managers.iter().map(|&m| uid(m)).collect();
    managers.sort();
    let l = ManagerList {
        server_key: server(),
        generation: 1,
        anchor_hash: anchor.hash(),
        seq,
        managers,
        signer_uid: uid(signer),
        signed_at: NOW,
    };
    SignedList::sign(l, certified(&key(signer), &key(signer + 50), NOW - 1), &key(signer + 50))
}

#[test]
fn statements_have_the_documented_text_and_round_trip() {
    let g = genesis(1);
    assert_eq!(g.statement(), format!("gwar owner genesis v1\n{}\n1\n{}", server(), uid(1)));
    assert_eq!(Genesis::parse(&g.statement()), Some(g.clone()));

    let mut chain = OwnerChain::new(g.clone());
    let a = anchor(1, &chain.head(), 1, &[2]);
    assert_eq!(
        a.anchor.statement(),
        format!("gwar confirmers v1\n{}\n1\n{}\n1\n{}\n{NOW}", server(), g.hash(), uid(2))
    );
    assert_eq!(SignedAnchor::parse(&a.encode()), Some(a.clone()));

    transfer(&mut chain, 1, 3, Some(&a));
    let link = &chain.links()[0];
    assert_eq!(
        link.link.statement(),
        format!(
            "gwar owner v1\n{}\n1\n1\n{}\n{}\n{}\n{}\n{NOW}",
            server(),
            g.hash(),
            public(&key(1)),
            uid(3),
            a.hash()
        )
    );
    assert_eq!(SignedLink::parse(&link.link.statement(), &link.signature), Some(link.clone()));

    let l = list(&a, 1, &[], 2);
    assert_eq!(l.list.statement(), format!("gwar managers v1\n{}\n1\n{}\n1\n-\n{}\n{NOW}", server(), a.hash(), uid(2)));
    assert_eq!(SignedList::parse(&l.encode()), Some(l.clone()));
}

#[test]
fn malformed_statements_are_refused() {
    let g = genesis(1).statement();
    for bad in [
        format!("{g}\n"),
        g.replace("\n1\n", "\n01\n"),
        g.replace("\n1\n", "\n0\n"),
        g.replace("genesis v1", "genesis v2"),
    ] {
        assert_eq!(Genesis::parse(&bad), None, "{bad}");
    }
    let chain = OwnerChain::new(genesis(1));
    let a = anchor(1, &chain.head(), 1, &[2, 3]).anchor.statement();
    let (u2, u3) = (uid(2), uid(3));
    let (lo, hi) = if u2 < u3 { (u2, u3) } else { (u3, u2) };
    for bad in [
        a.replace(&format!("{lo},{hi}"), &format!("{hi},{lo}")),
        a.replace(&format!("{lo},{hi}"), &format!("{lo},{lo}")),
        a.replace(&format!("{lo},{hi}"), ""),
    ] {
        assert_eq!(Anchor::parse(&bad), None, "unsorted, repeated or empty confirmers: {bad}");
    }
    // A signature by another key than the one carried.
    let mut forged = anchor(1, &chain.head(), 1, &[]);
    forged.account_key = public(&key(9));
    assert_eq!(SignedAnchor::parse(&forged.encode()), None);
}

#[test]
fn the_chain_accepts_only_extensions_signed_by_the_current_owner() {
    let mut chain = OwnerChain::new(genesis(1));
    let link = |seq: u64, previous: String, from: u8, to: u8, generation: u64| OwnerLink {
        server_key: server(),
        generation,
        seq,
        previous_link_hash: previous,
        previous_owner_account_key: public(&key(from)),
        new_owner_uid: uid(to),
        anchor_hash: None,
        signed_at: NOW,
    };
    let head = chain.head();
    for (bad, signer) in [
        (link(1, head.clone(), 2, 3, 1), 2),            // not the owner's key
        (link(2, head.clone(), 1, 3, 1), 1),            // a gap
        (link(1, statement_hash("other"), 1, 3, 1), 1), // a fork
        (link(1, head.clone(), 1, 3, 2), 1),            // another generation
    ] {
        assert!(chain.clone().push(SignedLink::sign(bad, &key(signer))).is_none());
    }
    transfer(&mut chain, 1, 3, None);
    transfer(&mut chain, 3, 4, None);
    assert_eq!((chain.seq(), chain.owner()), (2, uid(4)));
    assert_eq!(chain.owner_at(&chain.genesis().hash()), Some(uid(1)));
    // The same links verify from scratch, and a lower seq is no extension.
    let again = OwnerChain::verify(genesis(1), chain.links().to_vec()).unwrap();
    assert_eq!(again, chain);
    assert!(OwnerChain::verify(genesis(1), chain.links()[1..].to_vec()).is_none());
}

#[test]
fn anchors_are_current_only_from_the_current_owner_or_handed_over_by_hash() {
    let mut chain = OwnerChain::new(genesis(1));
    let h0 = chain.head();
    let x = anchor(1, &h0, 1, &[5]);
    assert!(chain.anchor_current(&x));
    // A hands X over to B.
    transfer(&mut chain, 1, 2, Some(&x));
    assert!(chain.anchor_current(&x), "inherited");
    // A, no longer the owner, signs a higher anchor under its old head: history at most.
    let z = anchor(1, &h0, 9, &[6]);
    assert!(chain.anchor_valid(&z) && !chain.anchor_current(&z));
    // B mints one under the head.
    let y = anchor(2, &chain.head(), 2, &[5]);
    assert!(chain.anchor_current(&y));
    // B cannot sign under A's head, nor A under B's.
    assert!(!chain.anchor_valid(&anchor(2, &h0, 3, &[])));
    assert!(!chain.anchor_valid(&anchor(1, &chain.head(), 3, &[])));
    // B moves ownership on handing over Y: X stays valid history, no longer current.
    transfer(&mut chain, 2, 3, Some(&y));
    assert!(chain.anchor_current(&y) && chain.anchor_valid(&x) && !chain.anchor_current(&x));
    // An anchor of another chain is not valid here.
    let other = OwnerChain::new(Genesis { generation: 2, ..genesis(1) });
    assert!(!other.anchor_valid(&x));
}

#[test]
fn lists_are_history_or_current_by_their_signer_and_anchor() {
    let mut chain = OwnerChain::new(genesis(1));
    let x = anchor(1, &chain.head(), 1, &[5]);
    // The owner and a confirmer sign current lists; a manager does not.
    assert!(chain.list_current(&x, &list(&x, 1, &[7], 1), NOW, &()));
    assert!(chain.list_current(&x, &list(&x, 2, &[7], 5), NOW, &()));
    assert!(!chain.list_valid(&x, &list(&x, 3, &[], 7)));
    // A hands X over to B: A's list stays history, B renews it as current.
    let by_a = list(&x, 3, &[7], 1);
    transfer(&mut chain, 1, 2, Some(&x));
    assert!(chain.list_valid(&x, &by_a) && !chain.list_current(&x, &by_a, NOW, &()));
    assert!(chain.list_current(&x, &list(&x, 4, &[7], 2), NOW, &()));
    // X → Y → transfer naming Y: lists under X still verify as history.
    let y = anchor(2, &chain.head(), 2, &[5]);
    transfer(&mut chain, 2, 3, Some(&y));
    assert!(chain.list_valid(&x, &by_a));
    assert!(!chain.list_current(&x, &list(&x, 5, &[], 5), NOW, &()), "X is no longer current");
    assert!(chain.list_current(&y, &list(&y, 5, &[], 5), NOW, &()));
    // A list must name its anchor.
    assert!(!chain.list_valid(&y, &list(&x, 6, &[], 5)));
}

#[test]
fn bounds_and_certificates_limit_lists() {
    let chain = OwnerChain::new(genesis(1));
    let x = anchor(1, &chain.head(), 1, &[5]);
    assert!(!chain.list_valid(&x, &list(&x, 1, &[5], 1)), "a manager who is also a confirmer");
    assert!(!chain.list_valid(&x, &list(&x, 1, &[1], 5)), "the owner as a manager");
    assert!(
        !chain.list_valid(&anchor(1, &chain.head(), 1, &[1]), &list(&anchor(1, &chain.head(), 1, &[1]), 1, &[], 1))
    );
    let many: Vec<u8> = (110..=210).collect();
    assert!(!chain.list_valid(&x, &list(&x, 1, &many, 1)), "101 managers");
    assert!(chain.list_valid(&x, &list(&x, 1, &many[..100], 1)));
    // An expired or revoked signer's list is history, not current.
    let late = list(&x, 1, &[], 5);
    assert!(!chain.list_current(&x, &late, NOW + 31 * 86_400_000, &()));
    struct Revoked;
    impl Revocations for Revoked {
        fn revoked_at(&self, _: &str, _: &str) -> Option<i64> {
            Some(NOW - 1)
        }
    }
    assert!(chain.list_valid(&x, &late) && !chain.list_current(&x, &late, NOW, &Revoked));
}

#[test]
fn checkpoints_never_go_back_or_change_at_a_seq() {
    let c = Checkpoint { seq: 3, hash: "h".into() };
    assert!(c.admits(4, "x") && c.admits(3, "h"));
    assert!(!c.admits(3, "x") && !c.admits(2, "h"));
}

#[test]
fn chain_pages_are_bounded() {
    let mut chain = OwnerChain::new(genesis(1));
    for i in 0..70u8 {
        let (from, to) = (if i == 0 { 1 } else { 10 + i - 1 }, 10 + i);
        transfer(&mut chain, from, to, None);
    }
    let (page, more) = chain.page(0);
    assert_eq!((page.len(), more), (CHAIN_PAGE, true));
    let (page, more) = chain.page(64);
    assert_eq!((page.len(), more, page[0].link.seq), (6, false, 65));
    assert!(chain.page(70).0.is_empty());
}
