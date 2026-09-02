//! FictionBook 2 cover extraction (ported from DarkThumbs' fb.cpp):
//!   <coverpage><image l:href="#ID"/></coverpage>  ->  <binary id="ID">BASE64</binary>
//!
//! FB2 is frequently windows-1251, but this parser never needs to know that. Every
//! byte it actually inspects — the `<coverpage>`/`<binary>` structural tags, the
//! `href`/`id` attribute values, and the base64 payload itself — is ASCII, and every
//! encoding this format declares (windows-1251, KOI8-R, UTF-8, ...) is a strict
//! superset of ASCII in the 0x00-0x7F range. So the whole document is scanned as raw
//! bytes instead of being decoded through a charset table first: the id extracted
//! from `href` and the id matched inside `<binary id="...">` are compared byte-for-
//! byte from the SAME undecoded buffer, which is consistent (and correct) regardless
//! of what the prolog declares. This is why `looks_like_fb2` below already worked on
//! raw bytes even before this file stopped decoding — don't "fix" this by
//! reintroducing a charset decode, it would add nothing but risk (e.g. a lossy
//! UTF-8 re-encode could still shift/replace bytes) for content this parser never
//! actually reads as text.

use base64::Engine;

use super::util::{contains_ci, find};

/// True if `bytes` looks like a FictionBook XML document.
pub fn looks_like_fb2(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(2048)];
    contains_ci(head, b"<fictionbook")
}

pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    // The whole document is scanned byte-by-byte to parse it, so bound the input
    // like every other container path (zip/7z/rar all cap at MAX_COVER). Real
    // FB2 books are KB–low-MB; 64 MiB is very generous and keeps the transient
    // allocation sane. Oversized files just fall back to the default icon.
    if bytes.len() as u64 > super::MAX_COVER.saturating_mul(2) {
        return None;
    }
    let cover_id = coverpage_id(bytes)?;
    let b64 = binary_by_id(bytes, &cover_id)?;
    let cleaned: Vec<u8> = b64
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    let out = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .ok()?;
    (out.len() as u64 <= super::MAX_COVER).then_some(out)
}

/// Real FB2 binary ids are short identifiers (a handful of characters). Capping the
/// accepted href/id length here keeps `binary_by_id`'s needle search from ever being
/// handed an attacker-chosen multi-megabyte needle: `util::find`'s plain
/// `windows(needle.len())` scan costs O(needle.len()) per failed window, so a long
/// needle sharing a long common prefix with the haystack turns the whole lookup
/// O(n*m). Rejecting an oversized id here, before it reaches `binary_by_id`, keeps
/// the needle small regardless of how large the crafted href was.
const MAX_ID_LEN: usize = 256;

/// The binary id referenced by `<coverpage>`'s image href (leading '#'s stripped).
fn coverpage_id(bytes: &[u8]) -> Option<Vec<u8>> {
    let cp = find(bytes, b"<coverpage")?;
    let rest = bytes.get(cp..)?;
    let hp = find(rest, b"href=\"")? + 6;
    let he = find(rest.get(hp..)?, b"\"")? + hp;
    let mut id = rest.get(hp..he)?;
    while let Some(rest) = id.strip_prefix(b"#") {
        id = rest;
    }
    (!id.is_empty() && id.len() <= MAX_ID_LEN).then(|| id.to_vec())
}

/// Last index of a single byte in `hay`, or `None`.
fn rfind_byte(hay: &[u8], byte: u8) -> Option<usize> {
    hay.iter().rposition(|&b| b == byte)
}

/// The base64 payload of `<binary id="ID" ...>...</binary>`.
fn binary_by_id<'a>(bytes: &'a [u8], id: &[u8]) -> Option<&'a [u8]> {
    let mut needle = Vec::with_capacity(id.len() + 5);
    needle.extend_from_slice(b"id=\"");
    needle.extend_from_slice(id);
    needle.push(b'"');
    let mut from = 0usize;
    loop {
        let p = find(bytes.get(from..)?, &needle)? + from;
        let lt = rfind_byte(bytes.get(..p)?, b'<')?;
        if bytes.get(lt..)?.starts_with(b"<binary") {
            let gt = find(bytes.get(p..)?, b">")? + p + 1;
            let end = find(bytes.get(gt..)?, b"</binary>")? + gt;
            return bytes.get(gt..end);
        }
        from = p + needle.len();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a minimal FB2 document with the given prolog `encoding` label, a
    /// `<coverpage>` pointing at `id`, and a `<binary id="...">` holding `payload`
    /// base64-encoded. `title_bytes` are spliced into an unrelated `<book-title>`
    /// so a test can plant bytes that are not valid UTF-8 without touching the
    /// structural markup the parser actually reads.
    fn doc(encoding: &str, id: &[u8], title_bytes: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut d = Vec::new();
        d.extend_from_slice(
            format!("<?xml version=\"1.0\" encoding=\"{encoding}\"?>\n").as_bytes(),
        );
        d.extend_from_slice(b"<FictionBook><description><title-info><book-title>");
        d.extend_from_slice(title_bytes);
        d.extend_from_slice(b"</book-title>");
        d.extend_from_slice(b"<coverpage><image l:href=\"#");
        d.extend_from_slice(id);
        d.extend_from_slice(b"\"/></coverpage>");
        d.extend_from_slice(b"</title-info></description>");
        d.extend_from_slice(b"<body><section><p>text</p></section></body>");
        d.extend_from_slice(b"<binary id=\"");
        d.extend_from_slice(id);
        d.extend_from_slice(b"\" content-type=\"image/png\">");
        d.extend_from_slice(
            base64::engine::general_purpose::STANDARD
                .encode(payload)
                .as_bytes(),
        );
        d.extend_from_slice(b"</binary></FictionBook>");
        d
    }

    #[test]
    fn looks_like_fb2_matches_the_root_tag() {
        assert!(looks_like_fb2(b"<FictionBook><x/></FictionBook>"));
        assert!(!looks_like_fb2(b"<html></html>"));
    }

    #[test]
    fn extract_reads_the_referenced_binary() {
        let payload = b"plain-ascii-cover";
        let bytes = doc("utf-8", b"cover.png", b"Plain Title", payload);
        assert_eq!(extract(&bytes).as_deref(), Some(payload.as_slice()));
    }

    /// The dependency this file used to pull in (`encoding_rs`) existed to decode
    /// the WHOLE document before parsing it, on the theory that a windows-1251/
    /// KOI8-R declared FB2 (very common) needs real charset decoding to be parsed
    /// safely. It does not: the structural tags and the base64 payload are ASCII
    /// regardless of what the prolog declares, so raw bytes containing genuinely
    /// invalid UTF-8 elsewhere in the document (e.g. a Cyrillic title stored in a
    /// single-byte codepage) must not stop the cover from extracting. A naive
    /// dependency removal that tried `str::from_utf8(bytes).ok()?` instead of
    /// scanning raw bytes would return `None` here and fail this test.
    #[test]
    fn extract_survives_non_utf8_bytes_elsewhere_in_document() {
        let payload = b"cover-bytes-1";
        // 0xC0 followed by 0xE8 is not valid UTF-8 (0xC0 is a 2-byte lead byte;
        // 0xE8 is not a valid continuation byte for it).
        let title = [0xC0u8, 0xE8, 0xE2, b' ', 0xE2u8, 0xE5, 0xF1];
        let bytes = doc("windows-1251", b"cover.png", &title, payload);
        assert!(std::str::from_utf8(&bytes).is_err(), "test fixture setup");
        assert_eq!(extract(&bytes).as_deref(), Some(payload.as_slice()));
    }

    /// Same guard as above, but the raw non-UTF-8 bytes sit IN the id itself (both
    /// in the `href` and in the matching `<binary id="...">`), not in unrelated
    /// text. This is the case that would break if the two id occurrences were ever
    /// compared through two independent decode/re-encode passes instead of being
    /// matched byte-for-byte out of the same undecoded buffer.
    #[test]
    fn extract_matches_a_raw_non_utf8_id() {
        let payload = b"cover-bytes-2";
        // 0xEE (3-byte lead) followed by 0xE1 (not a valid continuation byte).
        let id = [0xEEu8, 0xE1, 0xEB];
        let bytes = doc("windows-1251", &id, b"Title", payload);
        assert!(std::str::from_utf8(&bytes).is_err(), "test fixture setup");
        assert_eq!(extract(&bytes).as_deref(), Some(payload.as_slice()));
    }

    #[test]
    fn extract_returns_none_without_a_coverpage() {
        let bytes = b"<FictionBook><description></description></FictionBook>".to_vec();
        assert_eq!(extract(&bytes), None);
    }

    #[test]
    fn extract_returns_none_when_the_binary_id_is_missing() {
        // href points at an id that has no matching <binary id="...">.
        let bytes =
            b"<FictionBook><coverpage><image l:href=\"#nope\"/></coverpage></FictionBook>".to_vec();
        assert_eq!(extract(&bytes), None);
    }

    #[test]
    fn extract_rejects_oversized_input() {
        let huge = vec![b'a'; (super::super::MAX_COVER * 2 + 1) as usize];
        assert_eq!(extract(&huge), None);
    }

    /// A crafted long href/id (sharing a long common prefix with a large document,
    /// so a naive substring scan would cost O(needle.len()) per failed window) must
    /// be rejected by `coverpage_id` itself, before `binary_by_id` ever builds a
    /// needle from it, and must not stall doing so.
    #[test]
    fn overlong_cover_id_is_rejected_quickly_not_scanned() {
        let long_id = vec![b'a'; MAX_ID_LEN + 1];
        // A large body sharing the same repeated-'a' prefix is exactly the shape
        // that makes an unbounded needle search quadratic.
        let filler = vec![b'a'; 4 * 1024 * 1024];
        let bytes = doc("utf-8", &long_id, &filler, b"cover-bytes");

        let start = std::time::Instant::now();
        let result = extract(&bytes);
        let elapsed = start.elapsed();

        assert_eq!(result, None);
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "extract took {elapsed:?} for an overlong id — looks like it wasn't rejected before the scan"
        );
    }
}
