//! Lenient decoding for non-structural DjVu strings (#524).
//!
//! DjVu itself never validated text encoding, and DjVuLibre happily writes
//! whatever the OS handed it — on Windows that is typically CP1252, so real
//! bundled documents carry bytes like `0x96`/`0x97` (en/em dash) in NAVM
//! bookmark titles, TXTz text layers, and METa values. Strict
//! `core::str::from_utf8` on those strings used to abort `Document::open`
//! for files whose every page decodes fine.
//!
//! Policy: non-structural strings (bookmark titles/URLs, text layer, metadata
//! values) are decoded leniently — valid UTF-8 passes through untouched;
//! anything else is decoded as Windows-1252, which recovers the intended
//! characters for the overwhelmingly common legacy case instead of scattering
//! U+FFFD. Structural parsing (chunk layout, BZZ, zone records) stays strict.

#[cfg(not(feature = "std"))]
use alloc::{borrow::Cow, string::String};
#[cfg(feature = "std")]
use std::borrow::Cow;

/// Decode bytes as UTF-8, decoding invalid byte runs as Windows-1252.
///
/// Works like [`String::from_utf8_lossy`], except invalid runs become their
/// CP1252 characters instead of U+FFFD. Valid UTF-8 input is returned
/// borrowed and byte-identical. Mixed input (UTF-8 text with stray CP1252
/// bytes — the common "DjVuLibre on Windows edited a UTF-8 file" shape) keeps
/// the UTF-8 parts intact and recovers the stray bytes. Never fails.
pub(crate) fn decode_lossy(bytes: &[u8]) -> Cow<'_, str> {
    let mut rest = match core::str::from_utf8(bytes) {
        Ok(s) => return Cow::Borrowed(s),
        Err(_) => bytes,
    };
    let mut out = String::with_capacity(bytes.len() + 8);
    loop {
        match core::str::from_utf8(rest) {
            Ok(s) => {
                out.push_str(s);
                break;
            }
            Err(e) => {
                let valid_len = e.valid_up_to();
                // Split point comes from the validator, so this sub-slice is
                // valid by construction.
                out.push_str(core::str::from_utf8(&rest[..valid_len]).unwrap_or(""));
                let after = &rest[valid_len..];
                // error_len() is None only at end-of-input (truncated char).
                let bad_len = e.error_len().unwrap_or(after.len()).min(after.len());
                for &b in &after[..bad_len] {
                    out.push(cp1252_char(b));
                }
                rest = &after[bad_len..];
            }
        }
    }
    Cow::Owned(out)
}

/// Convenience: owned-`String` form of [`decode_lossy`].
pub(crate) fn decode_lossy_string(bytes: &[u8]) -> String {
    // into_owned(), not to_string(): the latter re-allocates even when the
    // Cow is already Owned — i.e. exactly on the fallback path.
    decode_lossy(bytes).into_owned()
}

/// Map one Windows-1252 byte to its Unicode character.
///
/// `0x00..=0x7F` is ASCII and `0xA0..=0xFF` coincides with Latin-1 (= the
/// first 256 Unicode code points); only `0x80..=0x9F` needs a table. The
/// table matches the WHATWG `windows-1252` index (what browsers'
/// `TextDecoder` uses): the five bytes CP1252 leaves undefined (0x81, 0x8D,
/// 0x8F, 0x90, 0x9D) pass through as their C1 control code points, so the
/// decoding is total and standard-shaped.
fn cp1252_char(b: u8) -> char {
    const C1: [char; 32] = [
        '\u{20AC}', // 0x80 €
        '\u{0081}', // 0x81 (undefined in CP1252 — C1 pass-through per WHATWG)
        '\u{201A}', // 0x82 ‚
        '\u{0192}', // 0x83 ƒ
        '\u{201E}', // 0x84 „
        '\u{2026}', // 0x85 …
        '\u{2020}', // 0x86 †
        '\u{2021}', // 0x87 ‡
        '\u{02C6}', // 0x88 ˆ
        '\u{2030}', // 0x89 ‰
        '\u{0160}', // 0x8A Š
        '\u{2039}', // 0x8B ‹
        '\u{0152}', // 0x8C Œ
        '\u{008D}', // 0x8D (undefined — C1 pass-through)
        '\u{017D}', // 0x8E Ž
        '\u{008F}', // 0x8F (undefined — C1 pass-through)
        '\u{0090}', // 0x90 (undefined — C1 pass-through)
        '\u{2018}', // 0x91 '
        '\u{2019}', // 0x92 '
        '\u{201C}', // 0x93 "
        '\u{201D}', // 0x94 "
        '\u{2022}', // 0x95 •
        '\u{2013}', // 0x96 –
        '\u{2014}', // 0x97 —
        '\u{02DC}', // 0x98 ˜
        '\u{2122}', // 0x99 ™
        '\u{0161}', // 0x9A š
        '\u{203A}', // 0x9B ›
        '\u{0153}', // 0x9C œ
        '\u{009D}', // 0x9D (undefined — C1 pass-through)
        '\u{017E}', // 0x9E ž
        '\u{0178}', // 0x9F Ÿ
    ];
    match b {
        0x80..=0x9F => C1[(b - 0x80) as usize],
        _ => b as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_utf8_is_borrowed_unchanged() {
        let s = "многоязычный текст — with dashes";
        match decode_lossy(s.as_bytes()) {
            Cow::Borrowed(out) => assert_eq!(out, s),
            Cow::Owned(_) => panic!("valid UTF-8 must not be re-decoded"),
        }
    }

    #[test]
    fn cp1252_dashes_decode_to_unicode() {
        // "Chapter 1 \x96 Intro" — the exact #524 reproducer byte.
        let bytes = b"Chapter 1 \x96 Intro";
        assert_eq!(decode_lossy_string(bytes), "Chapter 1 – Intro");
        assert_eq!(decode_lossy_string(b"a\x97b"), "a\u{2014}b");
    }

    #[test]
    fn cp1252_quotes_euro_and_latin1() {
        assert_eq!(
            decode_lossy_string(b"\x93hi\x94 \x80 caf\xE9"),
            "\u{201C}hi\u{201D} \u{20AC} caf\u{E9}"
        );
    }

    #[test]
    fn undefined_cp1252_bytes_pass_through_as_c1_controls() {
        // WHATWG windows-1252 behavior (browsers' TextDecoder): the five
        // undefined bytes map to their C1 control code points, not U+FFFD.
        assert_eq!(
            decode_lossy_string(b"\x81\x8D\x8F\x90\x9D"),
            "\u{0081}\u{008D}\u{008F}\u{0090}\u{009D}"
        );
    }

    #[test]
    fn mixed_utf8_and_cp1252_keeps_utf8_runs_intact() {
        // UTF-8 Cyrillic with one stray CP1252 en-dash — the "DjVuLibre on
        // Windows edited a UTF-8 file" shape. The Cyrillic must survive.
        let mut bytes = "Глава ".as_bytes().to_vec();
        bytes.push(0x96);
        bytes.extend_from_slice(" один".as_bytes());
        assert_eq!(decode_lossy_string(&bytes), "Глава – один");
    }

    #[test]
    fn every_byte_value_decodes_without_panic() {
        let bytes: [u8; 256] = core::array::from_fn(|i| i as u8);
        let decoded = decode_lossy(&bytes);
        assert!(matches!(decoded, Cow::Owned(_)));
        // 0xC3 0xC4... aren't valid sequences alone; each invalid byte maps to
        // exactly one char here since no valid multi-byte runs exist in 0x80..
        assert!(decoded.chars().count() >= 128);
    }

    #[test]
    fn truncated_utf8_tail_falls_back() {
        // 0xD0 opens a 2-byte sequence that never completes (end of input) —
        // error_len() is None; must not panic or loop.
        assert_eq!(decode_lossy_string(b"ab\xD0"), "abÐ");
    }
}
