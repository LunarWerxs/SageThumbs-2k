//! Encapsulated PostScript (`.eps`) embedded preview.
//!
//! Rendering real PostScript needs Ghostscript (not bundled by design), but the
//! common **DOS-EPS binary** flavor (what Adobe/Corel
//! tools export for Windows) wraps the PostScript in a 30-byte header that
//! carries offsets to a baked-in **TIFF** (or WMF) screen preview. We slice the
//! TIFF out and let the normal image tiers decode it — same trick as the PSD
//! resource-1036 thumbnail, zero new decode code.
//!
//! Plain-text EPS (`%!PS-Adobe…`) may carry an EPSI greyscale preview or a
//! Photoshop Image Resources run. We decode those embedded previews only; we
//! never interpret PostScript. All reads here are bounds-checked slices.

/// DOS-EPS binary-header magic.
const MAGIC: [u8; 4] = [0xC5, 0xD0, 0xD3, 0xC6];

use image::{DynamicImage, GrayImage};

use super::{psd, util::le32, CoverOut, MAX_COVER};

/// EPS previews conventionally live at the head; never scan an arbitrary huge EPS.
const ASCII_SCAN_MAX: usize = 1 << 20;

/// True for either DOS-EPS binary framing or plain PostScript/EPS bytes.
///
/// The decoder uses this as a terminal safety classification after the embedded
/// preview extractor has had its chance. A shell stream may be nameless, so the
/// rule must be content-based rather than relying on a `.eps` extension.
pub(crate) fn is_eps(bytes: &[u8]) -> bool {
    bytes.starts_with(&MAGIC) || bytes.starts_with(b"%!PS")
}

/// Extract the embedded TIFF preview from a DOS-EPS, or None.
///
/// Header layout (all little-endian u32 pairs after the magic):
/// PS (offset @4, len @8) · WMF (@12, @16) · TIFF (@20, @24) · checksum @28.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    if !bytes.starts_with(&MAGIC) {
        return None;
    }
    let off = le32(bytes, 20)? as usize;
    let len = le32(bytes, 24)? as usize;
    if off == 0 || len < 8 {
        return None; // no TIFF preview (WMF-only or bare PS) — we can't draw WMF
    }
    // Bound the preview we hand back (shared CBXMEM cap): the declared length is
    // attacker-controlled and the TIFF is decoded downstream under panic=abort.
    if len as u64 > crate::container::MAX_COVER {
        return None;
    }
    let tiff = bytes.get(off..off.checked_add(len)?)?;
    // Sanity: a real TIFF starts "II*\0" (LE) or "MM\0*" (BE).
    if tiff.starts_with(b"II\x2A\x00") || tiff.starts_with(b"MM\x00\x2A") {
        Some(tiff.to_vec())
    } else {
        None
    }
}

/// Extract an embedded preview from a plain-text EPS, without rendering PostScript.
pub fn extract_ascii_preview(bytes: &[u8]) -> Option<CoverOut> {
    if !bytes.starts_with(b"%!PS") {
        return None;
    }
    let head = &bytes[..bytes.len().min(ASCII_SCAN_MAX)];
    epsi_preview(head)
        .map(CoverOut::Image)
        .or_else(|| photoshop_preview(head).map(CoverOut::Bytes))
}

fn epsi_preview(bytes: &[u8]) -> Option<DynamicImage> {
    let (header, mut rest) = find_comment(bytes, b"%%BeginPreview:")?;
    let mut fields = std::str::from_utf8(header).ok()?.split_ascii_whitespace();
    let width = fields.next()?.parse::<u32>().ok()?;
    let height = fields.next()?.parse::<u32>().ok()?;
    let depth = fields.next()?.parse::<u32>().ok()?;
    let lines = fields.next()?.parse::<u32>().ok()?;
    if fields.next().is_some()
        || !matches!(depth, 1 | 2 | 4 | 8)
        || width == 0
        || height == 0
        || lines == 0
        || width > crate::decode::limits::MAX_DIM
        || height > crate::decode::limits::MAX_DIM
    {
        return None;
    }
    let pixels = (width as u64).checked_mul(height as u64)?;
    if pixels > MAX_COVER {
        return None;
    }
    let row_bytes = ((width as usize).checked_mul(depth as usize)?).checked_add(7)? / 8;
    let packed_len = row_bytes.checked_mul(height as usize)?;
    // A legal EPSI payload must fit in the bounded head scan as well.
    if packed_len > ASCII_SCAN_MAX / 2 {
        return None;
    }
    let mut packed = Vec::with_capacity(packed_len);
    for _ in 0..lines {
        let (line, next) = take_line(rest);
        rest = next;
        append_hex(comment_payload(line)?, &mut packed, packed_len)?;
    }
    if packed.len() != packed_len {
        return None;
    }
    let mut grey = Vec::with_capacity(pixels as usize);
    // EPSI samples run from the lower-left upward, while image buffers run from
    // the upper-left downward. Reverse the packed rows as we unpack them.
    for row in packed.chunks_exact(row_bytes).rev() {
        unpack_row(row, width as usize, depth, &mut grey)?;
    }
    // Skip blank lines before the terminator. ImageMagick's `epi:` coder writes one, and this
    // check used to demand `%%EndPreview` on the VERY next line - so a preview whose 4800 bytes
    // had all been read correctly was thrown away on the strength of an empty line, and every
    // EPS that tool produces silently had no thumbnail. The corpus sample passed throughout
    // because Adobe does not write the blank line, which is exactly how it went unnoticed.
    //
    // Still terminator-checked rather than dropped: reaching `%%EndPreview` is what proves the
    // hex ran to the end of a real preview instead of us having stopped in the middle of one.
    let mut end;
    loop {
        let (line, next) = take_line(rest);
        rest = next;
        end = line;
        if !end.trim_ascii().is_empty() || rest.is_empty() {
            break;
        }
    }
    if end != b"%%EndPreview" || grey.len() as u64 != pixels {
        return None;
    }
    GrayImage::from_raw(width, height, grey).map(DynamicImage::ImageLuma8)
}

fn photoshop_preview(bytes: &[u8]) -> Option<Vec<u8>> {
    let (header, mut rest) = find_comment(bytes, b"%%BeginPhotoshop:")?;
    let declared = std::str::from_utf8(header.trim_ascii())
        .ok()?
        .parse::<usize>()
        .ok()?;
    if declared == 0 || declared as u64 > MAX_COVER || declared > ASCII_SCAN_MAX / 2 {
        return None;
    }
    let mut resource = Vec::with_capacity(declared);
    while resource.len() < declared {
        let (line, next) = take_line(rest);
        rest = next;
        if line == b"%%EndPhotoshop" {
            return None;
        }
        append_hex(comment_payload(line)?, &mut resource, declared)?;
    }
    psd::thumbnail_from_resources(&resource)
}

fn find_comment<'a>(bytes: &'a [u8], marker: &[u8]) -> Option<(&'a [u8], &'a [u8])> {
    let mut rest = bytes;
    while !rest.is_empty() {
        let (line, next) = take_line(rest);
        if let Some(value) = line.strip_prefix(marker) {
            return Some((value, next));
        }
        rest = next;
    }
    None
}

fn take_line(bytes: &[u8]) -> (&[u8], &[u8]) {
    match bytes.iter().position(|&b| b == b'\n') {
        Some(i) => (
            bytes[..i].strip_suffix(b"\r").unwrap_or(&bytes[..i]),
            &bytes[i + 1..],
        ),
        None => (bytes, &[]),
    }
}

fn comment_payload(line: &[u8]) -> Option<&[u8]> {
    let payload = line.strip_prefix(b"%")?;
    Some(payload.strip_prefix(b" ").unwrap_or(payload))
}

fn append_hex(line: &[u8], out: &mut Vec<u8>, max_len: usize) -> Option<()> {
    let mut high = None;
    for &byte in line {
        if byte.is_ascii_whitespace() {
            continue;
        }
        let digit = hex_digit(byte)?;
        if let Some(high) = high.take() {
            if out.len() == max_len {
                return None;
            }
            out.push((high << 4) | digit);
        } else {
            high = Some(digit);
        }
    }
    high.is_none().then_some(())
}

fn hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn unpack_row(packed: &[u8], width: usize, depth: u32, out: &mut Vec<u8>) -> Option<()> {
    let max_value = ((1u16 << depth) - 1) as u8;
    for pixel in 0..width {
        let bit = pixel.checked_mul(depth as usize)?;
        let byte = *packed.get(bit / 8)?;
        let shift = 8usize.checked_sub(depth as usize)?.checked_sub(bit % 8)?;
        let value = (byte >> shift) & max_value;
        // EPSI defines sample zero as white and the maximum as black, the
        // inverse of an ordinary Luma8 buffer.
        out.push(255 - (value as u16 * 255 / max_value as u16) as u8);
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epsi(width: u32, height: u32, depth: u32, rows: &[&str]) -> Vec<u8> {
        let mut out = format!(
            "%!PS-Adobe-3.0 EPSF-3.0\n%%BeginPreview: {width} {height} {depth} {}\n",
            rows.len()
        )
        .into_bytes();
        for row in rows {
            out.extend_from_slice(b"% ");
            out.extend_from_slice(row.as_bytes());
            out.push(b'\n');
        }
        out.extend_from_slice(b"%%EndPreview\nshowpage\n");
        out
    }

    fn photoshop_resource(jpeg: &[u8]) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(&1u32.to_be_bytes());
        data.extend_from_slice(&[0u8; 24]);
        data.extend_from_slice(jpeg);
        let mut out = b"8BIM".to_vec();
        out.extend_from_slice(&1036u16.to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(&data);
        if data.len() & 1 == 1 {
            out.push(0);
        }
        out
    }

    fn photoshop_eps(resource: &[u8], declared: usize) -> Vec<u8> {
        let mut out =
            format!("%!PS-Adobe-3.0 EPSF-3.0\n%%BeginPhotoshop: {declared}\n").into_bytes();
        for chunk in resource.chunks(24) {
            out.extend_from_slice(b"% ");
            for byte in chunk {
                out.extend_from_slice(format!("{byte:02X}").as_bytes());
            }
            out.push(b'\n');
        }
        out.extend_from_slice(b"%%EndPhotoshop\nshowpage\n");
        out
    }

    fn tiny_tiff() -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            6,
            4,
            image::Rgb([40, 160, 220]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Tiff,
        )
        .unwrap();
        buf
    }

    fn tiny_jpeg() -> Vec<u8> {
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            6,
            4,
            image::Rgb([40, 160, 220]),
        ))
        .write_to(
            &mut std::io::Cursor::new(&mut buf),
            image::ImageFormat::Jpeg,
        )
        .unwrap();
        buf
    }

    /// Wrap `ps` + a TIFF preview in a DOS-EPS binary header, like Adobe exports.
    fn dos_eps(ps: &[u8], tiff: &[u8]) -> Vec<u8> {
        let ps_off = 30u32;
        let tiff_off = ps_off + ps.len() as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&MAGIC);
        out.extend_from_slice(&ps_off.to_le_bytes());
        out.extend_from_slice(&(ps.len() as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // WMF offset (none)
        out.extend_from_slice(&0u32.to_le_bytes()); // WMF length
        out.extend_from_slice(&tiff_off.to_le_bytes());
        out.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
        out.extend_from_slice(&[0xFF, 0xFF]); // checksum: FFFF = unused
        out.extend_from_slice(ps);
        out.extend_from_slice(tiff);
        out
    }

    #[test]
    fn extracts_dos_eps_tiff_preview() {
        let tiff = tiny_tiff();
        let eps = dos_eps(
            b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 0 0 6 4\nshowpage\n",
            &tiff,
        );
        let got = extract(&eps).expect("TIFF preview");
        assert_eq!(got, tiff);
        assert!(
            image::load_from_memory(&got).is_ok(),
            "preview should decode as TIFF"
        );
    }

    /// A blank line between the last preview row and `%%EndPreview` must not throw the preview
    /// away. **ImageMagick's `epi:` coder writes exactly that**, so before this every EPS that
    /// tool produced silently had no thumbnail while the curated corpus sample passed - Adobe
    /// does not write the blank line, which is precisely why nothing caught it.
    ///
    /// Asserted on real CONTENT and a 1-BIT depth, not on "something came back": the corpus only
    /// ever exercised 8-bit, and the pixels prove the row order and the inverted EPSI polarity
    /// still hold on the path this fix reopened.
    #[test]
    fn a_blank_line_before_end_preview_does_not_discard_it() {
        // 8x2, 1bpp. Rows are bottom-up in EPSI, and sample 0 is WHITE, so the stored top row
        // (0xFF) is all-black and the stored bottom row (0x00) is all-white once unpacked.
        let rows = ["00", "FF"];
        let clean = epsi(8, 2, 1, &rows);
        let mut blank = clean.clone();
        // The only difference: one empty line ahead of the terminator.
        let at = blank
            .windows(b"%%EndPreview".len())
            .position(|w| w == b"%%EndPreview")
            .expect("terminator");
        blank.splice(at..at, *b"\n");

        let a = epsi_preview(&clean).expect("preview without the blank line");
        let b = epsi_preview(&blank).expect("a blank line must not discard a complete preview");
        assert_eq!(a.to_luma8().into_raw(), b.to_luma8().into_raw());

        let px = b.to_luma8();
        assert_eq!((px.width(), px.height()), (8, 2));
        assert_eq!(px.get_pixel(0, 0).0[0], 0, "top row is the LAST stored row");
        assert_eq!(px.get_pixel(0, 1).0[0], 255, "sample 0 is white in EPSI");
    }

    /// The terminator is still REQUIRED - skipping blanks must not become "stop caring where the
    /// preview ends", or a truncated file would hand back half an image as if it were whole.
    #[test]
    fn a_preview_that_never_terminates_is_still_refused() {
        let mut truncated = epsi(8, 2, 1, &["00", "FF"]);
        let at = truncated
            .windows(b"%%EndPreview".len())
            .position(|w| w == b"%%EndPreview")
            .expect("terminator");
        truncated.truncate(at);
        truncated.extend_from_slice(b"\n\n\n%%Trailer\n");
        assert!(epsi_preview(&truncated).is_none());
    }

    #[test]
    fn classifies_only_eps_signatures() {
        assert!(is_eps(b"%!PS-Adobe-3.0 EPSF-3.0\n"));
        assert!(is_eps(&MAGIC));
        assert!(!is_eps(b"\x89PNG\r\n\x1A\n"));
        assert!(!is_eps(b"prefix %!PS-Adobe-3.0"));
    }

    #[test]
    fn decoder_accepts_embedded_previews_but_rejects_unscoped_jpeg_bytes() {
        let tiff = tiny_tiff();
        let dos = dos_eps(b"%!PS-Adobe-3.0 EPSF-3.0\nshowpage\n", &tiff);
        assert!(crate::decode::decode_preview(&dos).is_ok());

        let ascii = epsi(3, 1, 8, &["0055AA"]);
        assert!(crate::decode::decode_preview(&ascii).is_ok());

        let jpeg = tiny_jpeg();
        let resources = photoshop_resource(&jpeg);
        let photoshop = photoshop_eps(&resources, resources.len());
        assert!(crate::decode::decode_preview(&photoshop).is_ok());

        // A random JPEG byte run inside PostScript is not a declared raster
        // preview. Before the terminal EPS guard, the generic lenient-JPEG tier
        // accepted this and bypassed the embedded-preview-only policy.
        let mut previewless = b"%!PS-Adobe-3.0 EPSF-3.0\nshowpage\n".to_vec();
        previewless.extend_from_slice(&jpeg);
        assert!(crate::decode::decode_preview(&previewless).is_err());
    }

    #[test]
    fn rejects_plain_and_malformed_eps() {
        // Plain-text EPS: no binary header, nothing to extract.
        assert!(extract(b"%!PS-Adobe-3.0 EPSF-3.0\nshowpage\n").is_none());
        // Truncated header.
        assert!(extract(&MAGIC).is_none());
        // Header whose TIFF offsets point past EOF must fail cleanly, not panic.
        let mut bad = Vec::new();
        bad.extend_from_slice(&MAGIC);
        bad.extend_from_slice(&30u32.to_le_bytes());
        bad.extend_from_slice(&4u32.to_le_bytes());
        bad.extend_from_slice(&[0u8; 8]);
        bad.extend_from_slice(&0xFFFF_FF00u32.to_le_bytes()); // absurd TIFF offset
        bad.extend_from_slice(&0xFFFF_FF00u32.to_le_bytes()); // absurd TIFF length
        bad.extend_from_slice(&[0xFF, 0xFF]);
        assert!(extract(&bad).is_none());
    }

    #[test]
    fn decodes_epsi_depths_and_row_padding() {
        let cases = [
            (1, vec!["A0"], vec![0, 255, 0]),
            (2, vec!["1B"], vec![255, 170, 85]),
            (4, vec!["05A0"], vec![255, 170, 85]),
            (8, vec!["0055AA"], vec![255, 170, 85]),
        ];
        for (depth, rows, expected) in cases {
            let eps = epsi(3, 1, depth, &rows);
            let Some(CoverOut::Image(image)) = extract_ascii_preview(&eps) else {
                panic!("EPSI depth {depth} should decode");
            };
            assert_eq!(image.to_luma8().into_raw(), expected);
        }
    }

    #[test]
    fn decodes_epsi_rows_split_across_preview_lines() {
        let eps = epsi(3, 1, 8, &["00", " 55", "AA"]);
        let Some(CoverOut::Image(image)) = extract_ascii_preview(&eps) else {
            panic!("split EPSI row should decode");
        };
        assert_eq!(image.to_luma8().into_raw(), vec![255, 170, 85]);
    }

    #[test]
    fn decodes_epsi_rows_from_bottom_to_top() {
        let eps = epsi(2, 2, 8, &["00FF", "55AA"]);
        let Some(CoverOut::Image(image)) = extract_ascii_preview(&eps) else {
            panic!("two-row EPSI should decode");
        };
        assert_eq!(
            image.to_luma8().into_raw(),
            vec![170, 85, 255, 0],
            "the second encoded row is the displayed top row"
        );
    }

    #[test]
    fn rejects_bad_epsi_previews() {
        assert!(extract_ascii_preview(&epsi(16, 1, 8, &["00"])).is_none());
        assert!(extract_ascii_preview(&epsi(3, 1, 8, &["0001"])).is_none());
        assert!(extract_ascii_preview(&epsi(16_385, 1, 8, &["00"])).is_none());
        assert!(extract_ascii_preview(b"%!PS-Adobe-3.0 EPSF-3.0\nshowpage\n").is_none());
    }

    #[test]
    fn extracts_photoshop_preview_and_rejects_bad_payloads() {
        let jpeg = b"\xFF\xD8\xFFpreview";
        let resource = photoshop_resource(jpeg);
        let good = photoshop_eps(&resource, resource.len());
        assert!(matches!(
            extract_ascii_preview(&good),
            Some(CoverOut::Bytes(bytes)) if bytes == jpeg
        ));
        assert!(extract_ascii_preview(&photoshop_eps(&resource, resource.len() - 1)).is_none());
        assert!(extract_ascii_preview(&photoshop_eps(&resource[..8], resource.len())).is_none());
        assert!(extract_ascii_preview(b"%!PS\n%%BeginPhotoshop: 33554433\n").is_none());
    }
}
