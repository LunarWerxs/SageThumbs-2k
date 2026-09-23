//! Fixed-width hex, in one place.
//!
//! The byte-wise decoder below existed twice - `update::verify::parse_sig_hex` for a downloaded
//! `.sig` asset, and a generic `parse_hex<N>` in the `update-sign` release example, whose own
//! comment said it was "the same shape as" the other one. Two hand-rolled codecs on either side
//! of a SIGNATURE check is the worst place in this repo for them to drift, so there is one now.
//!
//! Byte-wise on purpose: these read untrusted input (a downloaded signature, an environment
//! variable), and indexing a `&str` by byte offset is how a multi-byte character turns a
//! malformed file into a panic. `is_ascii()` is checked first and every lookup goes through the
//! `u8`, so no input can land mid-character.

/// `N * 2` hex characters -> `N` bytes. `None` on anything else: wrong length, non-ASCII, or a
/// character that is not a hex digit.
///
/// Whitespace is NOT trimmed, because the callers disagree about whether that is allowed - a
/// `.sig` asset is taken exactly as downloaded, an environment variable is trimmed by its
/// caller first - and a decoder that silently accepted a trailing newline for both would make
/// the strict caller's rule unenforceable.
pub fn decode<const N: usize>(s: &str) -> Option<[u8; N]> {
    let bytes = s.as_bytes();
    if bytes.len() != N * 2 || !bytes.is_ascii() {
        return None;
    }
    let mut out = [0u8; N];
    for i in 0..N {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Bytes -> lower-case hex, no separators. The inverse of [`decode`].
pub fn encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_round_trips_encode() {
        let raw = [0x00u8, 0x0f, 0x10, 0xff, 0xa5];
        let text = encode(&raw);
        assert_eq!(text, "000f10ffa5");
        assert_eq!(decode::<5>(&text), Some(raw));
    }

    #[test]
    fn decode_refuses_anything_but_exactly_n_pairs_of_hex() {
        assert_eq!(decode::<2>("abcd"), Some([0xab, 0xcd]));
        assert_eq!(decode::<2>("abc"), None, "odd length");
        assert_eq!(decode::<2>("abcdef"), None, "too long");
        assert_eq!(
            decode::<2>("abcd\n"),
            None,
            "not trimmed on the callers' behalf"
        );
        assert_eq!(decode::<2>("abzz"), None, "not hex");
        assert_eq!(
            decode::<2>("ABCD"),
            Some([0xab, 0xcd]),
            "upper case is hex too"
        );
    }

    /// The reason this is byte-wise: a `&str` of the right BYTE length can still be made of
    /// multi-byte characters, and slicing one by byte offset panics. The ASCII gate must run
    /// before any indexing, so this must return `None` rather than abort the shell.
    #[test]
    fn decode_refuses_multibyte_input_without_panicking() {
        assert_eq!(decode::<1>("é"), None, "two bytes, one character");
        assert_eq!(decode::<3>("日本"), None);
    }
}
