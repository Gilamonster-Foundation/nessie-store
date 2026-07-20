//! Minimal lowercase-hex codec, shared by the self-describing byte-newtypes
//! ([`crate::Digest`], [`crate::SignerId`], [`crate::Signature`]) so their plain-text
//! wire form has a single implementation. Dependency-free by design.

const HEX: &[u8; 16] = b"0123456789abcdef";

/// Encode bytes as lowercase hex.
pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

fn val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decode lowercase/uppercase hex. `None` if the length is odd or any character
/// is not a hex digit.
pub(crate) fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.as_bytes();
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    for pair in s.chunks_exact(2) {
        out.push((val(pair[0])? << 4) | val(pair[1])?);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips() {
        let b = [0x00u8, 0x1e, 0xff, 0xa5];
        assert_eq!(encode(&b), "001effa5");
        assert_eq!(decode("001effa5").unwrap(), b);
        assert_eq!(decode("001EFFA5").unwrap(), b); // uppercase accepted
    }

    #[test]
    fn rejects_odd_length_and_non_hex() {
        assert!(decode("abc").is_none());
        assert!(decode("zz").is_none());
    }
}
