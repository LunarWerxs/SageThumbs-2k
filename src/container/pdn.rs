//! **Paint.NET** `.pdn` — lift the flattened preview Paint.NET already saved.
//!
//! A `.pdn` is Paint.NET's internal object graph written out with .NET
//! serialization, which we have no business trying to parse. We do not have to:
//! the file opens with a small XML preamble that carries a **base64-encoded PNG**
//! of the flattened image, which is exactly the composite a thumbnail wants.
//! So this is a header read plus a base64 decode, and the serialized layer data
//! after the preamble is never touched. Same deal as PSD resource 1036 or the
//! `.kra` merged image.
//!
//! Layout, verified 2026-08-01 against five real files from `addisonElliott/pypdn`
//! (`tests/data`), spanning Paint.NET **3.510** through **4.21**:
//!
//! ```text
//! offset 0   "PDN3"                      4-byte ASCII magic
//! offset 4   XML length                  24-bit LITTLE-endian, so <= 16 MiB by construction
//! offset 7   <pdnImage …>…</pdnImage>    the preamble, UTF-8
//!            …then the .NET-serialized document (gzip 1F 8B, or 00 01 uncompressed)
//! ```
//!
//! The preview lives in `<custom><thumb png="…base64…"/></custom>`. All five
//! samples carried one, at 256x192 for an 800x600 document, so Paint.NET caps the
//! long edge at 256 and preserves aspect. The 3.510 file has it too, so this is
//! not a modern-only feature. Files saved WITHOUT a thumb are still possible, and
//! they simply keep the stock icon rather than falling through to a tier that
//! would try to deserialize .NET objects.
//!
//! Note the base64 alphabet (`A-Za-z0-9+/=`) contains nothing XML would escape, so
//! the attribute never needs entity decoding. Do not "fix" that by adding an XML
//! parser; the whole point is that this stays a bounded byte scan.

use super::util;

/// The largest base64 attribute we will even attempt to decode. Observed real
/// previews are 686 B to 10.4 KB of PNG (roughly 1-14 KB of base64), so this is
/// ~300x the worst real case and still refuses a hostile multi-megabyte
/// attribute outright. The 24-bit length field already bounds the preamble to
/// 16 MiB; this bounds the allocation we do inside it.
const MAX_THUMB_BASE64: usize = 4 * 1024 * 1024;

/// Base64 expands 3 bytes into 4, so the decode of a capped attribute is capped too.
/// Asserting that here, at compile time, is worth more than a runtime `MAX_COVER`
/// check that the cap above makes unreachable: dead branches cannot be tested and
/// rot silently. If anyone raises `MAX_THUMB_BASE64` past the container budget, this
/// stops the build instead of quietly letting a bigger cover through.
const _: () = assert!((MAX_THUMB_BASE64 / 4 * 3) as u64 <= super::MAX_COVER);

/// Paint.NET 3.x and later. (Older `PDN2`-era files use a different container and
/// are not claimed here — nothing to test against, so nothing asserted.)
pub fn looks_like_pdn(b: &[u8]) -> bool {
    b.starts_with(b"PDN3")
}

/// Carve the embedded preview PNG, or `None` when the file has no usable thumb.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !looks_like_pdn(bytes) {
        return None;
    }
    // 24-bit little-endian preamble length. Max value is 0xFF_FFFF, so this can
    // never overflow the usize add below on any target we build for.
    let len = u32::from(*bytes.get(4)?)
        | (u32::from(*bytes.get(5)?) << 8)
        | (u32::from(*bytes.get(6)?) << 16);
    let len = len as usize;
    // A declared length is attacker-controlled: it must actually fit the file.
    let xml = bytes.get(7..7usize.checked_add(len)?)?;

    // Byte scan rather than a UTF-8 conversion: a malformed preamble should be a
    // miss, not a lossy multi-megabyte String allocation.
    let key = b"<thumb png=\"";
    let start = util::find(xml, key)?.checked_add(key.len())?;
    let rest = xml.get(start..)?;
    let end = rest.iter().position(|&c| c == b'"')?;
    let b64 = rest.get(..end)?;
    if b64.len() > MAX_THUMB_BASE64 {
        return None;
    }

    // Paint.NET writes the attribute on one line, but tolerate a writer that wraps
    // it. Whitespace is the only thing worth stripping: any other stray byte is not
    // base64 and should fail the decode rather than be silently dropped.
    let cleaned: Vec<u8> = b64
        .iter()
        .copied()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();

    use base64::Engine as _;
    let png = base64::engine::general_purpose::STANDARD
        .decode(cleaned)
        .ok()?;
    // No MAX_COVER check here on purpose: the const assertion above already proves
    // the decode cannot exceed that budget, so a runtime test would be a dead branch.
    // Never hand on bytes we have not sniffed: the attribute could decode to
    // anything at all, and this output goes straight into the decode tiers.
    util::decodable_image(png)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1x1 PNG, the smallest thing `decodable_image` will accept.
    fn tiny_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([9, 40, 200, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .expect("encode test png");
        out.into_inner()
    }

    /// Build a synthetic `.pdn` with the given preamble body.
    fn pdn_with_xml(xml: &str) -> Vec<u8> {
        let xml = xml.as_bytes();
        let mut out = Vec::from(*b"PDN3");
        let len = xml.len();
        out.push((len & 0xFF) as u8);
        out.push(((len >> 8) & 0xFF) as u8);
        out.push(((len >> 16) & 0xFF) as u8);
        out.extend_from_slice(xml);
        // Stand-in for the .NET-serialized body; the extractor must never read it.
        out.extend_from_slice(&[0x1F, 0x8B, 0x08, 0x00, 0xDE, 0xAD, 0xBE, 0xEF]);
        out
    }

    fn pdn_with_thumb(png: &[u8]) -> Vec<u8> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png);
        pdn_with_xml(&format!(
            "<pdnImage width=\"800\" height=\"600\" layers=\"1\"><custom><thumb png=\"{b64}\" /></custom></pdnImage>"
        ))
    }

    #[test]
    fn extracts_the_embedded_preview() {
        let png = tiny_png();
        let got = extract(&pdn_with_thumb(&png)).expect("thumb extracted");
        assert_eq!(got, png);
    }

    #[test]
    fn rejects_a_foreign_magic() {
        let mut file = pdn_with_thumb(&tiny_png());
        file[..4].copy_from_slice(b"PDN2");
        assert!(extract(&file).is_none());
        assert!(extract(b"8BPS").is_none());
        assert!(extract(b"").is_none());
    }

    #[test]
    fn declared_length_past_the_end_is_a_miss_not_a_panic() {
        let mut file = pdn_with_thumb(&tiny_png());
        file[4] = 0xFF;
        file[5] = 0xFF;
        file[6] = 0xFF;
        assert!(extract(&file).is_none());
    }

    #[test]
    fn truncation_is_a_clean_miss_up_to_the_end_of_the_preamble() {
        let full = pdn_with_thumb(&tiny_png());
        let declared =
            usize::from(full[4]) | usize::from(full[5]) << 8 | usize::from(full[6]) << 16;
        let preamble_end = 7 + declared;
        // Every prefix that cuts into the preamble must miss cleanly, never panic.
        for cut in 0..preamble_end {
            assert!(
                extract(&full[..cut]).is_none(),
                "prefix of {cut} bytes cuts the preamble and must not extract"
            );
        }
        // And the moment the preamble is complete it must work, WITHOUT the
        // .NET-serialized body that follows. That is the property that keeps this
        // extractor off the serialized document entirely, so assert it rather than
        // treating it as an accident.
        assert!(
            extract(&full[..preamble_end]).is_some(),
            "the preamble alone must be enough"
        );
    }

    #[test]
    fn a_file_without_a_thumb_is_a_clean_miss() {
        let file = pdn_with_xml("<pdnImage width=\"800\" height=\"600\" layers=\"14\"></pdnImage>");
        assert!(extract(&file).is_none());
    }

    #[test]
    fn unterminated_attribute_is_a_miss() {
        let file = pdn_with_xml("<pdnImage><custom><thumb png=\"iVBORw0KGgo");
        assert!(extract(&file).is_none());
    }

    #[test]
    fn non_image_payload_is_refused() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(b"this is not a picture at all");
        let file = pdn_with_xml(&format!(
            "<pdnImage><custom><thumb png=\"{b64}\"/></custom></pdnImage>"
        ));
        assert!(
            extract(&file).is_none(),
            "a decodable base64 blob that is not an image must not reach the decode tiers"
        );
    }

    #[test]
    fn invalid_base64_is_a_miss() {
        let file = pdn_with_xml(
            "<pdnImage><custom><thumb png=\"!!!! not base64 !!!!\"/></custom></pdnImage>",
        );
        assert!(extract(&file).is_none());
    }

    #[test]
    fn oversized_attribute_is_refused_before_decoding() {
        let huge = "A".repeat(MAX_THUMB_BASE64 + 4);
        let file = pdn_with_xml(&format!(
            "<pdnImage><custom><thumb png=\"{huge}\"/></custom></pdnImage>"
        ));
        assert!(extract(&file).is_none());
    }

    #[test]
    fn tolerates_a_wrapped_attribute() {
        let png = tiny_png();
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let wrapped: String = b64
            .as_bytes()
            .chunks(8)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join("\n  ");
        let file = pdn_with_xml(&format!(
            "<pdnImage><custom><thumb png=\"{wrapped}\"/></custom></pdnImage>"
        ));
        assert_eq!(extract(&file).as_deref(), Some(png.as_slice()));
    }
}
