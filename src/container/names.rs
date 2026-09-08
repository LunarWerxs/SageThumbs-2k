//! Shared archive entry-name decoding for the zip/RAR cover and listing extractors: turns raw,
//! possibly non-UTF-8 archive entry-name bytes into a displayable `String`.
//!
//! Archive formats disagree on how a non-ASCII entry name is stored. ZIP either sets the
//! UTF-8 flag (general-purpose bit 11) or falls back to CP437; RAR pre-5 stores the writer's
//! local code page verbatim with no flag at all. A Western European Windows box writing a
//! Japanese-named file into a RAR3 archive stores Shift-JIS bytes with nothing in the header
//! to say so, so a plain `from_utf8_lossy` renders mojibake instead of the real name.
//!
//! [`decode_entry_name`] is the one place this project decides what a raw entry-name byte
//! string means, so the zip and RAR extractors agree with each other and neither reinvents the
//! fallback chain.

use windows::Win32::Globalization::{MultiByteToWideChar, MB_ERR_INVALID_CHARS};

/// Entry names past this length are truncated before any decode attempt. They come straight
/// from attacker-controlled archive bytes, so nothing here allocates or converts proportional
/// to an unbounded length.
const MAX_NAME_BYTES: usize = 4096;

/// Windows code page for Shift-JIS.
const CP_SHIFT_JIS: u32 = 932;
/// Windows code page for CP437 (the IBM PC / MS-DOS OEM code page - ZIP's own default for an
/// unflagged name).
const CP_437: u32 = 437;

/// Decode a raw archive entry-name byte string into a displayable name.
///
/// `utf8_flagged` is the archive's own claim that `bytes` are UTF-8 (ZIP general-purpose bit
/// 11; a caller with no such flag, like RAR, passes `false`). The chain, in order:
/// 1. `bytes` are already valid UTF-8 - checked first, so a genuinely UTF-8 name (flagged or
///    not) is always taken as-is and never reinterpreted as anything else.
/// 2. `bytes` contain a Shift-JIS lead byte (`0x81..=0x9F` or `0xE0..=0xFC`) and
///    `MultiByteToWideChar(932, ..)` accepts the WHOLE name - real CP437/Latin-1 text
///    essentially never round-trips as valid Shift-JIS by chance, so this only fires on an
///    actual Shift-JIS name.
/// 3. CP437 (ZIP/DOS's own default for an unflagged name) - every byte value maps to some
///    character in this code page, so this step accepts anything step 2 didn't.
/// 4. Lossy UTF-8, as the final catch-all (empty input, or a decoder call that itself failed).
///
/// Steps 2 and 3 are skipped when `utf8_flagged` is set and the bytes fail UTF-8 validation:
/// the archive already told us the encoding, so a corrupt flagged name goes straight to the
/// lossy catch-all instead of being guessed at as a different encoding entirely.
///
/// Bounded: names over [`MAX_NAME_BYTES`] are truncated before any conversion runs, since the
/// bytes are attacker-controlled archive content.
pub(crate) fn decode_entry_name(bytes: &[u8], utf8_flagged: bool) -> String {
    let bytes = if bytes.len() > MAX_NAME_BYTES {
        &bytes[..MAX_NAME_BYTES]
    } else {
        bytes
    };

    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    if !utf8_flagged {
        if looks_like_shift_jis(bytes) {
            if let Some(s) = decode_codepage(bytes, CP_SHIFT_JIS, true) {
                return s;
            }
        }
        if let Some(s) = decode_codepage(bytes, CP_437, false) {
            return s;
        }
    }
    String::from_utf8_lossy(bytes).into_owned()
}

/// Does `bytes` contain a Shift-JIS lead byte (`0x81..=0x9F` or `0xE0..=0xFC`)? A cheap
/// presence check only - `MultiByteToWideChar` with `MB_ERR_INVALID_CHARS` is what actually
/// validates the whole name; this just decides whether trying Shift-JIS is worth the call.
fn looks_like_shift_jis(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .any(|&b| (0x81..=0x9F).contains(&b) || (0xE0..=0xFC).contains(&b))
}

/// Decode `bytes` with Windows code page `cp`. With `strict`, any byte sequence the code page
/// can't map makes this return `None` (`MB_ERR_INVALID_CHARS`); without it, unmappable bytes
/// become the code page's default replacement character.
fn decode_codepage(bytes: &[u8], cp: u32, strict: bool) -> Option<String> {
    if bytes.is_empty() {
        return Some(String::new());
    }
    let flags = if strict {
        MB_ERR_INVALID_CHARS
    } else {
        Default::default()
    };
    let n = unsafe { MultiByteToWideChar(cp, flags, bytes, None) };
    if n <= 0 {
        return None;
    }
    let mut buf = vec![0u16; n as usize];
    let written = unsafe { MultiByteToWideChar(cp, flags, bytes, Some(&mut buf)) };
    if written <= 0 {
        return None;
    }
    buf.truncate(written as usize);
    Some(String::from_utf16_lossy(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_name_round_trips() {
        assert_eq!(decode_entry_name(b"readme.txt", false), "readme.txt");
    }

    #[test]
    fn valid_utf8_name_round_trips() {
        let name = "cafe-\u{65e5}\u{672c}\u{8a9e}.png";
        assert_eq!(decode_entry_name(name.as_bytes(), true), name);
    }

    /// `0x93 0xFA 0x96 0x7B` is a well-known Shift-JIS encoding of a two-kanji word. Verify
    /// the expected text against the SAME Win32 API `decode_entry_name` itself calls, rather
    /// than a hand-typed literal, so this test can't silently assert the wrong string - then
    /// separately confirm that verified text really is the two kanji it should be.
    #[test]
    fn shift_jis_bytes_decode_to_the_expected_kanji() {
        let bytes = [0x93, 0xFA, 0x96, 0x7B];
        let expected = decode_codepage(&bytes, CP_SHIFT_JIS, true)
            .expect("code page 932 must accept this well-formed Shift-JIS pair");
        assert_eq!(decode_entry_name(&bytes, false), expected);
        assert_eq!(expected, "\u{65e5}\u{672c}");
    }

    /// A lone `0x82` is not valid UTF-8, and paired with the following `.` (0x2E, not a legal
    /// Shift-JIS trail byte) it is not valid Shift-JIS either, so this exercises the CP437
    /// fallback. CP437 maps `0x82` to e-acute.
    #[test]
    fn cp437_byte_decodes_to_the_right_accented_char() {
        let bytes = [b'r', 0x82, b'.', b't', b'x', b't'];
        assert_eq!(decode_entry_name(&bytes, false), "r\u{e9}.txt");
    }

    #[test]
    fn oversized_name_is_bounded_before_conversion() {
        let long = vec![b'a'; MAX_NAME_BYTES + 500];
        let decoded = decode_entry_name(&long, false);
        assert_eq!(decoded.len(), MAX_NAME_BYTES);
        assert!(decoded.chars().all(|c| c == 'a'));
    }

    /// A UTF-8-flagged name with invalid UTF-8 bytes must not be reinterpreted as Shift-JIS or
    /// CP437 - the archive already claimed an encoding, so a corrupt flagged name falls straight
    /// to the lossy catch-all instead of guessing a different one.
    #[test]
    fn utf8_flagged_invalid_bytes_skip_the_codepage_fallbacks() {
        let bytes = [0xFF, 0xFE, b'x'];
        assert_eq!(
            decode_entry_name(&bytes, true),
            String::from_utf8_lossy(&bytes)
        );
    }
}
