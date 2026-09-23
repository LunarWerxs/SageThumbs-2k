//! Adobe Illustrator (`.ai`): the private thumbnail, and the artboards.
//!
//! An Illustrator file is a PDF with Illustrator's own private data appended (`/AIPrivateData`
//! stream objects; a PostScript-style header, then `%AI12_CompressedData`). Two things in it
//! matter here, both learned from issues #44 and #45 (2026-09-19):
//!
//! 1. **The PDF half is only the artwork when "Create PDF Compatible File" was ticked at save
//!    time.** Unticked, Illustrator writes a one-page placeholder that says, in the app's UI
//!    language, "this file was saved without PDF content"; rendering it faithfully is what a
//!    blank white tile with a paragraph of grey text on it is (#45). The artwork itself is in
//!    the private data, in a format nobody else renders - EXCEPT the small raster thumbnail
//!    Illustrator has written into that header since version 7, PDF-compatible or not, which
//!    is what every other viewer shows for such files. [`private_thumbnail`] decodes it.
//! 2. **One artboard is one PDF page**, so a PDF-compatible file with three artboards is a
//!    three-page PDF, and page one alone hides that the file holds more (#44). The caller
//!    lays the first pages out as a contact sheet, the way a comic archive shows its pages.
//!
//! The thumbnail's layout, established byte by byte against `test-corpus/real.ai` (Illustrator
//! 14) and checked against the PDF render of the same file: the header line
//! `%AI7_Thumbnail: <w> <h> 8`, a `%%BeginData: <n> Hex Bytes` line, then hex text on lines
//! that each start with `%`, up to `%%EndData`. Decoded, the bytes are a 256-entry RGB
//! palette (768 bytes: the 216-colour web cube, then greys and primaries), the ASCII tag
//! `RLE`, and the pixels as palette indices: a literal byte is one pixel, and the escape
//! `0xFD <count> <index>` is a run. Exactly `w*h` indices come out of a well-formed block.

use image::{DynamicImage, RgbaImage};

use super::util::find;

/// The header that names the thumbnail's size. Present in every Illustrator file since 7.
const THUMB_KEY: &[u8] = b"%AI7_Thumbnail:";
/// Never scan more of a file than this for the header: it sits near the start of the private
/// data, which follows the PDF objects; a hostile file could be gigabytes.
const SCAN_MAX: usize = 64 << 20;
/// The hex text is bounded by what a 512x512 8-bit thumbnail plus palette could need.
const MAX_HEX: usize = (768 + 512 * 512) * 2 + (512 * 512 / 30) * 3;
const MAX_EDGE: u32 = 512;
const PALETTE_BYTES: usize = 256 * 3;
const RUN: u8 = 0xFD;

/// The PDF dictionary key every PDF-based Illustrator file carries, whatever its era: the
/// catalog's `/Private` object names `/AIPrivateData1..N` (the compressed art) beside
/// `/AIMetaData`. Illustrator 2020+ files carry NO `%AI7_Thumbnail`, so this is the tell.
const PRIVATE_KEY: &[u8] = b"/AIPrivateData";

/// The user-facing reason a modern file saved without PDF content gets no thumbnail; the
/// doctor prints it as the decode's failure.
pub(crate) const NO_PDF_CONTENT: &str = "Adobe Illustrator file saved without \"Create PDF \
    Compatible File\": it holds no picture another program can show. Re-save it in Illustrator \
    with that option ticked (File > Save As > Illustrator Options).";

/// Whether `bytes` carry Illustrator's private data: the `/AIPrivateData` key of every
/// PDF-based file, or the `%AI7_Thumbnail` header of the PostScript-era ones. Content-based:
/// a shell stream is nameless.
pub(crate) fn is_illustrator(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(SCAN_MAX)];
    find(head, PRIVATE_KEY).is_some() || find(head, THUMB_KEY).is_some()
}

/// Streams looked at, and bytes inflated per stream, before the placeholder question is
/// given up on: the notice is always among the first objects Illustrator writes.
const MAX_STREAMS: usize = 256;
const MAX_INFLATED: u64 = 1 << 20;

/// Is the single page of an Illustrator file with no private raster (2020+) the "saved
/// without PDF content" placeholder rather than artwork? Structural, not visual, and not
/// tied to the UI language: that page draws one text-only Form XObject over and over, and
/// the text Adobe puts there names "Adobe Illustrator" and "PDF" in every language seen
/// (Illustrator 30.8 in English: "This is an Adobe Illustrator File that was saved without
/// PDF Content"; Spanish, from issue #45's screenshot: "Este es un archivo de Adobe
/// Illustrator guardado sin contenido en PDF"). The file's stream objects are walked
/// (bounded), Flate ones inflated (bounded), and the strings of the text operators read: a
/// text stream that names both words is the notice. Artwork whose only content is text
/// mentioning both would be judged the same way and get no thumbnail; that is the cheaper
/// mistake, and one no real file has produced.
pub(crate) fn looks_like_placeholder_page(bytes: &[u8]) -> bool {
    let head = &bytes[..bytes.len().min(SCAN_MAX)];
    let mut at = 0;
    for _ in 0..MAX_STREAMS {
        let Some(rel) = find(&head[at..], b"stream") else {
            return false;
        };
        let kw = at + rel;
        if head[..kw].ends_with(b"end") {
            at = kw + 6; // the tail of an `endstream`, not a stream start
            continue;
        }
        let Some((data, end)) = stream_bounds(head, kw) else {
            return false;
        };
        at = end + 9;
        if stream_names_illustrator_and_pdf(&head[kw.saturating_sub(600)..kw], &head[data..end]) {
            return true;
        }
    }
    false
}

/// After the `stream` keyword ending at `kw`, the byte range `(data, end)` of the stream up
/// to `endstream`, or `None` when either marker is missing.
fn stream_bounds(head: &[u8], kw: usize) -> Option<(usize, usize)> {
    let mut data = kw + 6;
    if head[data..].starts_with(b"\r\n") {
        data += 2;
    } else if head[data..].starts_with(b"\n") {
        data += 1;
    }
    let end = data + find(&head[data..], b"endstream")?;
    Some((data, end))
}

/// Does one stream object's content name both "illustrator" and "pdf"? A `FlateDecode` body
/// is inflated (bounded) first; an invalid zlib stream is simply not a match.
fn stream_names_illustrator_and_pdf(dict: &[u8], raw: &[u8]) -> bool {
    let inflated;
    let body: &[u8] = if find(dict, b"FlateDecode").is_some() {
        let Some(out) = inflate_bounded(raw) else {
            return false;
        };
        inflated = out;
        &inflated
    } else {
        raw
    };
    find(body, b"BT").is_some() && text_names_illustrator_and_pdf(body)
}

/// `raw` inflated with zlib, bounded to `MAX_INFLATED` bytes; `None` on invalid zlib.
fn inflate_bounded(raw: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(raw)
        .take(MAX_INFLATED)
        .read_to_end(&mut out)
        .ok()?;
    Some(out)
}

/// The text of a content stream's string operands (`(…) Tj`, `[(…) … (…)] TJ`), joined and
/// lowercased, names both "illustrator" and "pdf".
fn text_names_illustrator_and_pdf(stream: &[u8]) -> bool {
    let text = content_stream_text(stream);
    text.contains("illustrator") && text.contains("pdf")
}

/// The alphabetic bytes inside every `(…)` literal string of a content stream, lowercased.
fn content_stream_text(stream: &[u8]) -> String {
    let mut text = String::with_capacity(256);
    let mut i = 0;
    while i < stream.len() {
        if stream[i] == b'(' {
            i += 1;
            i = push_literal(stream, i, &mut text);
        }
        i += 1;
        if text.len() > 4096 {
            break;
        }
    }
    text
}

/// Append the alphabetic bytes of the `(…)` literal starting at `i` (the byte after `(`),
/// lowercased, and return the index of the closing `)` (or end of input). A backslash skips
/// the next byte (`\)`, `\256`'s first digit, ...).
fn push_literal(stream: &[u8], mut i: usize, text: &mut String) -> usize {
    while i < stream.len() && stream[i] != b')' {
        if stream[i] == b'\\' {
            i += 1; // skip the escaped byte
        } else if stream[i].is_ascii_alphabetic() {
            text.push(stream[i].to_ascii_lowercase() as char);
        }
        i += 1;
    }
    i
}

/// The raster thumbnail Illustrator wrote into its private data, or `None` when there is
/// none, it is malformed, or its declared size is not what the data holds.
pub(crate) fn private_thumbnail(bytes: &[u8]) -> Option<DynamicImage> {
    let head = &bytes[..bytes.len().min(SCAN_MAX)];
    let at = find(head, THUMB_KEY)?;
    let rest = &head[at + THUMB_KEY.len()..];
    let (w, h, depth, rest) = parse_size_line(rest)?;
    if depth != 8 || w == 0 || h == 0 || w > MAX_EDGE || h > MAX_EDGE {
        return None;
    }
    let raw = hex_block(rest)?;
    decode_indexed(&raw, w, h)
}

/// `<w> <h> <depth>` after the header key, then the rest of the bytes from the next line on.
fn parse_size_line(rest: &[u8]) -> Option<(u32, u32, u32, &[u8])> {
    let eol = rest.iter().position(|&b| b == b'\r' || b == b'\n')?;
    let line = std::str::from_utf8(&rest[..eol]).ok()?;
    let mut it = line.split_whitespace();
    let w = it.next()?.parse().ok()?;
    let h = it.next()?.parse().ok()?;
    let depth = it.next()?.parse().ok()?;
    Some((w, h, depth, &rest[eol..]))
}

/// The bytes of the `%%BeginData` .. `%%EndData` hex block: every line that starts with a
/// single `%` contributes its hex digits; anything else is skipped; `%%EndData` ends it.
fn hex_block(rest: &[u8]) -> Option<Vec<u8>> {
    let begin = find(&rest[..rest.len().min(4096)], b"%%BeginData")?;
    let hex = collect_hex_lines(&rest[begin..])?;
    pack_hex(&hex)
}

/// The hex digits between `%%BeginData` and `%%EndData`: every line prefixed with a single
/// `%` contributes, `%%` comments are skipped, `%%EndData` ends it. `None` when the block
/// never ends, overruns `MAX_HEX`, or holds fewer than two digits.
fn collect_hex_lines(block: &[u8]) -> Option<Vec<u8>> {
    let mut hex: Vec<u8> = Vec::new();
    let mut seen_end = false;
    for line in block.split(|&b| b == b'\r' || b == b'\n').skip(1) {
        if line.starts_with(b"%%EndData") {
            seen_end = true;
            break;
        }
        push_line_hex(line, &mut hex);
        if hex.len() > MAX_HEX {
            return None;
        }
    }
    if !seen_end || hex.len() < 2 {
        return None;
    }
    Some(hex)
}

/// Append one block line's hex digits: the bytes after a lone `%`, filtered to hex. A `%%`
/// comment or a line without a `%` prefix contributes nothing.
fn push_line_hex(line: &[u8], hex: &mut Vec<u8>) {
    if let Some(digits) = line.strip_prefix(b"%") {
        if !digits.starts_with(b"%") {
            hex.extend(digits.iter().filter(|b| b.is_ascii_hexdigit()));
        }
    }
}

/// Two hex digits per output byte; `None` on a non-hex digit.
fn pack_hex(hex: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(hex.len() / 2);
    for [hi, lo] in hex.as_chunks::<2>().0 {
        let hi = (*hi as char).to_digit(16)?;
        let lo = (*lo as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8);
    }
    Some(out)
}

/// Palette + `RLE` + runs -> an RGBA image, or `None` unless exactly `w*h` pixels decode.
fn decode_indexed(raw: &[u8], w: u32, h: u32) -> Option<DynamicImage> {
    let palette = raw.get(..PALETTE_BYTES)?;
    let stream = raw.get(PALETTE_BYTES..)?.strip_prefix(b"RLE")?;
    let want = (w as usize).checked_mul(h as usize)?;
    let idx = decode_rle_indices(stream, want)?;
    if idx.len() != want {
        return None;
    }
    let mut img = RgbaImage::new(w, h);
    for (px, &k) in img.pixels_mut().zip(idx.iter()) {
        let p = &palette[k as usize * 3..k as usize * 3 + 3];
        *px = image::Rgba([p[0], p[1], p[2], 255]);
    }
    Some(DynamicImage::ImageRgba8(img))
}

/// The colour indices of an `RLE` pixel stream, capped at `want`: a literal byte is one
/// pixel, the escape `0xFD <count> <index>` is a run. `None` on a truncated escape.
fn decode_rle_indices(stream: &[u8], want: usize) -> Option<Vec<u8>> {
    let mut idx: Vec<u8> = Vec::with_capacity(want);
    let mut i = 0;
    while i < stream.len() && idx.len() < want {
        let b = stream[i];
        if b == RUN {
            let count = *stream.get(i + 1)? as usize;
            let value = *stream.get(i + 2)?;
            let n = count.min(want - idx.len());
            idx.extend(std::iter::repeat_n(value, n));
            i += 3;
        } else {
            idx.push(b);
            i += 1;
        }
    }
    Some(idx)
}

/// Fraction of pixels that are not near-white, on a picture reduced to at most 64 px. The
/// placeholder page a non-PDF-compatible file renders to is white with a small block of
/// grey text: a few percent at most. Real artwork is almost always more; and when it is not
/// (a small mark on a white artboard), the private thumbnail shows that same mark, so
/// preferring it costs resolution, never correctness - which is why the decision below is
/// allowed to err towards the thumbnail.
pub(crate) fn ink_fraction(img: &DynamicImage) -> f32 {
    let small = img.thumbnail(64, 64).to_rgba8();
    let total = small.width() as usize * small.height() as usize;
    if total == 0 {
        return 0.0;
    }
    let ink = small
        .pixels()
        .filter(|p| p[3] > 16 && (p[0] < 235 || p[1] < 235 || p[2] < 235))
        .count();
    ink as f32 / total as f32
}

/// Is `page` the "saved without PDF content" placeholder rather than the artwork?
/// Decided against the private thumbnail of the same file: the page is near-blank AND the
/// thumbnail plainly is not.
pub(crate) fn page_is_placeholder(page: &DynamicImage, thumb: &DynamicImage) -> bool {
    let page_ink = ink_fraction(page);
    let thumb_ink = ink_fraction(thumb);
    page_ink < 0.04 && thumb_ink > page_ink * 2.0 && thumb_ink > 0.02
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout established against Illustrator 14's own output: 56x128, 8-bit, a 256
    /// palette, the RLE tag, FD-escaped runs, exactly w*h pixels. `real.ai` is a corpus
    /// sample, not a fixture in git: absent (CI), the test says NOT MEASURED and returns.
    #[test]
    fn real_ai_private_thumbnail_decodes_to_its_declared_size() {
        let Some(bytes) = st2k_base::testcorpus::read("real.ai") else {
            return;
        };
        assert!(is_illustrator(&bytes));
        let img = private_thumbnail(&bytes).expect("thumbnail");
        assert_eq!((img.width(), img.height()), (56, 128));
        // A sheet of icons on white: mostly white, plainly not blank.
        let ink = ink_fraction(&img);
        assert!(ink > 0.05 && ink < 0.6, "ink fraction {ink}");
        // Mostly white paper with coloured icons on it: white is the commonest pixel by far,
        // and there is real colour, not a grey ramp.
        let rgba = img.to_rgba8();
        let white = rgba
            .pixels()
            .filter(|p| p.0 == [255, 255, 255, 255])
            .count();
        assert!(
            white * 2 > rgba.pixels().len(),
            "white pixels: {white} of {}",
            rgba.pixels().len()
        );
        assert!(
            rgba.pixels().any(|p| p[0] != p[1] || p[1] != p[2]),
            "no colour at all"
        );
    }

    /// The thumbnail is a faithful low-resolution picture of the same artwork the PDF page
    /// renders: the two agree on where the ink is, row by row, which is what makes it a safe
    /// stand-in when the page is the placeholder.
    #[test]
    fn real_ai_private_thumbnail_agrees_with_the_rendered_page() {
        let Some(bytes) = st2k_base::testcorpus::read("real.ai") else {
            return;
        };
        let thumb = private_thumbnail(&bytes).expect("thumbnail");
        let png = crate::pdf::render_first_page(&bytes, 256).expect("page render");
        let page = image::load_from_memory(&png).expect("png");
        assert!(
            !page_is_placeholder(&page, &thumb),
            "a real page is not the placeholder"
        );
        let rows = |img: &DynamicImage| -> Vec<bool> {
            let s = img
                .resize_exact(16, 32, image::imageops::FilterType::Triangle)
                .to_rgba8();
            (0..32)
                .map(|y| (0..16).any(|x| s.get_pixel(x, y).0[..3].iter().any(|&c| c < 200)))
                .collect()
        };
        let (a, b) = (rows(&thumb), rows(&page));
        let agree = a.iter().zip(&b).filter(|(x, y)| x == y).count();
        assert!(agree >= 26, "rows agreeing on ink: {agree}/32");
    }

    /// The placeholder decision: a near-white page against a thumbnail with a picture on it.
    #[test]
    fn a_near_blank_page_defers_to_a_thumbnail_that_is_not() {
        let mut page = RgbaImage::from_pixel(200, 260, image::Rgba([255, 255, 255, 255]));
        // A paragraph of small grey text: ~1.5% of the page.
        for y in 40..46 {
            for x in 30..160 {
                page.put_pixel(x, y, image::Rgba([90, 90, 90, 255]));
            }
        }
        let page = DynamicImage::ImageRgba8(page);
        let mut thumb = RgbaImage::from_pixel(56, 128, image::Rgba([255, 255, 255, 255]));
        for y in 20..100 {
            for x in 8..48 {
                thumb.put_pixel(x, y, image::Rgba([220, 40, 30, 255]));
            }
        }
        let thumb = DynamicImage::ImageRgba8(thumb);
        assert!(page_is_placeholder(&page, &thumb));
        // Both blank: keep the page (nothing to prefer).
        let blank = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
            56,
            128,
            image::Rgba([255, 255, 255, 255]),
        ));
        assert!(!page_is_placeholder(&page, &blank));
        // A page with real artwork is never the placeholder, whatever the thumbnail holds.
        assert!(!page_is_placeholder(&thumb, &thumb));
    }

    /// Malformed blocks are refused, never guessed at: a size the data cannot fill, a depth
    /// other than 8, a missing end marker, an oversized declared edge.
    #[test]
    fn malformed_thumbnails_are_refused() {
        // The refusals that need no sample run everywhere.
        assert!(private_thumbnail(b"%PDF-1.4 nothing here").is_none());
        assert!(!is_illustrator(b"%PDF-1.4 nothing here"));
        assert!(
            private_thumbnail(b"%AI7_Thumbnail: 2 2 8\r%%BeginData: 4 Hex Bytes\r%00").is_none()
        );
        // The mutations of a real block need the corpus file (NOT MEASURED without it).
        let Some(good) = st2k_base::testcorpus::read("real.ai") else {
            return;
        };
        let at = find(&good, THUMB_KEY).unwrap();
        // Declare more rows than the runs supply.
        let mut short = good.clone();
        short.splice(
            at..at + b"%AI7_Thumbnail: 56 128 8".len(),
            b"%AI7_Thumbnail: 56 129 8".iter().copied(),
        );
        assert!(private_thumbnail(&short).is_none());
        let mut depth = good.clone();
        depth.splice(
            at..at + b"%AI7_Thumbnail: 56 128 8".len(),
            b"%AI7_Thumbnail: 56 128 4".iter().copied(),
        );
        assert!(private_thumbnail(&depth).is_none());
        let mut huge = good.clone();
        huge.splice(
            at..at + b"%AI7_Thumbnail: 56 128 8".len(),
            b"%AI7_Thumbnail: 9999 128".iter().copied(),
        );
        assert!(private_thumbnail(&huge).is_none());
        let end = find(&good, b"%%EndData").unwrap();
        let truncated = good[..end].to_vec();
        assert!(private_thumbnail(&truncated).is_none());
    }
}
