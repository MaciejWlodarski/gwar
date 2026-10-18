use super::*;
use crate::credential::tests::{certified, key, public};
use crate::owner::{Anchor, Genesis, ManagerList, OwnerLink, SignedLink};

const NOW: i64 = 10_000;
const GROUP: [u8; 32] = [7; 32];

fn uid(seed: u8) -> Uid {
    vc_proto::uid_for_key(key(seed).verifying_key().as_bytes())
}

fn server() -> String {
    public(&key(100))
}

fn sorted(seeds: &[u8]) -> Vec<Uid> {
    let mut v: Vec<Uid> = seeds.iter().map(|&s| uid(s)).collect();
    v.sort();
    v
}

fn chain() -> OwnerChain {
    OwnerChain::new(Genesis { server_key: server(), generation: 1, owner_uid: uid(1) })
}

fn pair(chain: &OwnerChain, confirmers: &[u8], managers: &[u8], signer: u8) -> (SignedAnchor, SignedList) {
    let a = Anchor {
        server_key: server(),
        generation: 1,
        owner_head: chain.head(),
        anchor_seq: 1,
        confirmers: sorted(confirmers),
        signed_at: NOW,
    };
    let anchor = SignedAnchor::sign(a, &key(1));
    let l = ManagerList {
        server_key: server(),
        generation: 1,
        anchor_hash: anchor.hash(),
        seq: 1,
        managers: sorted(managers),
        signer_uid: uid(signer),
        signed_at: NOW,
    };
    let list = SignedList::sign(l, certified(&key(signer), &key(signer + 50), NOW - 1), &key(signer + 50));
    (anchor, list)
}

fn members(group: [u8; 32], uids: &[u8], signer: u8) -> SignedMembers {
    let m = ChannelMembers {
        server_key: server(),
        generation: 1,
        group_id: b64(&group),
        seq: 1,
        members: sorted(uids),
        signer_uid: uid(signer),
        signed_at: NOW,
    };
    SignedMembers::sign(m, certified(&key(signer), &key(signer + 50), NOW - 1), &key(signer + 50))
}

#[test]
fn the_statement_and_extension_round_trip() {
    let c = chain();
    let (anchor, list) = pair(&c, &[5], &[6], 1);
    let m = members(GROUP, &[8, 9], 6);
    assert_eq!(
        m.members().statement(),
        format!(
            "gwar channel members v1\n{}\n1\n{}\n1\n{}\n{}\n{NOW}",
            server(),
            b64(&GROUP),
            sorted(&[8, 9]).join(","),
            uid(6)
        )
    );
    let access = Access::new(anchor, list, m);
    assert_eq!(Access::parse(&access.encode()), Some(access.clone()));
    let tampered = String::from_utf8(access.encode()).unwrap().replace(&uid(8), &uid(4)).into_bytes();
    assert_eq!(Access::parse(&tampered), None);
}

#[test]
fn a_member_list_is_valid_only_for_its_group_and_from_an_authorized_signer() {
    let c = chain();
    let (anchor, list) = pair(&c, &[5], &[6], 1);
    // A manager, a confirmer and the owner sign; a member does not.
    for signer in [6, 5, 1] {
        assert!(Access::new(anchor.clone(), list.clone(), members(GROUP, &[8], signer)).valid(&c, &GROUP, NOW));
    }
    assert!(!Access::new(anchor.clone(), list.clone(), members(GROUP, &[8], 8)).valid(&c, &GROUP, NOW));
    // Bound to its group.
    assert!(!Access::new(anchor.clone(), list.clone(), members([3; 32], &[8], 6)).valid(&c, &GROUP, NOW));
    // Certified at the commit's time, not after.
    assert!(!Access::new(anchor, list, members(GROUP, &[8], 6)).valid(&c, &GROUP, NOW + 31 * 86_400_000));
}

#[test]
fn admission_follows_the_lists_and_the_owner_of_the_time() {
    let mut c = chain();
    let (anchor, list) = pair(&c, &[5], &[6], 1);
    let access = Access::new(anchor.clone(), list, members(GROUP, &[8], 6));
    for seed in [1, 5, 6, 8] {
        assert!(access.admits(&c, &uid(seed)) && access.admits_now(&c, &uid(seed)));
    }
    assert!(!access.admits(&c, &uid(9)));
    assert_eq!(access.admitted(&c), sorted(&[1, 5, 6, 8]));
    // The owner moves to 2, handing the anchor over: 1 stays admitted as history, not now.
    let link = OwnerLink {
        server_key: server(),
        generation: 1,
        seq: 1,
        previous_link_hash: c.head(),
        previous_owner_account_key: public(&key(1)),
        new_owner_uid: uid(2),
        anchor_hash: Some(anchor.hash()),
        signed_at: NOW,
    };
    c.push(SignedLink::sign(link, &key(1))).unwrap();
    assert!(access.admits(&c, &uid(1)) && !access.admits_now(&c, &uid(1)));
    assert!(access.admits(&c, &uid(2)) && access.admits_now(&c, &uid(2)));
}

#[test]
fn currency_needs_a_current_pair_and_a_currently_authorized_signer() {
    let c = chain();
    let (anchor, list) = pair(&c, &[5], &[6], 1);
    let access = Access::new(anchor.clone(), list.clone(), members(GROUP, &[8], 6));
    assert!(access.current(&c, &GROUP, NOW, &()));
    struct Revoked;
    impl Revocations for Revoked {
        fn revoked_at(&self, _: &str, _: &str) -> Option<i64> {
            Some(NOW - 1)
        }
    }
    assert!(access.valid(&c, &GROUP, NOW) && !access.current(&c, &GROUP, NOW, &Revoked));
    // Signed by a manager the newest list no longer names: history, not current.
    let (_, without) = pair(&c, &[5], &[], 1);
    let stale = Access::new(anchor, without, members(GROUP, &[8], 6));
    assert!(!stale.valid(&c, &GROUP, NOW) && !stale.current(&c, &GROUP, NOW, &()));
}

#[test]
fn bounds_hold() {
    let many: Vec<u8> = (0..=255u8).collect();
    let c = chain();
    let (anchor, list) = pair(&c, &[], &[], 1);
    let ok = Access::new(anchor, list, members(GROUP, &many[110..], 1));
    assert!(ok.valid(&c, &GROUP, NOW));
    // A statement with more than 1000 members does not parse.
    let mut sorted_many: Vec<String> = (0..1001u32)
        .map(|i| {
            let mut bytes = [0u8; 20];
            bytes[..4].copy_from_slice(&i.to_be_bytes());
            b64(&bytes)
        })
        .collect();
    sorted_many.sort();
    let statement = format!(
        "gwar channel members v1\n{}\n1\n{}\n1\n{}\n{}\n{NOW}",
        server(),
        b64(&GROUP),
        sorted_many.join(","),
        uid(1)
    );
    assert!(sorted_many.len() > MAX_MEMBERS && ChannelMembers::parse(&statement).is_none());
    // The extension's size bound.
    assert!(Access::parse(&vec![b'0'; MAX_ACCESS_BYTES + 1]).is_none());
}
