//! DjVu thumbnail extraction.
//!
//! A DjVu page stacks two images: an IW44-wavelet **background** (`BG44`) and a
//! JB2 bilevel **text mask** (`Sjbz`) tinted by the `FGbz` foreground palette;
//! the visible page is the mask composited over the background. We decode both:
//! the wavelet via [`super::iw44`], the mask via [`super::jb2`] (both on the
//! shared [`super::zp`] arithmetic coder). A pre-rendered `TH44` page thumbnail
//! is preferred outright (it has everything baked in). Pages whose mask needs a
//! shared multipage dictionary (`INCL`→`Djbz`) degrade to background-only; pages
//! with nothing decodable return None and keep the default icon (we never panic
//! — all slicing is checked).

use image::DynamicImage;

/// An IW44 layer to decode: the chunk payloads (a primary chunk + any
/// progressive-refinement chunks, in order). Geometry comes from the serial-0
/// chunk's own header.
pub struct Layer {
    pub chunks: Vec<Vec<u8>>,
}

/// Everything thumbnail-relevant found in one page's chunk stream.
struct Page {
    /// Page dimensions from `INFO` (full resolution).
    width: usize,
    height: usize,
    /// Pre-rendered page thumbnail (`TH44`), preferred when present.
    th44: Vec<Vec<u8>>,
    /// Background layer (`BG44`), progressive chunk sequence.
    bg44: Vec<Vec<u8>>,
    /// Bilevel text mask (`Sjbz`).
    sjbz: Option<Vec<u8>>,
    /// Foreground palette (`FGbz`).
    fgbz: Option<Vec<u8>>,
}

pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    let page = find_page(bytes)?;

    // A TH44 thumbnail is the encoder's own page rendering — use it as-is.
    if !page.th44.is_empty() {
        return super::iw44::decode_layer(&Layer { chunks: page.th44 });
    }

    // Decode the background (if any), then composite the text mask over it.
    let bg = if page.bg44.is_empty() {
        None
    } else {
        super::iw44::decode_layer(&Layer { chunks: page.bg44 })
    };

    let mask = page
        .sjbz
        .as_deref()
        .and_then(super::jb2::decode)
        .filter(|m| m.width as usize == page.width && m.height as usize == page.height);

    match (bg, mask) {
        (Some(bg), Some(mask)) => Some(composite(bg, &mask, page.fgbz.as_deref())),
        (Some(bg), None) => Some(bg), // photo page / unsupported mask
        (None, Some(mask)) => Some(render_mask(&mask, page.fgbz.as_deref())),
        (None, None) => None,
    }
}

/// Composite the JB2 mask over the decoded background. The background is
/// subsampled (typically 3×) relative to the page, so the mask is reduced to a
/// per-background-pixel coverage count and alpha-blended — which is also what
/// anti-aliases the text.
fn composite(bg: DynamicImage, mask: &super::jb2::Jb2Image, fgbz: Option<&[u8]>) -> DynamicImage {
    let mut rgb = bg.to_rgb8();
    let (bw, bh) = (rgb.width() as usize, rgb.height() as usize);
    if bw == 0 || bh == 0 {
        return DynamicImage::ImageRgb8(rgb);
    }
    let sub = (mask.width as usize).div_ceil(bw).max(1) as u32;
    let cov = super::jb2::coverage(mask, sub, bw, bh);
    let fg = fgbz.and_then(super::jb2::fg_color).unwrap_or([0, 0, 0]);
    let maxc = (sub * sub) as u32;
    for (px, &c) in rgb.pixels_mut().zip(&cov) {
        if c == 0 {
            continue;
        }
        let a = (c as u32).min(maxc);
        for (ch, &f) in px.0.iter_mut().zip(&fg) {
            *ch = ((*ch as u32 * (maxc - a) + f as u32 * a) / maxc) as u8;
        }
    }
    DynamicImage::ImageRgb8(rgb)
}

/// Pure-bilevel page (no background layer): render the mask on white at a
/// thumbnail-friendly subsample.
fn render_mask(mask: &super::jb2::Jb2Image, fgbz: Option<&[u8]>) -> DynamicImage {
    let (w, h) = (mask.width as usize, mask.height as usize);
    let sub = (w.max(h).div_ceil(1600)).max(1) as u32;
    let (ow, oh) = (w.div_ceil(sub as usize).max(1), h.div_ceil(sub as usize).max(1));
    let cov = super::jb2::coverage(mask, sub, ow, oh);
    let fg = fgbz.and_then(super::jb2::fg_color).unwrap_or([0, 0, 0]);
    let maxc = (sub * sub) as u32;
    let mut rgb = image::RgbImage::from_pixel(ow as u32, oh as u32, image::Rgb([255, 255, 255]));
    for (px, &c) in rgb.pixels_mut().zip(&cov) {
        if c == 0 {
            continue;
        }
        let a = (c as u32).min(maxc);
        for (ch, &f) in px.0.iter_mut().zip(&fg) {
            *ch = ((255 * (maxc - a) + f as u32 * a) / maxc) as u8;
        }
    }
    DynamicImage::ImageRgb8(rgb)
}

fn be16(d: &[u8], o: usize) -> Option<usize> {
    Some(((*d.get(o)? as usize) << 8) | *d.get(o + 1)? as usize)
}

fn be32(d: &[u8], o: usize) -> Option<usize> {
    Some(((be16(d, o)?) << 16) | be16(d, o + 2)?)
}

fn id4(d: &[u8], o: usize) -> Option<&[u8]> {
    d.get(o..o + 4)
}

/// Locate the first page and gather its thumbnail-relevant chunks. Handles
/// single-page (`FORM:DJVU`) and multi-page (`FORM:DJVM` → first `FORM:DJVU`).
fn find_page(bytes: &[u8]) -> Option<Page> {
    if bytes.len() < 16 || &bytes[0..4] != b"AT&T" {
        return None;
    }
    // The file body is one top-level FORM chunk.
    if id4(bytes, 4)? != b"FORM" {
        return None;
    }
    let top_len = be32(bytes, 8)?;
    let top_type = id4(bytes, 12)?;
    // Sub-chunk area of the top FORM (after the 4-byte form type).
    let body_start = 16;
    let body_end = (12 + top_len).min(bytes.len());

    let page = match top_type {
        b"DJVU" => (body_start, body_end), // single page
        b"DJVM" => find_first_djvu(bytes, body_start, body_end)?, // multipage → first page
        _ => return None,
    };
    collect_page(bytes, page.0, page.1)
}

/// In a DJVM body, find the first embedded `FORM:DJVU` page, returning its
/// sub-chunk range.
fn find_first_djvu(d: &[u8], start: usize, end: usize) -> Option<(usize, usize)> {
    let mut off = start;
    while off + 8 <= end {
        let cid = id4(d, off)?;
        let len = be32(d, off + 4)?;
        if cid == b"FORM" && id4(d, off + 8)? == b"DJVU" {
            let cend = (off + 8 + len).min(end);
            return Some((off + 12, cend)); // sub-chunks start after FORM+len+type
        }
        // advance, chunks are padded to even length
        off += 8 + len + (len & 1);
    }
    None
}

/// Walk a page's sub-chunks, gathering INFO dimensions, IW44 layers and the
/// JB2 mask + palette.
fn collect_page(d: &[u8], start: usize, end: usize) -> Option<Page> {
    let mut page = Page {
        width: 0,
        height: 0,
        th44: Vec::new(),
        bg44: Vec::new(),
        sjbz: None,
        fgbz: None,
    };

    let mut off = start;
    while off + 8 <= end {
        let cid = id4(d, off)?.to_vec();
        let len = be32(d, off + 4)?;
        let data_end = (off + 8 + len).min(d.len());
        let data = d.get(off + 8..data_end)?;
        match cid.as_slice() {
            b"INFO" => {
                page.width = be16(data, 0).unwrap_or(0);
                page.height = be16(data, 2).unwrap_or(0);
            }
            b"BG44" => page.bg44.push(data.to_vec()),
            b"TH44" => page.th44.push(data.to_vec()),
            b"Sjbz" => page.sjbz = Some(data.to_vec()),
            b"FGbz" => page.fgbz = Some(data.to_vec()),
            _ => {}
        }
        off += 8 + len + (len & 1);
    }

    if page.th44.is_empty() && page.bg44.is_empty() && page.sjbz.is_none() {
        return None;
    }
    Some(page)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal DjVu (AT&T + FORM:DJVU + INFO + one BG44 stub) and confirm
    /// the IFF walker pulls out the dimensions + the BG44 payload.
    fn synthetic_djvu() -> Vec<u8> {
        fn chunk(id: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut c = Vec::new();
            c.extend_from_slice(id);
            c.extend_from_slice(&(data.len() as u32).to_be_bytes());
            c.extend_from_slice(data);
            if data.len() & 1 == 1 {
                c.push(0); // pad to even
            }
            c
        }
        let info = [0x01, 0x40, 0x00, 0xC8, 24, 0, 0, 0, 0, 0]; // 320 x 200
        let bg = [0xBEu8, 0xEF, 0x12, 0x34, 0x56]; // opaque IW44 stub
        let mut body = Vec::new();
        body.extend_from_slice(b"DJVU");
        body.extend_from_slice(&chunk(b"INFO", &info));
        body.extend_from_slice(&chunk(b"BG44", &bg));
        let mut form = Vec::new();
        form.extend_from_slice(b"FORM");
        form.extend_from_slice(&(body.len() as u32).to_be_bytes());
        form.extend_from_slice(&body);
        let mut out = Vec::new();
        out.extend_from_slice(b"AT&T");
        out.extend_from_slice(&form);
        out
    }

    #[test]
    fn iff_parser_finds_dims_and_layer() {
        let d = synthetic_djvu();
        let page = find_page(&d).expect("should parse the synthetic DjVu");
        assert_eq!((page.width, page.height), (320, 200));
        assert_eq!(page.bg44.len(), 1);
        assert_eq!(page.bg44[0], vec![0xBE, 0xEF, 0x12, 0x34, 0x56]);
        assert!(page.th44.is_empty() && page.sjbz.is_none());
    }

    #[test]
    fn rejects_non_djvu() {
        assert!(find_page(b"not a djvu file at all").is_none());
        assert!(find_page(&[]).is_none());
    }

    // Verifies the IFF walker on the REAL multi-page sample (downloaded to the
    // scratch dir during development). Run explicitly:
    //   cargo test --release -- --ignored real_djvu_iff
    #[test]
    #[ignore]
    fn real_djvu_iff_structure() {
        let path = r"D:\st2k-target\djvu\Example.djvu";
        let Ok(bytes) = std::fs::read(path) else {
            return; // sample not present
        };
        let page = find_page(&bytes).expect("should parse the real DJVM");
        assert!(page.width > 0 && page.height > 0, "got {}x{}", page.width, page.height);
        assert!(!page.bg44.is_empty(), "should find BG44 chunks");
        assert!(page.sjbz.is_some(), "page 1 carries a text mask");
        assert!(page.fgbz.is_some(), "page 1 carries a foreground palette");
    }

    /// Full real-file composite: background + JB2 text. Writes a PNG for visual
    /// inspection. Run explicitly:
    ///   cargo test --release -- --ignored real_djvu_composite
    #[test]
    #[ignore]
    fn real_djvu_composite() {
        let path = r"D:\st2k-target\djvu\Example.djvu";
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let img = extract(&bytes).expect("should decode the real DjVu");
        assert_eq!((img.width(), img.height()), (1700, 2200));
        img.save(r"D:\st2k-target\djvu\decoded_page.png").expect("save PNG");
    }
}
