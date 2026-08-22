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
    // The size a render would actually come back at. That is the bar the baked thumbnail has
    // to clear, and it is bounded by the page as well as by the caller: for a page smaller than
    // the request, a thumbnail the size of the page IS the whole picture.
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
        for (w, h) in [(1000u32, 1650u32), (1000, 3300), (1000, 4400), (1000, 6600)] {
            let bytes = encode(w, h, EncodeQuality::Photo);
            let img = extract(&bytes)
                .unwrap_or_else(|| panic!("{w}x{h} DjVuPhoto page produced no cover at all"));
            let sd = luma_sd(&img);
            assert!(
                sd > 20.0,
                "{w}x{h} DjVuPhoto page rendered flat (luma sd {sd:.2}) - the background layer \
                 was dropped, so this is a rectangle of fill colour, not a picture"
            );
        }
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
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus");
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

    /// The real corpus scan, which is what actually ships through this code. Skipped when the
    /// corpus is absent (it is a sibling of the repo and CI never checks it out).
    #[test]
    fn the_corpus_scan_still_decodes_to_a_picture() {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus")
            .join("sample.djvu");
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
