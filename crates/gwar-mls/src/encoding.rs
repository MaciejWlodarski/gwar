//! Strict text encodings shared by every Gwar statement: canonical decimals,
//! canonical base64url and netstring lists.

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};

pub(crate) fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

/// Canonical unpadded base64url of exactly `N` bytes.
pub(crate) fn fixed<const N: usize>(value: &str) -> Option<[u8; N]> {
    gwar_proto::decode_fixed(value)
}

/// A canonical decimal (no sign, no leading zeros) up to `MAX_SAFE_INTEGER`.
pub(crate) fn decimal(value: &str) -> Option<u64> {
    let canonical =
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) && (value == "0" || !value.starts_with('0'));
    let n: u64 = value.parse().ok().filter(|_| canonical)?;
    (n <= gwar_proto::MAX_SAFE_INTEGER as u64).then_some(n)
}

/// Netstrings as in `gwar_proto::canonical_message_body`: `{len}:{bytes},` each.
pub(crate) fn netstrings(fields: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for field in fields {
        out.extend_from_slice(field.len().to_string().as_bytes());
        out.push(b':');
        out.extend_from_slice(field);
        out.push(b',');
    }
    out
}

/// Exactly `count` canonical netstrings and nothing else.
pub(crate) fn parse_netstrings(mut input: &[u8], count: usize) -> Option<Vec<&[u8]>> {
    let mut fields = Vec::with_capacity(count);
    while !input.is_empty() {
        let colon = input.iter().position(|&b| b == b':')?;
        let len = usize::try_from(decimal(std::str::from_utf8(&input[..colon]).ok()?)?).ok()?;
        let rest = &input[colon + 1..];
        if rest.len() <= len || rest[len] != b',' {
            return None;
        }
        // More fields than expected: refused at once, without collecting them.
        if fields.len() == count {
            return None;
        }
        fields.push(&rest[..len]);
        input = &rest[len + 1..];
    }
    (fields.len() == count).then_some(fields)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimals_are_canonical() {
        assert_eq!(decimal("0"), Some(0));
        assert_eq!(decimal("42"), Some(42));
        for bad in ["", "01", "+1", "-1", " 1", "1.0", "9007199254740992"] {
            assert_eq!(decimal(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn netstrings_round_trip_and_reject_trailing_bytes() {
        let encoded = netstrings(&[b"a", b"", b"x,y:z"]);
        assert_eq!(encoded, b"1:a,0:,5:x,y:z,");
        assert_eq!(parse_netstrings(&encoded, 3).unwrap(), vec![&b"a"[..], b"", b"x,y:z"]);
        assert!(parse_netstrings(&encoded, 2).is_none());
        let mut trailing = encoded.clone();
        trailing.push(b'1');
        assert!(parse_netstrings(&trailing, 3).is_none());
        assert!(parse_netstrings(b"01:a,", 1).is_none());
        assert!(parse_netstrings(b"2:a,", 1).is_none());
        assert!(parse_netstrings(b"4294967297:a,", 1).is_none());
        assert!(parse_netstrings(b"9007199254740991:a,", 1).is_none());
    }
}
