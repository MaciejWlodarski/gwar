use super::*;
use rand::{Rng, RngCore, SeedableRng, rngs::StdRng};
use serde::Deserialize;
use sframe::{CipherSuite, frame::SingleUseCounter};

#[derive(Deserialize)]
struct Vectors {
    frames: Vec<Vector>,
    rejected: Vec<Vector>,
}
#[derive(Deserialize)]
struct Vector {
    id: String,
    codec: String,
    kid: String,
    ctr: String,
    base_key: String,
    plaintext: String,
    clear: usize,
    wire: String,
    canonical_wire: Option<String>,
}
fn vectors() -> Vectors {
    serde_json::from_str(include_str!("../../tests/screen-sframe-vectors.json")).unwrap()
}
fn hex(text: &str) -> Vec<u8> {
    text.as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}
fn layout(codec: &str) -> VideoLayout {
    match codec {
        "vp9" => VideoLayout::Vp9,
        "vp8" => VideoLayout::Vp8,
        _ => panic!("unknown test codec"),
    }
}
fn keys(kid: u64, base: &[u8]) -> (EncryptionKey, DecryptionKey) {
    (
        EncryptionKey::derive_from(CipherSuite::AesGcm128Sha256, kid, base).unwrap(),
        DecryptionKey::derive_from(CipherSuite::AesGcm128Sha256, kid, base).unwrap(),
    )
}
#[test]
fn shared_vectors_match_sealing_and_opening_at_every_kid_ctr_width() {
    let vectors = vectors();
    assert_eq!(vectors.frames.len(), 400);
    let mut widths = std::collections::BTreeSet::new();
    for v in vectors.frames {
        let layout = layout(&v.codec);
        let (kid, ctr) = (v.kid.parse().unwrap(), v.ctr.parse().unwrap());
        let (enc, dec) = keys(kid, &hex(&v.base_key));
        let (pt, wire) = (hex(&v.plaintext), hex(&v.wire));
        assert_eq!(layout.clear_len(&pt), Ok(v.clear));
        assert_eq!(layout.seal(&enc, &mut SingleUseCounter::new(ctr), &pt).unwrap(), wire, "{}", v.id);
        assert_eq!(layout.open(&dec, &wire).unwrap(), pt, "{}", v.id);
        let config = wire[v.clear];
        let k = if config & 0x80 == 0 { 0 } else { ((config >> 4) & 7) + 1 };
        let c = if config & 8 == 0 { 0 } else { (config & 7) + 1 };
        widths.insert((k, c));
        let parsed = layout.parse(&wire).unwrap();
        assert_eq!((parsed.header().key_id(), parsed.header().counter()), (kid, ctr));
    }
    assert_eq!(widths.len(), 81); // inline + all 1..=8-byte encodings, independently paired
}
#[test]
fn authenticated_noncanonical_headers_are_rejected_without_rewriting() {
    for v in vectors().rejected {
        let layout = layout(&v.codec);
        let (_, dec) = keys(v.kid.parse().unwrap(), &hex(&v.base_key));
        assert!(layout.parse(&hex(&v.wire)).is_err(), "{}", v.id);
        assert!(layout.open(&dec, &hex(&v.wire)).is_err());
        assert_eq!(layout.open(&dec, &hex(&v.canonical_wire.unwrap())).unwrap(), hex(&v.plaintext));
    }
}
#[test]
fn every_bit_of_prefix_header_ciphertext_and_tag_is_authenticated() {
    // Includes compact and widest headers, for both codecs and frame types.
    for v in vectors().frames.into_iter().filter(|v| v.kid == v.ctr && (v.kid == "0" || v.kid == u64::MAX.to_string()))
    {
        let layout = layout(&v.codec);
        let kid = v.kid.parse().unwrap();
        let (_, dec) = keys(kid, &hex(&v.base_key));
        let wire = hex(&v.wire);
        for i in 0..wire.len() {
            for bit in 0..8 {
                let mut bad = wire.clone();
                bad[i] ^= 1 << bit;
                assert!(layout.open(&dec, &bad).is_err(), "{} byte {i} bit {bit}", v.id);
            }
        }
        for length in 0..wire.len() {
            assert!(layout.open(&dec, &wire[..length]).is_err(), "{} length {length}", v.id);
        }
        let (_, wrong_key) = keys(kid, &[0xff; 32]);
        let (_, wrong_epoch) = keys(kid ^ (1 << 32), &hex(&v.base_key));
        assert!(layout.open(&wrong_key, &wire).is_err());
        assert!(layout.open(&wrong_epoch, &wire).is_err());
    }
}
#[test]
fn random_roundtrip_and_malformed_inputs_are_bounded_and_do_not_panic() {
    let mut rng = StdRng::seed_from_u64(0xb2_9605);
    for _ in 0..512 {
        let (kid, ctr) = (rng.random(), rng.random());
        let base: [u8; 32] = rng.random();
        let (enc, dec) = keys(kid, &base);
        let mut pt = vec![0; rng.random_range(0..8192)];
        rng.fill_bytes(&mut pt);
        for layout in [VideoLayout::Vp9, VideoLayout::Vp8] {
            let clear = layout.clear_len(&pt);
            if clear.is_ok() {
                let wire = layout.seal(&enc, &mut SingleUseCounter::new(ctr), &pt).unwrap();
                assert_eq!(layout.open(&dec, &wire).unwrap(), pt);
            } else {
                assert!(layout.seal(&enc, &mut SingleUseCounter::new(ctr), &pt).is_err());
            }
            // Any random bit string is an adversarial parser input; opening must fail.
            assert!(layout.open(&dec, &pt).is_err());
        }
    }
}
#[test]
fn exact_bounds_and_prefix_only_frames() {
    let (enc, dec) = keys(0, &[1; 32]);
    for (layout, min) in [(VideoLayout::Vp9, 0), (VideoLayout::Vp8, 3), (VideoLayout::Vp8, 10)] {
        for length in [min, MAX_FRAME_BYTES] {
            let mut pt = vec![0; length];
            if layout == VideoLayout::Vp8 && min == 3 {
                pt[0] = 1;
            }
            let wire = layout.seal(&enc, &mut SingleUseCounter::new(0), &pt).unwrap();
            assert_eq!(layout.open(&dec, &wire).unwrap(), pt);
        }
        assert!(layout.seal(&enc, &mut SingleUseCounter::new(0), &vec![0; MAX_FRAME_BYTES + 1]).is_err());
        assert!(layout.parse(&vec![0; MAX_WIRE_BYTES + 1]).is_err());
    }
    for first in [0, 1] {
        let min = if first == 0 { 10 } else { 3 };
        for length in 0..min {
            assert!(VideoLayout::Vp8.seal(&enc, &mut SingleUseCounter::new(0), &vec![first; length]).is_err());
        }
    }
}
