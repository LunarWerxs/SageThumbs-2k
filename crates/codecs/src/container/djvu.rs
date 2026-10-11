//! DjVu (.djvu) cover extraction via the maintained pure-Rust `djvu-rs` crate.
//!
//! Replaced the hand-rolled `zp`/`iw44`/`jb2` decode stack (2026-06-14): djvu-rs is
//! MIT, C-free, and fuzzed, and handles multipage shared-dictionary (`INCL`→`Djbz`)
//! pages our hand-roll degraded to background-only. We decode page 1 to RGBA and
//! return it as a `DynamicImage` (the contract the rest of `container` expects).
//!
//! Uses the high-level (`std`) API on purpose: the crate's render pipeline requires
//! `std` at runtime — a `default-features=false` no_std build compiles but silently
//! fails to decode (see the Cargo.toml note). Runs in Explorer's thumbnail host under
//! `panic = "abort"`; djvu-rs is fuzzed, but we still bomb-guard + cap before allocating.
//!
//! A scanned book is the DjVu file that gets big, and djvu-rs refuses a bundle whose declared
//! length runs past the bytes it is given, so a book too big to hold had no thumbnail even
//! though its first page sits a few MB in. [`first_page_seek`] reads that page by offset.

use std::io::{Read, Seek, SeekFrom};

use image::DynamicImage;

/// The decode pipeline's single bomb-guard ceiling.
const MAX_DIM: u32 = crate::decode::limits::MAX_DIM;
/// Cap the rendered long edge: a full-res scan can be 5000×6600 (~130 MB RGBA),
/// pointless for a thumbnail the caller fit-to-box downscales anyway.
///
/// Deliberately NOT lowered to the caller's target. It reads like free money (why composite
/// 1600 px for a 256 px tile) and it is not, twice over. The cost here is the JB2 mask and the
/// IW44 background, both decoded at the page's own resolution and therefore costing the same
/// whatever size they are composited into: measured 105 ms at cap 128 through 125 ms at cap
/// 1600 on the 600 dpi corpus scan, so the entire saving is our own downscale, tens of ms.
/// What it BUYS at that price is a coarser IW44 subsample, and the compositor samples the
/// background plane nearest-neighbour, so shrinking the render visibly blocks up photographic
/// content (obvious side by side at a 768 px target). Worse, a coarser subsample walks
/// straight into the decoder defect documented on [`render_page`].
const RENDER_CAP: u32 = 1600;

/// Cover for a caller with no size in mind (Convert, the container dispatcher).
pub fn extract(bytes: &[u8]) -> Option<DynamicImage> {
    extract_scaled(bytes, None)
}

/// Cover for a caller that knows the longest side it can use.
///
/// The target does NOT shrink the render (see [`RENDER_CAP`]). Its one job is to decide whether
/// the file's baked TH44 thumbnail can answer this request at all, which is a correctness
/// question rather than a speed one - see the comment on that branch below.
pub(crate) fn extract_scaled(bytes: &[u8], target_edge: Option<u32>) -> Option<DynamicImage> {
    // `djvu_rs::Document::from_bytes` only takes an owned `Vec<u8>` (no `&[u8]`-borrowing
    // constructor in its public API - `DjVuDocument::parse(&[u8])` is the low-level, non-owning
    // entry point, but it lacks the display_width/height rotation handling this file relies on,
    // so switching to it would risk silently swapping w/h on a rotated page). The crate itself
    // does NOT copy again: `Document::from_bytes` moves the Vec straight into an `Arc`. The one
    // copy here is therefore bounded by the same input cap every caller already enforces before
    // reaching this function (`decode::limits::MAX_INPUT_BYTES`, 256 MiB) - a bounded one-time
    // cost, not unbounded growth.
    let doc = djvu_rs::Document::from_bytes(bytes.to_vec()).ok()?;
    let page = doc.page(0).ok()?;

    let (dw, dh) = (page.display_width().max(1), page.display_height().max(1));
    let long = dw.max(dh);
    // The caller's usable size, clamped by the page long edge and by RENDER_CAP - not the size
    // a render comes back at. That is the bar the baked thumbnail has to clear, and it is bounded
    // by the page as well as by the caller: for a page smaller than the request, a thumbnail the
    // size of the page IS the whole picture.
    let want = target_edge
        .filter(|t| *t > 0)
        .unwrap_or(RENDER_CAP)
        .min(RENDER_CAP)
        .min(long);

    // The encoder's baked page thumbnail (TH44) is nearly free next to a render, but only
    // usable when it is at least as big as the tile that was asked for. Every encoder that
    // writes one caps it at 128 px on the long edge (djvu-rs `thumbnail::THUMBNAIL_MAX_SIDE`,
    // and DjVuLibre's `djvused set-thumbnails` defaults to the same), so taking it
    // unconditionally - which is what shipped through 2.3.0 - answered Explorer's 256 px and
    // 768 px views with a 128 px picture to stretch, and answered Convert with a 128 px picture
    // for a 5100 px page. Same shape as the InDesign bug: something decoded, so nobody asked
    // how big it was. Below the bar we render instead. At 96 px (Explorer's small-icon view)
    // the baked thumbnail still clears the bar and is still nearly free.
    //
    // `page.thumbnail()` fully decodes the TH44 IW44 stream (allocating its RGBA buffer)
    // before returning, so the MAX_DIM check below necessarily runs AFTER that allocation -
    // djvu-rs 0.27's public API has no declared-dimension probe to check first. We rely on
    // djvu-rs's own internal bounds during that decode (the crate is fuzzed for exactly this
    // input); MAX_DIM here is defense in depth against whatever it does hand back, not a
    // pre-allocation guard.
    let baked = match page.thumbnail() {
        Ok(Some(thumb)) if thumb.width.max(thumb.height) >= want => Some(thumb),
        _ => None,
    };
    let pm = match baked {
        Some(thumb) => thumb,
        None => render_page(&page, dw, dh, long)?,
    };

    if pm.width == 0 || pm.height == 0 || pm.width > MAX_DIM || pm.height > MAX_DIM {
        return None;
    }
    // pm.data is straight RGBA8 (4 B/px), top row first — same as our other decoders.
    image::RgbaImage::from_raw(pm.width, pm.height, pm.data).map(DynamicImage::ImageRgba8)
}

/// Composite page 1 at up to [`RENDER_CAP`] on its long edge.
///
/// Goes through `render_progressive` over EVERY BG44 chunk rather than the obvious
/// `render_to_size`, and that is not a style choice.
///
/// djvu-rs 0.27's normal render path takes a first-BG44-chunk-only shortcut whenever the IW44
/// subsample lands at 4 or 8 (`decode_background_chunks` -> `decoded_bg44_partial`), on the
/// reasoning that later chunks are only high-frequency refinement. That holds for a file whose
/// first chunk already carries the image, which is what DjVuLibre's `c44` writes (its default
/// `-slice 74,89,99` puts 74 of 99 slices in chunk one). It does NOT hold for an encoder that
/// chunks finely - djvu-rs's own encoder defaults to ten chunks of ten slices - and there the
/// partial decode yields no usable background at all. The render then silently drops the whole
/// background layer instead of failing:
///
///   - a DjVuPhoto page (`INFO + BG44` only, no mask) comes back a FLAT GREY RECTANGLE;
///   - a layered page comes back as bare ink on blank paper, the photographs gone.
///
/// Reproduced end to end through the shipped binary on generated 3400x4400 and 5100x6600 pages:
/// standard deviation 0.00 across the whole tile, i.e. not a picture. It bites at the shipped
/// cap for any page past roughly 3300 px on the long edge, which is an ordinary letter or A4
/// scan at 400 dpi and up.
///
/// `render_progressive(.., n - 1)` asks for chunks `0..=n-1` explicitly, which routes through
/// the branch that decodes each chunk itself and never consults the partial cache. Verified to
/// produce the same picture as `render_to_size` on every file where `render_to_size` is right
/// (the real corpus scan included), and a picture instead of grey on every file where it is
/// not. A page with no BG44 at all is bilevel - there is no background to lose - so it takes
/// the plain path.
fn render_page(page: &djvu_rs::Page<'_>, dw: u32, dh: u32, long: u32) -> Option<djvu_rs::Pixmap> {
    let (w, h) = if long > RENDER_CAP {
        let s = RENDER_CAP as f32 / long as f32;
        (
            ((dw as f32 * s).round() as u32).max(1),
            ((dh as f32 * s).round() as u32).max(1),
        )
    } else {
        (dw, dh)
    };
    match page.bg44_chunk_count() {
        0 => page.render_to_size(w, h).ok(),
        n => page.render_progressive(w, h, n - 1).ok(),
    }
}

/// True for a bundled or indirect multi-page document (`AT&T FORM ... DJVM`).
pub fn looks_like_djvm(head: &[u8]) -> bool {
    head.starts_with(b"AT&TFORM") && head.get(12..16) == Some(b"DJVM")
}

/// The longest `DIRM` payload read: a directory of 65 535 components is 256 KiB of offsets
/// plus their compressed names.
const MAX_DIRM: u32 = 4 << 20;

/// One component named by the document's `DIRM` directory.
struct Part {
    offset: u64,
    /// `0` shared (an `INCL` target such as a JB2 shape dictionary), `1` page, `2` thumbnails.
    kind: u8,
    id: String,
}

/// [`extract`] for a bundled document too big to hold, off a seekable reader.
pub(super) fn first_page_seek<R: Read + Seek>(r: &mut R) -> Option<DynamicImage> {
    extract(&first_page_bundle(r)?)
}

/// A bundled document's page 1 as a one-page document of its own, which every DjVu reader
/// draws. The directory (`DIRM`) at the head gives every component's offset, so page 1 and the
/// shared components it includes are read on their own, a few small reads however long the
/// book is.
pub fn first_page_bundle<R: Read + Seek>(r: &mut R) -> Option<Vec<u8>> {
    let parts = directory(r)?;
    rebundle_first_page(r, &parts)
}

/// The components a bundled document's `DIRM` names, in file order.
fn directory<R: Read + Seek>(r: &mut R) -> Option<Vec<Part>> {
    let mut head = [0u8; 24];
    r.seek(SeekFrom::Start(0)).ok()?;
    r.read_exact(&mut head).ok()?;
    if !looks_like_djvm(&head) || &head[16..20] != b"DIRM" {
        return None;
    }
    let len = u32::from_be_bytes([head[20], head[21], head[22], head[23]]);
    let payload = read_exact_vec(r, u64::from(len.min(MAX_DIRM)))?;
    parse_directory(&payload)
}

/// A bundled `DIRM` payload: flags (bit 7 = bundled), a component count, one offset each, then
/// a BZZ block holding 3-byte sizes, 1-byte flags and NUL-terminated ids (each id followed by a
/// name when its flag has bit 7, and a title when it has bit 6).
fn parse_directory(p: &[u8]) -> Option<Vec<Part>> {
    let (&flags, rest) = p.split_first()?;
    if flags & 0x80 == 0 || rest.len() < 2 {
        return None; // indirect: the pages live in other files
    }
    let n = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
    let offsets = rest.get(2..2 + n * 4)?;
    let meta = djvu_rs::bzz::bzz_decode(&rest[2 + n * 4..]).ok()?;
    let kinds = meta.get(n * 3..n * 4)?;
    let mut strings = meta.get(n * 4..)?.split(|&b| b == 0);
    let mut parts = Vec::with_capacity(n);
    for (i, &kind) in kinds.iter().enumerate() {
        let id = String::from_utf8_lossy(strings.next()?).into_owned();
        // The optional name and title follow the id; only the id is ever referenced.
        for bit in [0x80, 0x40] {
            if kind & bit != 0 {
                strings.next()?;
            }
        }
        let at = &offsets[i * 4..i * 4 + 4];
        let offset = u64::from(u32::from_be_bytes([at[0], at[1], at[2], at[3]]));
        parts.push(Part {
            offset,
            kind: kind & 0x3f,
            id,
        });
    }
    Some(parts)
}

/// Page 1 and the shared components its `INCL` chunks name, as a bundle of their own.
fn rebundle_first_page<R: Read + Seek>(r: &mut R, parts: &[Part]) -> Option<Vec<u8>> {
    use djvu_rs::djvm::{DjvmSpool, DjvmStreamWriter};
    let page = parts.iter().find(|p| p.kind == 1)?;
    let page_form = read_form(r, page.offset)?;
    let mut bundle = DjvmStreamWriter::new(Vec::new(), DjvmSpool::Memory).ok()?;
    for id in includes(&page_form) {
        // A missing dictionary is the decoder's to judge; it may still draw the page.
        let Some(shared) = parts.iter().find(|p| p.kind == 0 && p.id == id) else {
            continue;
        };
        bundle
            .add_component(&shared.id, 0, &read_form(r, shared.offset)?)
            .ok()?;
    }
    bundle.add_component(&page.id, 1, &page_form).ok()?;
    bundle.finish().ok()
}

/// The `FORM` chunk at `at`, header included, at most [`super::MAX_COVER`] long.
fn read_form<R: Read + Seek>(r: &mut R, at: u64) -> Option<Vec<u8>> {
    let mut hdr = [0u8; 8];
    r.seek(SeekFrom::Start(at)).ok()?;
    r.read_exact(&mut hdr).ok()?;
    let len = u64::from(u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]));
    if &hdr[..4] != b"FORM" || len > super::MAX_COVER {
        return None;
    }
    let body = read_exact_vec(r, len)?;
    let mut form = hdr.to_vec();
    form.extend_from_slice(&body);
    Some(form)
}

/// Exactly `len` bytes from the reader's position, reserved fallibly (this runs in the shell's
/// thumbnail host, where a refused allocation must decline rather than abort), or None.
fn read_exact_vec<R: Read>(r: &mut R, len: u64) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    out.try_reserve_exact(usize::try_from(len).ok()?).ok()?;
    r.take(len).read_to_end(&mut out).ok()?;
    (out.len() as u64 == len).then_some(out)
}

/// The component ids a page's `INCL` chunks name, in order.
fn includes(form: &[u8]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut at = 12; // past "FORM", the length and "DJVU"
    while let Some(hdr) = form.get(at..at + 8) {
        let len = u32::from_be_bytes([hdr[4], hdr[5], hdr[6], hdr[7]]) as usize;
        let Some(data) = form.get(at + 8..(at + 8).saturating_add(len)) else {
            break;
        };
        if &hdr[..4] == b"INCL" {
            let id = String::from_utf8_lossy(data);
            ids.push(id.trim_end_matches(['\0', '\n', '\r', ' ']).to_owned());
        }
        at += 8 + len + (len & 1); // chunks are padded to an even length
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use djvu_rs::djvu_encode::{
        encode_djvm_layered_shared_with_thumbnails, EncodeQuality, PageEncoder,
    };

    #[test]
    fn rejects_non_djvu() {
        assert!(extract(b"not a djvu file at all").is_none());
        assert!(extract(&[]).is_none());
    }

    /// A page of flat colour blocks, big enough to be worth scaling and varied enough that a
    /// correct render has obvious contrast while a dropped layer has none.
    fn source_page(w: u32, h: u32) -> djvu_rs::Pixmap {
        let mut data = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                // Eight coarse bands, so the picture survives any sane downscale.
                let band = ((x * 4 / w) + (y * 2 / h) * 4) as u8;
                let (r, g, b) = match band % 4 {
                    0 => (20u8, 20u8, 20u8),
                    1 => (240, 30, 30),
                    2 => (30, 200, 60),
                    _ => (250, 250, 250),
                };
                data.extend_from_slice(&[r, g, b, 255]);
            }
        }
        djvu_rs::Pixmap {
            width: w,
            height: h,
            data,
        }
    }

    /// Standard deviation of luminance. The one number that separates "a picture" from "a
    /// rectangle of one colour", which is the difference this whole module turned out to hinge
    /// on. A tile of flat fill scores 0.00.
    fn luma_sd(img: &DynamicImage) -> f64 {
        let g = img.to_luma8();
        let n = g.len() as f64;
        let mean = g.iter().map(|&p| p as f64).sum::<f64>() / n;
        (g.iter().map(|&p| (p as f64 - mean).powi(2)).sum::<f64>() / n).sqrt()
    }

    fn encode(w: u32, h: u32, quality: EncodeQuality) -> Vec<u8> {
        PageEncoder::from_pixmap(&source_page(w, h))
            .with_quality(quality)
            .with_dpi(300)
            .encode()
            .expect("djvu-rs encodes its own pixmap")
    }

    /// **The regression this module exists to hold.** A `DjVuPhoto` page - `INFO + BG44`, no
    /// mask, which is the right profile for a photograph or a grayscale scan - decoded to a
    /// FLAT GREY RECTANGLE at every size Explorer asks for, on any page past roughly 3300 px
    /// on the long edge. Not a blank, not an error: a perfectly valid PNG of nothing, which is
    /// why every gate in the repo waved it through.
    ///
    /// Cause is upstream (djvu-rs 0.27 drops the background whenever the IW44 subsample lands
    /// at 4 or 8 and the file's first BG44 chunk is not a usable image on its own) and the
    /// workaround is in [`render_page`]. This test is written against the SYMPTOM rather than
    /// the workaround on purpose: it stays true whichever way the fix is eventually spelled,
    /// including after an upstream release lets us drop it.
    ///
    /// The fixtures are narrow and tall rather than page-shaped, which costs a tenth of the
    /// pixels to encode and trips exactly the same branch. What selects the subsample is
    /// `RENDER_CAP / page-long-edge` and nothing else, so 4267 px on the long edge is the line:
    /// 3300 decodes at subsample 2 and is the control, 4400 and 6600 land on 4 and were the
    /// broken ones. Verified against full-size 3400x4400 and 5100x6600 pages before being
    /// trimmed to this.
    #[test]
    fn photo_profile_pages_are_pictures_not_grey_rectangles() {
        // The four page sizes are independent; each runs on its own thread.
        std::thread::scope(|scope| {
            for (w, h) in [(1000u32, 1650u32), (1000, 3300), (1000, 4400), (1000, 6600)] {
                scope.spawn(move || {
                    let bytes = encode(w, h, EncodeQuality::Photo);
                    let img = extract(&bytes).unwrap_or_else(|| {
                        panic!("{w}x{h} DjVuPhoto page produced no cover at all")
                    });
                    let sd = luma_sd(&img);
                    assert!(
                        sd > 20.0,
                        "{w}x{h} DjVuPhoto page rendered flat (luma sd {sd:.2}) - the background \
                         layer was dropped, so this is a rectangle of fill colour, not a picture"
                    );
                });
            }
        });
    }

    /// The same defect wearing the other costume. A layered page keeps its ink, so it never
    /// looks empty - it just quietly loses every photograph on the page, which is far harder to
    /// notice and exactly as wrong. Measured against the same picture at a page size that
    /// decodes correctly, rather than a threshold pulled out of the air.
    ///
    /// A 3300 px long edge renders at the shipped cap with an IW44 subsample of 2, which never
    /// takes the broken shortcut; 4400 px lands on subsample 4, which does. At full page width
    /// those two scored 74 and 32 before the fix. Sizes stop at 4400 because djvu-rs cannot
    /// round-trip its OWN layered encode much past that - the decoder rejects the mask its
    /// encoder just wrote, with "JB2: image dimensions too large" - so a bigger fixture would
    /// fail for an unrelated reason while reading exactly like this bug. Real DjVuLibre files
    /// that size decode fine; the corpus scan is 5100x6600 and is covered separately below.
    #[test]
    fn layered_pages_keep_their_background_at_every_page_size() {
        let good = luma_sd(&extract(&encode(1000, 3300, EncodeQuality::Quality)).unwrap());
        let (w, h) = (1000u32, 4400u32);
        let img = extract(&encode(w, h, EncodeQuality::Quality))
            .unwrap_or_else(|| panic!("{w}x{h} layered page produced no cover at all"));
        let sd = luma_sd(&img);
        assert!(
            sd > good * 0.75,
            "{w}x{h} layered page rendered with luma sd {sd:.2} against {good:.2} for the same \
             picture at a page size that decodes correctly - the background layer is missing"
        );
    }

    /// A file with a baked TH44 thumbnail must not answer a big request with it. Encoders cap
    /// TH44 at 128 px on the long edge, so handing it back unconditionally - which is what
    /// shipped through 2.3.0 - gave Explorer's 768 px view a 128 px picture to stretch.
    #[test]
    fn a_baked_thumbnail_is_used_only_when_it_is_big_enough() {
        let bytes = encode_djvm_layered_shared_with_thumbnails(
            std::slice::from_ref(&source_page(1000, 3300)),
            EncodeQuality::Quality,
            300,
            None,
            usize::MAX,
            true,
        )
        .expect("djvu-rs encodes a bundle with thumbnails");

        // Guard the fixture itself: if the encoder ever stops writing TH44, this test would
        // pass for the wrong reason.
        let doc = djvu_rs::Document::from_bytes(bytes.clone()).unwrap();
        let baked = doc.page(0).unwrap().thumbnail().unwrap();
        let baked_long = baked
            .map(|t| t.width.max(t.height))
            .expect("fixture must actually carry a TH44 thumbnail");
        assert!(
            baked_long <= 128,
            "fixture assumption broken: TH44 came back {baked_long} px, so the 128 px cap this \
             test is about no longer holds"
        );

        // Small enough for the baked thumbnail: take it, it is nearly free.
        let small = extract_scaled(&bytes, Some(96)).unwrap();
        assert!(
            small.width().max(small.height()) <= baked_long,
            "a 96 px request should have been answered by the baked thumbnail"
        );

        // Bigger than the baked thumbnail: render, do not stretch.
        for target in [256u32, 768] {
            let img = extract_scaled(&bytes, Some(target)).unwrap();
            let long = img.width().max(img.height());
            assert!(
                long > baked_long,
                "a {target} px request came back {long} px - that is the baked {baked_long} px \
                 thumbnail being handed to a caller that asked for more than twice as much"
            );
        }

        // And a caller with no target at all (Convert) must never be answered with 128 px.
        let full = extract(&bytes).unwrap();
        assert!(
            full.width().max(full.height()) > baked_long,
            "Convert asked for real pixels and got the baked thumbnail"
        );
    }

    /// A bilevel page carries no BG44 at all, so [`render_page`] must take the plain path
    /// rather than asking for progressive chunks that do not exist.
    #[test]
    fn a_page_with_no_background_still_renders() {
        let src = source_page(1200, 1600);
        let mut bitmap = djvu_rs::Bitmap::new(src.width, src.height);
        for y in 0..src.height {
            for x in 0..src.width {
                let i = ((y * src.width + x) * 4) as usize;
                bitmap.set(x, y, src.data[i] < 128);
            }
        }
        let bytes = PageEncoder::from_bitmap(&bitmap)
            .with_quality(EncodeQuality::Lossless)
            .with_dpi(300)
            .encode()
            .expect("djvu-rs encodes a bilevel page");
        let img = extract(&bytes).expect("bilevel page produced no cover");
        assert!(
            luma_sd(&img) > 20.0,
            "bilevel page rendered flat - the mask was dropped"
        );
    }

    /// **The fuzzer's crash.** A mutated INFO chunk can declare a page WIDER than the bilevel
    /// mask the file actually ships: djvu-rs 0.27.0's render fallback loop clamped the source
    /// column to `page_w - 1` but then indexed `mask_row[px >> 3]`, where `mask_row` is only
    /// as wide as the mask itself - `index out of bounds: the len is N but the index is N`
    /// inside `composite_rows_bilevel_one`. Reached from here via `extract`, and from
    /// `container::fuzzseed::synthetic_djvu` via `container::extract_cover` in the always-on
    /// fuzz gate. This code runs inside Explorer's thumbnail host under `panic = "abort"`, so
    /// the crash used to abort the shell outright on a crafted `.djvu`.
    ///
    /// Fixed by vendoring djvu-rs with
    /// `crates/vendor/djvu-patches/djvu-rs.patch`, which clamps the fallback's source column
    /// to the mask's own width too, not just the page width.
    #[test]
    fn survives_a_page_width_wider_than_its_bilevel_mask() {
        let (w, h) = (32u32, 24u32);
        let mut bitmap = djvu_rs::Bitmap::new(w, h);
        for y in 0..h {
            for x in 0..w {
                bitmap.set(x, y, (x + y) % 2 == 0);
            }
        }
        let mut bytes = PageEncoder::from_bitmap(&bitmap)
            .with_quality(EncodeQuality::Lossless)
            .with_dpi(300)
            .encode()
            .expect("djvu-rs encodes a bilevel page");

        // Locate the INFO chunk and overwrite its width field: the container framing is a
        // 4-byte id + a 4-byte big-endian length (applied by djvu-rs's own IFF emission seam),
        // then a 10-byte payload whose first two bytes are the big-endian page width
        // (djvu-rs's `chunk_encode::encode_info`). Declaring the page wider than the encoded
        // bilevel mask is exactly the mismatch the fuzzer found.
        let info_at = bytes
            .windows(4)
            .position(|w| w == b"INFO")
            .expect("encoded page must carry an INFO chunk");
        let width_at = info_at + 4 + 4;
        let mutated_width = w * 4;
        bytes[width_at..width_at + 2].copy_from_slice(&(mutated_width as u16).to_be_bytes());

        // Must not panic - a rejected page (None) or a best-effort render (Some) are both an
        // acceptable answer to a corrupt INFO chunk; aborting the process is not.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| extract(&bytes)));
        assert!(
            result.is_ok(),
            "extract panicked on a page width wider than its bilevel mask - the fallback \
             compositor indexed past the end of the (narrower) mask row"
        );
    }

    /// Write the two `.djvu` corpus samples that the unit tests above cover in memory, so the
    /// END-TO-END gates cover them too: `check-render-sanity.ps1` renders the whole corpus
    /// through the shipped binary and flags a tile that is a flat rectangle, which is precisely
    /// the shape this bug took, and nothing in the corpus could produce one. The real
    /// `sample.djvu` is a DjVuLibre-encoded layered scan and decodes correctly either way, so
    /// it proves nothing here.
    ///
    /// Not downloaded, because nobody publishes a DjVuPhoto page as a test file, and not
    /// generated by `scripts/make-*-fixture.py` like the GIMP ones, because writing DjVu means
    /// an IW44 wavelet coder and a ZP arithmetic coder and we already link one. Run it by hand
    /// after changing the fixtures; `build-corpus.ps1` documents the same command:
    ///
    ///   cargo test --release --lib write_djvu_corpus_fixtures -- --ignored --nocapture
    #[test]
    #[ignore = "writes corpus fixtures on demand"]
    fn write_djvu_corpus_fixtures() {
        let corpus = st2k_base::testcorpus::dir();
        assert!(corpus.is_dir(), "no test-corpus at {}", corpus.display());

        // Page-shaped and past the 4267 px long edge where the IW44 subsample reaches 4, i.e.
        // an ordinary letter scan at 400 dpi. This one rendered FLAT GREY, sd 0.00.
        let photo = PageEncoder::from_pixmap(&source_page(3400, 4400))
            .with_quality(EncodeQuality::Photo)
            .with_dpi(400)
            .encode()
            .expect("encode DjVuPhoto page");
        let a = corpus.join("sample-djvu-photo.djvu");
        std::fs::write(&a, &photo).unwrap();

        // Carries a baked TH44 thumbnail, which every encoder caps at 128 px. This one used to
        // answer Explorer's 768 px view with that 128 px picture.
        let thumbed = encode_djvm_layered_shared_with_thumbnails(
            std::slice::from_ref(&source_page(2550, 3300)),
            EncodeQuality::Quality,
            300,
            None,
            usize::MAX,
            true,
        )
        .expect("encode bundle with thumbnails");
        let b = corpus.join("sample-djvu-thumbnail.djvu");
        std::fs::write(&b, &thumbed).unwrap();

        eprintln!("wrote {} ({} bytes)", a.display(), photo.len());
        eprintln!("wrote {} ({} bytes)", b.display(), thumbed.len());
    }

    /// `bytes` as a big book's head: the outer `FORM` declares 300 MB more than is there, as it
    /// does in the first window of a book too big to hold.
    fn as_big_book_head(bytes: &[u8]) -> Vec<u8> {
        let mut head = bytes.to_vec();
        let declared = u32::from_be_bytes(head[8..12].try_into().unwrap()) + (300 << 20);
        head[8..12].copy_from_slice(&declared.to_be_bytes());
        head
    }

    /// A book too big to hold gets page 1, read through the directory: the whole-file parser
    /// refuses the cut bundle (which is why a big book had no thumbnail), and the page read by
    /// offset, shared shape dictionary included, draws what the whole small book draws.
    #[test]
    fn a_big_book_gets_its_first_page_by_offset() {
        // Three page shapes, so the page drawn is identifiably the first.
        let pages = [
            source_page(600, 900),
            source_page(900, 600),
            source_page(500, 500),
        ];
        let book = encode_djvm_layered_shared_with_thumbnails(
            &pages,
            EncodeQuality::Quality,
            300,
            None,
            usize::MAX,
            true,
        )
        .expect("djvu-rs encodes a bundle");
        let mut real = vec![("encoded", book)];
        if let Some(scan) = st2k_base::testcorpus::read("sample.djvu") {
            real.push(("corpus sample.djvu", scan));
        }
        for (name, bytes) in real {
            let mut r = std::io::Cursor::new(&bytes);
            let parts = directory(&mut r).expect("a bundled directory");
            let page = parts.iter().find(|p| p.kind == 1).unwrap();
            let form = read_form(&mut r, page.offset).unwrap();
            eprintln!("{name}: page 1 includes {:?}", includes(&form));

            let head = as_big_book_head(&bytes);
            assert!(
                extract(&head).is_none(),
                "{name}: the whole-file parser read a cut bundle, so this proves nothing"
            );
            let got = first_page_seek(&mut std::io::Cursor::new(&head)).expect("page 1");
            let want = extract(&bytes).expect("the whole book draws");
            assert!(
                got.to_rgba8() == want.to_rgba8(),
                "{name}: {}x{} by offset, {}x{} whole",
                got.width(),
                got.height(),
                want.width(),
                want.height()
            );
        }
    }

    /// The encoder's bundle shares a JB2 shape dictionary, so the include path is what the test
    /// above exercises, not just a page that stands alone.
    #[test]
    fn the_encoded_book_shares_a_dictionary() {
        let book = encode_djvm_layered_shared_with_thumbnails(
            &[source_page(600, 900), source_page(900, 600)],
            EncodeQuality::Quality,
            300,
            None,
            usize::MAX,
            true,
        )
        .unwrap();
        let mut r = std::io::Cursor::new(&book);
        let parts = directory(&mut r).unwrap();
        let page = parts.iter().find(|p| p.kind == 1).unwrap();
        let ids = includes(&read_form(&mut r, page.offset).unwrap());
        assert!(
            ids.iter()
                .any(|id| parts.iter().any(|p| p.kind == 0 && &p.id == id)),
            "page 1 includes {ids:?}, none of them a shared component"
        );
        // A single page and garbage are not bundles.
        assert!(first_page_seek(&mut std::io::Cursor::new(encode(
            64,
            64,
            EncodeQuality::Quality
        )))
        .is_none());
        assert!(
            first_page_seek(&mut std::io::Cursor::new(b"AT&TFORM\0\0\0\x10DJVMDIRM")).is_none()
        );
    }

    /// The real corpus scan, which is what actually ships through this code. Skipped when the
    /// corpus is absent (it is a sibling of the repo and CI never checks it out).
    #[test]
    fn the_corpus_scan_still_decodes_to_a_picture() {
        let p = st2k_base::testcorpus::dir().join("sample.djvu");
        let Ok(bytes) = std::fs::read(&p) else {
            return;
        };
        for target in [None, Some(96), Some(256), Some(768)] {
            let img = extract_scaled(&bytes, target)
                .unwrap_or_else(|| panic!("corpus sample.djvu produced no cover at {target:?}"));
            let sd = luma_sd(&img);
            assert!(
                sd > 20.0 && img.width() > 1 && img.height() > 1,
                "corpus sample.djvu at {target:?} came back {}x{} with luma sd {sd:.2}",
                img.width(),
                img.height()
            );
        }
    }
}
