// SPDX-License-Identifier: AGPL-3.0-only
//! Lower-case hex, the text form of pins, tokens, proofs, nonces and launch secrets.

use zeroize::Zeroizing;

const DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Lower-case hex of `bytes`, in a buffer that wipes itself.
pub(crate) fn encode(bytes: &[u8]) -> Zeroizing<String> {
    let mut out = Zeroizing::new(String::with_capacity(bytes.len() * 2));
    for b in bytes {
        out.push(char::from(DIGITS[usize::from(b >> 4)]));
        out.push(char::from(DIGITS[usize::from(b & 0x0f)]));
    }
    out
}

/// Decodes exactly `N` bytes of hex (either case); anything else is `None`.
pub(crate) fn decode<const N: usize>(text: &str) -> Option<Zeroizing<[u8; N]>> {
    let text = text.as_bytes();
    if text.len() != N * 2 {
        return None;
    }
    let mut out = Zeroizing::new([0u8; N]);
    for (slot, pair) in out.iter_mut().zip(text.as_chunks::<2>().0) {
        *slot = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Some(out)
}

fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_refusals() {
        assert_eq!(encode(&[0x00, 0xab, 0xff]).as_str(), "00abff");
        assert_eq!(*decode::<3>("00ABff").unwrap(), [0x00, 0xab, 0xff]);
        assert!(decode::<3>("00abf").is_none());
        assert!(decode::<3>("00abfff0").is_none());
        assert!(decode::<1>("0g").is_none());
        assert!(decode::<1>("+1").is_none());
    }
}
