//! Adobe InDesign `.indd` / `.indt` embedded preview thumbnail.
//!
//! InDesign writes the page preview as a base64-encoded JPEG inside its XMP packet,
//! in `<xmpGImg:image>…</xmpGImg:image>` elements. We scan for those literal tags,
//! base64-decode the content, and pick the largest JPEG that is actually WHOLE.
//!
//! **The tag pair is not a reliable delimiter, and that is the whole difficulty.**
//! An `.indd` is a block database, not a linear document: every save appends a fresh
//! XMP packet and leaves the earlier ones behind, half-overwritten by unrelated block
//! data. So a real file is littered with ORPHANED `<xmpGImg:image>` open tags whose
//! base64 stops dead a kilobyte in, and the "matching" `</xmpGImg:image>` the scanner
//! then finds can sit hundreds of KB later inside a completely different packet.
//! Stripping the non-base64 bytes out of that span — which is what this module used
//! to do — SPLICES unrelated file content into the middle of the stream. The result
//! still starts `FFD8FF`, still ends `FFD9`, and is the LARGEST candidate in the
//! file, so it won every tie-break; it decodes to a few correct rows of pixels
//! followed by flat grey. That is the "preview only renders the top strip" bug
//! reported from the field, and it reproduces on `test-corpus/sample.indd`, where
//! five of the seven elements are fragmented and only the two at the file's tail
//! (the live packet) are contiguous.
//!
//! Hence two rules, both load-bearing:
//!
//! 1. **The base64 run ends at the first byte that cannot be XMP text.** Binary is
//!    not noise to skip past, it is the proof that we walked off the end of the real
//!    element. Stop there and resume scanning from that point, so a good element
//!    living between an orphaned open tag and its far-away close tag is still found.
//! 2. **A candidate must be a COMPLETE JPEG** — `FFD8FF` at the front and the `FFD9`
//!    end-of-image marker at the back. Truncation is the one thing rule 1 cannot
//!    repair, and a truncated preview is exactly what draws as a half-finished tile.
//!
//! Preview presence depends on InDesign's "Save Preview Images with Documents"
//! setting; without it there is no element → None, and the shell shows the stock
//! Adobe icon (correct) rather than a broken tile. A raw binary JPEG is present in
//! some files but absent in others, so the XMP path is the reliable one.
//! Bounds-checked throughout (`panic = "abort"`).

use base64::{engine::general_purpose::STANDARD, Engine};

use super::util::find;

/// The 16-byte master GUID every `.indd` starts with (then ASCII "DOCUMENT").
const INDD_GUID: [u8; 16] = [
    0x06, 0x06, 0xED, 0xF5, 0xD8, 0x1D, 0x46, 0xE5, 0xBD, 0x31, 0xEF, 0xE7, 0xFE, 0x74, 0xB7, 0x1D,
];

const OPEN: &[u8] = b"<xmpGImg:image>";
const CLOSE: &[u8] = b"</xmpGImg:image>";
/// Cap on elements scanned (bomb guard).
const MAX_ELEMENTS: usize = 24;
/// Cap on one element's base64 run, sized so the DECODED preview stays under
/// [`super::MAX_COVER`] (base64 spends 4 chars per 3 bytes). Enforced *while*
/// scanning, so a hostile file can't drive the allocation before the check runs.
const MAX_B64: usize = (super::MAX_COVER as usize / 3) * 4 + 4;
/// Initial buffer for one element's cleaned base64. Real previews are 128–1024 px
/// JPEGs (a few KB up to ~1.5 MB); this avoids re-growing for the common case
/// without letting a multi-hundred-MB span dictate the allocation.
const B64_HINT: usize = 64 * 1024;

pub fn looks_like_indd(head: &[u8]) -> bool {
    head.starts_with(&INDD_GUID)
}

/// Extract the largest COMPLETE embedded JPEG preview, or None.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut best: Option<Vec<u8>> = None;
    let mut pos = 0usize;
    for _ in 0..MAX_ELEMENTS {
        let open = match find(bytes.get(pos..)?, OPEN) {
            Some(o) => pos + o + OPEN.len(),
            None => break,
        };
        let rest = bytes.get(open..)?;
        // The close tag bounds the element WHEN the element is intact. When it is not,
        // the nearest close tag belongs to some other packet, so treat it as an upper
        // bound only — `clean_b64` decides where the data really ended.
        let close = find(rest, CLOSE);
        let span = rest.get(..close.unwrap_or(rest.len()))?;

        let (b64, used) = clean_b64(span);

        // Advance. Consuming the whole span means the element was intact, so step over
        // its close tag; stopping early means we hit foreign bytes, and a REAL element
        // can still sit between here and that far-away close tag — resume from the stop
        // point instead of jumping over it. `open` always exceeds the previous `pos`, so
        // this makes progress either way.
        pos = match close {
            Some(c) if used == span.len() => open + c + CLOSE.len(),
            _ => open + used,
        };

        if let Some(jpeg) = decode_b64_jpeg(b64) {
            if best.as_ref().is_none_or(|b| jpeg.len() > b.len()) {
                best = Some(jpeg);
            }
        }
    }
    best
}

/// Collect one element's base64, returning `(cleaned, bytes consumed from `raw`)`.
///
/// Accepts the base64 alphabet, `=` padding, ASCII whitespace, and XML numeric
/// character references (InDesign wraps its lines with the literal entity `&#xA;`,
/// NOT a raw newline — decoding those stray `x`/`A` chars as data corrupts the
/// stream). Anything else STOPS the run: XMP is XML text, so a byte outside that
/// set means the element was truncated and we are reading unrelated block data.
/// `consumed < raw.len()` is the caller's signal that this happened.
fn clean_b64(raw: &[u8]) -> (Vec<u8>, usize) {
    let mut b64: Vec<u8> = Vec::with_capacity(raw.len().min(B64_HINT));
    let mut i = 0;
    while i < raw.len() {
        let c = raw[i];
        if c.is_ascii_alphanumeric() || c == b'+' || c == b'/' {
            if b64.len() >= MAX_B64 {
                break;
            }
            b64.push(c);
            i += 1;
        } else if c == b'=' || c.is_ascii_whitespace() {
            i += 1; // padding is re-derived below; whitespace is XML line wrapping
        } else if c == b'&' {
            match entity_len(&raw[i..]) {
                Some(n) => i += n,
                None => break, // a bare `&` is not valid XML text either
            }
        } else {
            break; // binary — we have left the XMP packet
        }
    }
    (b64, i)
}

/// Length of the XML numeric character reference at the start of `s` (`&#xA;`,
/// `&#10;`, …), or None if `s` doesn't begin with one. Bounded, so a `&#` followed
/// by megabytes of digits can't scan the rest of the file.
fn entity_len(s: &[u8]) -> Option<usize> {
    const MAX_ENTITY: usize = 10; // `&#x10FFFF;` is the longest legal form
    if !s.starts_with(b"&#") {
        return None;
    }
    let end = s
        .get(..MAX_ENTITY.min(s.len()))?
        .iter()
        .position(|&c| c == b';')?;
    s.get(2..end)?
        .iter()
        .all(|&c| c.is_ascii_hexdigit() || c == b'x' || c == b'X')
        .then_some(end + 1)
}

/// Re-pad and decode, accepting only a JPEG that is WHOLE — the `FFD8FF` start
/// marker AND the `FFD9` end-of-image marker. The EOI test is the truncation guard:
/// a half-written preview decodes to a few correct rows and then grey, which is
/// worse than no thumbnail at all because it looks like the file is damaged.
fn decode_b64_jpeg(mut b64: Vec<u8>) -> Option<Vec<u8>> {
    while !b64.len().is_multiple_of(4) {
        b64.push(b'=');
    }
    let jpeg = STANDARD.decode(&b64).ok()?;
    (jpeg.starts_with(&[0xFF, 0xD8, 0xFF]) && jpeg.ends_with(&[0xFF, 0xD9])).then_some(jpeg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn jpeg_of(px: u32, rgb: [u8; 3]) -> Vec<u8> {
        let mut b = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(px, px, image::Rgb(rgb)))
            .write_to(&mut Cursor::new(&mut b), image::ImageFormat::Jpeg)
            .unwrap();
        b
    }

    fn tiny_jpeg() -> Vec<u8> {
        jpeg_of(8, [10, 90, 200])
    }

    fn indd_with(body: &[u8]) -> Vec<u8> {
        let mut f = INDD_GUID.to_vec();
        f.extend_from_slice(b"DOCUMENT");
        f.extend_from_slice(body);
        f
    }

    fn element(jpeg: &[u8]) -> String {
        format!("<xmpGImg:image>{}</xmpGImg:image>", STANDARD.encode(jpeg))
    }

    #[test]
    fn detects_guid() {
        let mut f = INDD_GUID.to_vec();
        f.extend_from_slice(b"DOCUMENT....");
        assert!(looks_like_indd(&f));
        assert!(!looks_like_indd(b"not indesign"));
    }

    #[test]
    fn decodes_xmp_jpeg_with_entity_separators() {
        let jpeg = tiny_jpeg();
        // Base64 with the &#xA; entity injected mid-stream (InDesign's line wrap).
        let mut b64 = STANDARD.encode(&jpeg);
        let mid = b64.len() / 2;
        b64.insert_str(mid, "&#xA;");
        let doc = format!("<xmpGImg:image>{b64}</xmpGImg:image>");

        let got = extract(&indd_with(doc.as_bytes())).expect("should decode the embedded JPEG");
        assert_eq!(got, jpeg);
        assert!(image::load_from_memory(&got).is_ok());
    }

    #[test]
    fn tolerates_raw_newlines_and_decimal_entities() {
        let jpeg = tiny_jpeg();
        let mut b64 = STANDARD.encode(&jpeg);
        let third = b64.len() / 3;
        b64.insert_str(third, "\r\n");
        b64.insert_str(third * 2, "&#10;");
        let doc = format!("<xmpGImg:image>{b64}</xmpGImg:image>");
        assert_eq!(extract(&indd_with(doc.as_bytes())), Some(jpeg));
    }

    #[test]
    fn picks_largest_of_several() {
        let small = tiny_jpeg();
        let big = jpeg_of(64, [1, 2, 3]);
        let doc = format!("{}junk{}", element(&small), element(&big));
        let got = extract(&indd_with(doc.as_bytes())).expect("some preview");
        assert_eq!(got, big, "should pick the larger preview");
    }

    /// The field bug: a stale, half-overwritten XMP packet leaves an ORPHANED open tag
    /// whose base64 dies in binary, and the next close tag belongs to a *later* packet.
    /// The old scanner stripped the binary out of that whole span, spliced the two
    /// packets into one "JPEG" — larger than any real candidate, and still `FFD8FF` …
    /// `FFD9` — and shipped it, so Explorer drew a few rows and then grey. It also
    /// skipped the good element entirely, because that lived inside the span.
    #[test]
    fn ignores_fragmented_stale_packet_and_finds_the_live_one() {
        let good = jpeg_of(32, [200, 30, 30]);
        let orphan = STANDARD.encode(jpeg_of(64, [9, 9, 9]));

        let mut body = Vec::new();
        body.extend_from_slice(OPEN);
        // A packet cut off mid-base64 by the block store, then unrelated binary…
        body.extend_from_slice(&orphan.as_bytes()[..orphan.len() / 2]);
        body.extend_from_slice(&[0u8; 4096]);
        body.extend(std::iter::successors(Some(0u8), |b| Some(b.wrapping_add(7))).take(4096));
        // …then the live packet, whose close tag is the first one the orphan can see.
        body.extend_from_slice(element(&good).as_bytes());

        let got = extract(&indd_with(&body)).expect("the live packet's preview");
        assert_eq!(
            got, good,
            "must not splice the stale fragment into the stream"
        );
        assert!(image::load_from_memory(&got).is_ok());
    }

    #[test]
    fn rejects_a_truncated_preview_outright() {
        let jpeg = tiny_jpeg();
        let b64 = STANDARD.encode(&jpeg);
        // Whole element present, but the JPEG inside it is missing its tail.
        let doc = format!("<xmpGImg:image>{}</xmpGImg:image>", &b64[..b64.len() / 2]);
        assert_eq!(
            extract(&indd_with(doc.as_bytes())),
            None,
            "a half-drawn tile is worse than the stock icon"
        );
    }

    #[test]
    fn no_preview_element_is_none() {
        assert_eq!(extract(&indd_with(b"no xmp here at all")), None);
    }

    /// Every real `.indd` save in the corpus must yield a preview that FULLY decodes —
    /// the property the field bug violated. Skipped when the corpus isn't present
    /// (it is a sibling of the repo and CI never checks it out).
    #[test]
    fn corpus_samples_decode_completely() {
        for name in ["sample.indd", "sample.indt"] {
            let path = std::path::Path::new("../test-corpus").join(name);
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            assert!(looks_like_indd(&bytes), "{name} should sniff as .indd");
            let jpeg = extract(&bytes).unwrap_or_else(|| panic!("{name}: no preview extracted"));
            let img = image::load_from_memory(&jpeg)
                .unwrap_or_else(|e| panic!("{name}: preview does not decode: {e}"));
            assert!(img.width() > 0 && img.height() > 0, "{name}: empty preview");
        }
    }
}
