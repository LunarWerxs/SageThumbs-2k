//! Percent-encoding (RFC 3986), in one place: the encoder for form and query values the app
//! sends, and the decoder for an OAuth callback's query and an EPUB's hrefs.
//!
//! Byte-wise on purpose, like [`crate::hex`]: the decoder reads untrusted input (a page that
//! called our loopback listener, a book's markup), and a `%` at the very end, or before a
//! multi-byte character, must stay literal rather than index past the input or split a
//! character.

const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

/// Percent-encode `s`, keeping only the RFC 3986 unreserved set (`A-Z a-z 0-9 - . _ ~`).
/// Everything else, `&` `=` `:` `/` space and every byte of a non-ASCII character, becomes
/// `%XX` (upper-case hex), so a value cannot smuggle a second field into a query or form body.
pub fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push('%');
            out.push(char::from(HEX_UPPER[usize::from(b >> 4)]));
            out.push(char::from(HEX_UPPER[usize::from(b & 0x0F)]));
        }
    }
    out
}

/// Decode every `%XX` (either case of hex) in `s`. A `%` not followed by two hex digits stays
/// as it is, `+` stays `+` (this is not form decoding), and bytes that do not make UTF-8 become
/// U+FFFD.
pub fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'%')
            .then(|| Some((hex_digit(bytes.get(i + 1))?, hex_digit(bytes.get(i + 2))?)))
            .flatten();
        match escaped {
            Some((hi, lo)) => {
                out.push((hi << 4) | lo);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_digit(b: Option<&u8>) -> Option<u8> {
    (char::from(*b?)).to_digit(16).map(|d| d as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the callers' own tests never send: a stray `%` (at the end, before one digit,
    /// before non-hex, before a multi-byte character) stays literal; either hex case decodes;
    /// `+` is not a space; escaped bytes that are not UTF-8 become U+FFFD instead of failing.
    #[test]
    fn decode_keeps_malformed_escapes_and_never_splits_a_character() {
        for (input, want) in [
            ("a%2Fb%2fc", "a/b/c"),
            ("100%", "100%"),
            ("%4", "%4"),
            ("%zz%", "%zz%"),
            ("%é", "%é"),
            ("a+b", "a+b"),
            ("caf%C3%A9", "café"),
            ("%FF%FE", "\u{FFFD}\u{FFFD}"),
            ("", ""),
        ] {
            assert_eq!(decode(input), want, "decode({input:?})");
        }
    }

    /// The unreserved set survives as is, every other byte goes out as upper-case `%XX`,
    /// non-ASCII as its UTF-8 bytes, and decoding gives the original back.
    #[test]
    fn encode_escapes_all_but_the_unreserved_set_and_round_trips() {
        for (input, want) in [
            ("AZaz09-._~", "AZaz09-._~"),
            ("a&b=c d", "a%26b%3Dc%20d"),
            (
                "http://127.0.0.1:52100/x",
                "http%3A%2F%2F127.0.0.1%3A52100%2Fx",
            ),
            ("é+%", "%C3%A9%2B%25"),
        ] {
            assert_eq!(encode(input), want, "encode({input:?})");
            assert_eq!(decode(&encode(input)), input, "round trip of {input:?}");
        }
    }
}
