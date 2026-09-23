//! The preview cascade's tiers: which decoder answers a byte buffer, and the last resorts when none does.

use super::*;

/// Apply the WIC decode's AVIF high-bit-depth curve fix when [`route_isobmff_wic_quirks`] says
/// it's needed, and log when we're falling back to the codec we deliberately tried to route
/// around (`magick_attempted`).
pub(super) fn finish_wic_fallback(
    img: DynamicImage,
    route: &WicQuirkRoute,
    magick_attempted: bool,
) -> DynamicImage {
    let img = if matches!(
        route.avif_verdict,
        color::AvifWicVerdict::NeedsHighDepthCurve
    ) {
        st2k_base::safety::log_debug("decode: undoing WIC's high-bit-depth AV1 transfer curve");
        color::undo_wic_high_depth_curve(img)
    } else {
        img
    };
    if magick_attempted {
        // Reaching WIC after we deliberately tried to avoid it means the thumbnail is
        // about to be produced by the codec we KNOW misreads this file, so say so
        // rather than returning a quietly wrong picture. A wrong-coloured tile still
        // beats no tile (it is what the Compact install shows anyway), but it must be
        // diagnosable — the alternative is issue #9's "some files are just wrong
        // sometimes", with nothing in the log to point at.
        st2k_base::safety::log_debug(
            "decode: fell back to WIC after routing around it — colours may be off",
        );
    }
    img
}

/// The tail of [`decode_any_with_wic_target`]'s tier chain, run once
/// [`route_isobmff_wic_quirks`] has decided WIC is the next thing to try (it either declined to
/// route around WIC, or its own route failed): WIC → TGA → ImageMagick (`external` only) → the
/// after-external RAW-preview retry → the reduced-IFD0 stash → the cheap embedded-JPEG scan.
/// Mirrors the original's linear fallthrough exactly, just moved off the caller's own
/// complexity budget.
pub(super) fn last_resort_tiers(
    bytes: &[u8],
    wic_thumbnail_cx: Option<u32>,
    raw_preview: RawPreviewOrder,
    external: bool,
    route: WicQuirkRoute,
    reduced_ifd0: Option<DynamicImage>,
) -> Result<DynamicImage> {
    let magick_attempted = route.magick_attempted;
    match wic_fallback(bytes, wic_thumbnail_cx) {
        Ok(img) => return Ok(finish_wic_fallback(img, &route, magick_attempted)),
        Err(e) => st2k_base::safety::log_debugf!("decode tier `WIC` failed: {e}"),
    }
    // TGA has no magic bytes, so the `image` guesser + magick-via-stdin both miss
    // it; detect it by a header sanity check and decode with an explicit format
    // BEFORE magick, so a real TGA skips a doomed (20s-capped) subprocess.
    match decode_tga(bytes) {
        Ok(img) => return Ok(img),
        Err(e) => st2k_base::safety::log_debugf!("decode tier `TGA` failed: {e}"),
    }
    // ImageMagick subprocess (the exotic long tail) + the full-fidelity after-external
    // RAW fallback. SKIPPED entirely when `external` is false: the classic in-shell menu
    // preview ([`decode_menu_preview`]) runs on explorer.exe's OWN UI thread and cannot
    // afford a subprocess (≤20s) there — it falls back to the cheap embedded-JPEG slice
    // below, or a caption-only tile.
    let mut last_err = route.magick_error.unwrap_or_else(|| Error::from(E_FAIL));
    if let Some(img) = try_external_tiers(
        bytes,
        wic_thumbnail_cx,
        raw_preview,
        external,
        magick_attempted,
        &mut last_err,
    ) {
        return Ok(img);
    }
    // The reduced-resolution IFD0 held back above. Every real decoder has now failed or is
    // absent, and a small genuine preview beats both the byte-scan carve below and a blank
    // tile — so this is where it is finally spent.
    if let Some(img) = reduced_ifd0 {
        return Ok(img);
    }
    // Last resort (CHEAP — a linear byte scan + image-tier decode, no subprocess, so the
    // menu path runs it too): every real decoder failed (or is absent — e.g. a clean
    // compact install with no Microsoft RAW Image Extension and no bundled ImageMagick).
    // If the file still embeds ANY decodable JPEG — a camera RAW's small EXIF thumbnail, a
    // document preview — show that rather than a blank tile. Strictly additive: only
    // reached AFTER every higher-fidelity tier above has failed, so it can't downgrade a
    // good result.
    if let Some(img) = try_embedded_jpeg_last_resort(bytes) {
        return Ok(img);
    }
    Err(last_err)
}

/// The `external`-only tail of [`last_resort_tiers`]: the capped ImageMagick subprocess and the
/// after-external camera-RAW preview retry. Returns the first image a tier produced, or `None`
/// when neither ran or both failed (magick's error is recorded in `last_err`).
fn try_external_tiers(
    bytes: &[u8],
    wic_thumbnail_cx: Option<u32>,
    raw_preview: RawPreviewOrder,
    external: bool,
    magick_attempted: bool,
    last_err: &mut Error,
) -> Option<DynamicImage> {
    if !external {
        return None;
    }
    if !magick_attempted {
        // Ask magick for no more than the caller's target edge. Rendering the fixed
        // 4096 cap and then throwing most of it away cost 15.6s on a 76 MP JPEG 2000
        // (issue #11) — over the preview pane's 12s budget, so the pane showed nothing
        // for a file that decodes perfectly well.
        match decode_via_magick_capped(bytes, wic_thumbnail_cx, raw_preview.fidelity()) {
            Ok(img) => return Some(finish_magick_output(img, bytes, false)),
            Err(e) => {
                st2k_base::safety::log_debugf!("decode tier `magick` failed: {e}");
                *last_err = e;
            }
        }
    }
    if raw_preview == RawPreviewOrder::AfterExternal {
        if let Some(img) = try_raw_preview_tier(bytes, wic_thumbnail_cx) {
            return Some(img);
        }
    }
    None
}

/// The picture's size as its own header declares it, for the formats the `image` crate can
/// read a header of (JPEG, PNG, TIFF, WebP, GIF, BMP, ...) plus PSD/PSB. A header-only read,
/// no pixels. `None` when no header is readable, which leaves [`decode_full_for_output`]'s
/// stand-in check switched off rather than guessing. For a TIFF (and the TIFF-based camera
/// RAWs) this is IFD0, which describes a small preview as often as the sensor, so a RAW whose
/// embedded preview is larger than its IFD0 is never refused on its account.
pub(crate) fn declared_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if let Some(dims) = psd_declared_dimensions(bytes) {
        return Some(dims);
    }
    image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()
}

/// Height and width straight off a PSD/PSB file header (big-endian, at bytes 14 and 18).
pub(super) fn psd_declared_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(b"8BPS") || bytes.len() < 22 {
        return None;
    }
    let be =
        |at: usize| u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
    Some((be(18), be(14)))
}

/// Longest edge at or below which a decode is a THUMBNAIL rather than a picture, for
/// [`refuse_a_preview_standing_in_for_the_picture`].
///
/// Measured off this repo's own RAW corpus, which is where the honest previews live:
/// `.kdc` 96x64, `.erf` 160x120, `.nef` 320x218, `.dcr` 380x252 — all baked thumbnails —
/// against `.fff` 1217x913 and the camera "review" JPEGs at 1024–2048 px that a Convert can
/// legitimately be satisfied by. 512 sits in the empty band between those two populations, so
/// a real preview is never called a stand-in and a postage stamp never passes for one.
pub(super) const STAND_IN_MAX_EDGE: u32 = 512;

/// Issue #41's rule, applied by [`decode_full_for_output`] alone: is `img` the picture, or a
/// postage stamp standing in for it?
///
/// ALL THREE conditions have to hold before anything is refused, and the last two are what
/// keep this from firing on honest work:
///
/// 1. The result is under a QUARTER of what the file's own header declares, on the longer
///    edge. A RAW whose IFD0 describes only its thumbnail therefore never trips it (the
///    preview is bigger than the declaration, not smaller).
/// 2. The result is at or below [`STAND_IN_MAX_EDGE`] — thumbnail-sized in absolute terms,
///    not merely smaller than a very large original. A 1217x913 camera preview of a 40 MP
///    sensor is a picture; it converts as it always has.
/// 3. The result is under our own [`MAGICK_MAX_EDGE_PX`] ceiling, which condition 2 already
///    implies and which is restated here because it is the load-bearing one if that floor is
///    ever raised: a 20000 px JPEG 2000 that ImageMagick capped at 4096, or a gigapixel scan
///    capped at [`limits::MAX_DIM`], is a real decode that hit a guard WE set.
///
/// What is left is the shape the issue reported: a 107x160 preview carved out of a 45 MP
/// photograph after every decoder refused the photograph itself.
pub(super) fn refuse_a_preview_standing_in_for_the_picture(
    img: DynamicImage,
    declared: Option<(u32, u32)>,
) -> Result<DynamicImage> {
    let Some((dw, dh)) = declared else {
        return Ok(img);
    };
    let (pw, ph) = (img.width(), img.height());
    let got = pw.max(ph);
    if got.saturating_mul(4) > dw.max(dh) || got > STAND_IN_MAX_EDGE || got >= MAGICK_MAX_EDGE_PX {
        return Ok(img);
    }
    Err(Error::new(
        E_FAIL,
        format!(
            "only a {pw}x{ph} preview embedded in this {dw}x{dh} picture could be decoded, \
             and it is too small to stand in for it"
        ),
    ))
}

/// Tiered decode: `image` crate → WIC → ImageMagick subprocess → headerless TGA,
/// except HEIC auxiliary-alpha files may prefer ImageMagick before WIC (see below).
/// Stops at the first tier that decodes. No resize, no orientation — raw pixels.
/// `wic_target` is a longest-edge hint for the WIC tier only (a scaling codec decodes
/// straight to it); every full-fidelity caller passes `None`.
pub(super) fn decode_any_with_wic_target(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    external: bool,
    wic_thumbnail_cx: Option<u32>,
) -> Result<DynamicImage> {
    // EPS is embedded-preview-only. Every ordinary caller tries
    // `container::extract_cover` before reaching this raster tier; if EPS bytes
    // still arrive here, no supported TIFF/EPSI/Photoshop preview was present.
    // Refuse them before image/WIC/ImageMagick/the lenient-JPEG fallback so a
    // nameless shell stream can never invoke a PostScript delegate or treat an
    // unrelated JPEG byte run as the file's declared preview.
    if crate::container::is_eps(bytes) {
        return Err(Error::from(E_FAIL));
    }
    // Per-tier breadcrumb: each tier's underlying error Display is logged before
    // we fall through, so a failed decode is diagnosable (`-Debug` on) instead of
    // every tier collapsing to a bare E_FAIL. Logging is gated by `log_debug`.
    if let Some(img) = try_jxl_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    if let Some(img) = try_dds_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    if let Some(img) = try_fits_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    // The WIC tiers get a target edge only from the ISOLATED hosts (`external`). A target
    // edge is what unlocks `MAX_SCALED_SOURCE_PIXELS`, the widened ceiling a thumbnail may
    // stream through because a hostile file costs a throwaway dllhost there; the in-process
    // path (the classic menu tile, on explorer.exe's own UI thread under panic=abort) keeps
    // the strict guard. This used to be a property of the call graph (`decode_cheap` passed
    // `None`) and a refactor quietly stopped it; the test that pins it only reached WIC once
    // COM was initialised in the test binary (2026-09-11). Decided HERE, once, for every WIC
    // call below.
    let wic_cx = if external { wic_thumbnail_cx } else { None };
    if let Some(img) = try_wic_thumbnail_fastpath(bytes, wic_cx) {
        return Ok(img);
    }
    // A TIFF whose IFD0 says `NewSubfileType = reduced-resolution` is a container whose
    // MAIN image lives elsewhere (SubIFDs), and the `image` crate only ever decodes IFD0.
    // Letting the first tier answer from it is how six camera-RAW formats thumbnailed from
    // a postage stamp — and a Kodak `.dcr` from a black placeholder — while WIC decoded the
    // same files at full resolution. So we keep the decode as a LAST-RESORT stash and let
    // the real tiers run: nothing that rendered before can stop rendering, it just stops
    // winning. See `streamsrc::tiff_ifd0_is_reduced`.
    let mut reduced_ifd0: Option<DynamicImage> = None;
    match try_image_tier(bytes, wic_thumbnail_cx) {
        ImageTierOutcome::Decoded(img) => return Ok(img),
        ImageTierOutcome::ReducedIfd0(img) => reduced_ifd0 = Some(img),
        ImageTierOutcome::Failed => {}
    }
    if let Some(img) = try_raw_raster_tier(bytes, wic_thumbnail_cx) {
        return Ok(img);
    }
    // Camera-RAW fast path for preview fidelity. A RAW file embeds a JPEG the
    // camera already rendered; decoding that is ~10–30× faster than demosaicing.
    // Keep this BEFORE WIC/magick only for thumbnails/menu previews. Full-fidelity
    // callers use the late fallback below so Convert/Resize/Image-info prefer real
    // WIC/ImageMagick decoders whenever they are available.
    if raw_preview == RawPreviewOrder::BeforeExternal {
        if let Some(img) = try_raw_preview_tier(bytes, wic_thumbnail_cx) {
            return Ok(img);
        }
    }
    // Two things Microsoft's WIC codecs get wrong on ISOBMFF images, both of which we can
    // detect from the container CHEAPLY and route around when the Full install's external
    // tier is available. In both cases WIC stays the fallback: on the Compact install (no
    // ImageMagick) a slightly wrong thumbnail still beats no thumbnail at all.
    match route_isobmff_wic_quirks(bytes, external, wic_cx, raw_preview.fidelity()) {
        Ok(img) => Ok(img),
        Err(route) => last_resort_tiers(bytes, wic_cx, raw_preview, external, route, reduced_ifd0),
    }
}

/// JPEG XL: our own pure-Rust tier, FIRST and signature-gated. The `image` crate and
/// WIC don't decode jxl, and build-release.ps1 strips the jxl coder out of the bundled
/// magick - so without this an ADVERTISED format silently fails to thumbnail on a
/// clean install. On failure the caller falls through to the tiers below (a machine
/// with a full ImageMagick could yet decode it).
pub(super) fn try_jxl_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if !is_jxl(bytes) {
        return None;
    }
    match decode_jxl(bytes, wic_thumbnail_cx) {
        Ok(img) => Some(img),
        Err(e) => {
            st2k_base::safety::log_debugf!("decode tier `jxl` failed: {e}");
            None
        }
    }
}

/// The simple rasters (binary PNM, PAM, PFM, farbfeld, TGA) the `image` tier declined - a
/// picture past its side limit, or a layout it does not read: our own row reader, which
/// shrinks as it reads (see `rawraster.rs`), the same one a file past the input ceiling gets.
pub(super) fn try_raw_raster_tier(
    bytes: &[u8],
    wic_thumbnail_cx: Option<u32>,
) -> Option<DynamicImage> {
    if !rawraster::is_raw_raster(bytes) {
        return None;
    }
    let edge = wic_thumbnail_cx.unwrap_or(limits::MAX_DIM);
    rawraster::decode_scaled(std::io::Cursor::new(bytes), edge)
}

/// FITS: our own reader, signature-gated, ahead of ImageMagick (see `fits.rs`: it finds an image
/// in an extension, reads a file of any size, and scales a 16-bit exposure instead of drawing
/// it black). The same reader answers past the input ceiling, in the stream cascade and by
/// path, so a small and a big file of the same picture look alike. A full-fidelity caller
/// (`None`) gets the picture at the decoders' side limit.
pub(super) fn try_fits_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if !fits::is_fits(bytes) {
        return None;
    }
    let edge = wic_thumbnail_cx.unwrap_or(limits::MAX_DIM);
    let img = fits::decode_scaled(std::io::Cursor::new(bytes), edge);
    if img.is_none() {
        st2k_base::safety::log_debug("decode tier `fits` found no image it reads");
    }
    img
}

/// DDS: our own tier, magic-gated, ahead of `image` because it OWNS the format -
/// BC1–BC7 (incl. BC6H HDR) plus the uncompressed layouts, all pure Rust. The `image`
/// crate stops at DXT1/3/5, WIC's DDS codec stops at the same three, and ImageMagick
/// (FULL install only) can't read BC4/BC5-signed/BC6H/float DDS at all - so before
/// this, BC7 (what every modern game texture uses) needed a 20 s subprocess and BC6H
/// worked nowhere. Failure falls through to the tiers below, so no DDS that
/// thumbnailed before can regress. See `dds.rs`.
pub(super) fn try_dds_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if !is_dds(bytes) {
        return None;
    }
    // Textures ship their own thumbnail chain; use it. A 16k BC7 texture is 268 MP at
    // level 0 and has a 256-px mip a few hundred KB in. Full-fidelity callers pass
    // `None` and keep level 0.
    match decode_dds(bytes, wic_thumbnail_cx) {
        // BC6H and the float layouts come back linear-float, tone-mapped here
        // exactly like the EXR/Radiance results below.
        Ok(img) => Some(
            if matches!(
                img,
                DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
            ) {
                tone_map_float(&img)
            } else {
                img
            },
        ),
        Err(e) => {
            st2k_base::safety::log_debugf!("decode tier `dds` failed: {e}");
            None
        }
    }
}

/// Three formats (BMP, GIF, WebP) prefer the OS codec for a bounded thumbnail ask, for the
/// same underlying reason: WIC SCALES WHILE IT DECODES, and the pure-Rust tier cannot - it
/// materialises the whole image and then shrinks it. That costs nothing on a small
/// file and a great deal on a large one, which is exactly what the size-tiered speed
/// baseline exists to show: BMP measured 2.3 ms at 0.08 MP but 258.6 ms at 12 MP
/// against Windows' 22.1 ms (11.7x), the single worst ratio in the whole matrix.
///
/// Still WebP prefers the OS codec when this is a bounded thumbnail ask: Windows' WebP
/// codec decodes ~3.8x faster than the pure-Rust tier (measured on a 1279x1280 sample:
/// ~27 ms vs ~103 ms, and the cost is the decode itself - flat whether the target is 64 px
/// or 1024 px). STRICTLY a fast path in FRONT of the existing one: the codec is an optional
/// Store extension, so any failure - absent codec included - falls straight through to the
/// `image` tier unchanged, which is also what keeps the Compact install and codec-less
/// machines exactly as they were. Animated WebP is excluded because FRAME CHOICE is a
/// decoder decision (`sample-decoy-frames.webp` pins first-frame selection to the verified
/// path), and ICC-tagged WebP is excluded so colour management stays where it is verified
/// today. GIF is admitted for that same scaling reason, but only as a single full-canvas
/// frame: an animation's frame choice belongs to the decoder, and the `image` tier composites
/// a partial frame onto the full canvas while WIC returns the frame at its own size.
/// Full-fidelity callers (`wic_thumbnail_cx == None`, e.g. Convert) are excluded on
/// purpose: their output bytes must not change decoder mid-release for a speed win the
/// non-interactive path doesn't need.
pub(super) fn try_wic_thumbnail_fastpath(
    bytes: &[u8],
    wic_thumbnail_cx: Option<u32>,
) -> Option<DynamicImage> {
    if wic_thumbnail_cx.is_none()
        || !(webp_prefers_wic(bytes) || bmp_prefers_wic(bytes) || gif_prefers_wic(bytes))
    {
        return None;
    }
    match wic_fallback(bytes, wic_thumbnail_cx) {
        Ok(img) => Some(img),
        Err(e) => {
            st2k_base::safety::log_debugf!(
                "decode: WIC fast path unavailable, using the image tier: {e}"
            );
            None
        }
    }
}

/// What the `image`-crate tier produced: a usable decode, a reduced-resolution IFD0
/// held back as a fallback stash, or nothing.
pub(super) enum ImageTierOutcome {
    Decoded(DynamicImage),
    ReducedIfd0(DynamicImage),
    Failed,
}

/// The `image` crate tier, including the reduced-resolution-IFD0 TIFF special case and
/// the HDR-float tone-map. See [`decode_any_with_wic_target`]'s callsite comment for
/// why a reduced IFD0 is stashed rather than answered from immediately.
pub(super) fn try_image_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> ImageTierOutcome {
    match decode_with_image_alloc_raw(bytes, MAX_ALLOC) {
        // The float exclusion is not fussiness: a 32-bit-float TIFF has to go through the
        // tone map below to become 8-bit sRGB at all, and stashing one would hand a caller
        // linear floats where it expects pixels. No camera-RAW preview IFD is float, so this
        // costs the fix nothing and closes the one shape that would break.
        Ok((img, icc))
            if crate::streamsrc::tiff_ifd0_is_reduced(bytes)
                && !matches!(
                    img,
                    DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
                ) =>
        {
            // Color-manage immediately (not after a reduce) — `reduced_ifd0_serves` below
            // reads the pixel content (`luma_sd`), so it must see the same colour-managed
            // pixels a served result would actually return.
            let img = apply_icc_to_srgb(img, icc);
            // Big enough for this tile AND not a blank placeholder: answer from it now and
            // skip the real decoders. This is the difference between a Hasselblad thumbnail
            // costing 1.3 seconds and costing nothing. See `reduced_ifd0_serves` for why the
            // content test is not optional.
            if reduced_ifd0_serves(&img, wic_thumbnail_cx) {
                st2k_base::safety::log_debug(
                    "decode tier `image`: reduced-resolution IFD0 covers this tile and has content - using it",
                );
                return ImageTierOutcome::Decoded(img);
            }
            st2k_base::safety::log_debug(
                "decode tier `image`: TIFF IFD0 is reduced-resolution - held as fallback",
            );
            ImageTierOutcome::ReducedIfd0(img)
        }
        Ok((img, icc)) => finish_image_tier(img, icc, wic_thumbnail_cx),
        Err(e) => {
            st2k_base::safety::log_debugf!("decode tier `image` failed: {e}");
            ImageTierOutcome::Failed
        }
    }
}

/// The successful `image`-tier decode: an HDR float (EXR/Radiance) result is reduced then
/// tone-mapped to 8-bit sRGB, and an ordinary image is reduced then colour-managed. See
/// [`try_image_tier`].
fn finish_image_tier(
    img: DynamicImage,
    icc: Option<Vec<u8>>,
    wic_thumbnail_cx: Option<u32>,
) -> ImageTierOutcome {
    // HDR float (EXR/Radiance) decodes to 32-bit linear float, which can't
    // be saved as PNG/JPEG or turned into an 8-bit DIB directly. Tone-map
    // it to 8-bit sRGB ourselves (native Rust) - no ImageMagick subprocess,
    // so EXR/HDR also work on the compact (no-magick) install.
    if matches!(
        img,
        DynamicImage::ImageRgb32F(_) | DynamicImage::ImageRgba32F(_)
    ) {
        // REDUCE FIRST, when the caller only wants a tile. A 12 MP Radiance file is
        // 144 MB of float and the tone map then runs over every one of those pixels
        // to produce a 256 px thumbnail. Averaging in LINEAR light before the curve
        // is also the physically correct order, and it is not a new idea here:
        // `exrscale::decode_scaled` has always box-averaged OpenEXR into the target
        // grid and handed the caller a small float image to tone-map. This gives the
        // formats that reach the `image` tier (Radiance .hdr, float PNM, jxl HDR) the
        // same treatment. Full-fidelity callers pass `None` and are untouched.
        let img = match wic_thumbnail_cx {
            Some(cx) => pre_reduce(img, cx),
            None => img,
        };
        // A no-op for float variants (apply_icc_to_srgb's match falls through to
        // `other => other` for them), kept for symmetry with the paths above.
        let img = apply_icc_to_srgb(img, icc);
        return ImageTierOutcome::Decoded(tone_map_float(&img));
    }
    // The ordinary successful decode: for a thumbnail request, reduce FIRST and
    // colour-manage the small result, instead of running the CMS transform over
    // every source pixel only to immediately throw most of them away. For a
    // non-sRGB profile that averages gamut-encoded values before the transform: a
    // deviation of the same order as the gamma-space box reduce every thumbnail
    // already accepts, visible at most as a slight shift on saturated edges, and the
    // accepted price of not colour-managing a 50-megapixel source for a 256 px tile.
    // Full-fidelity callers (`wic_thumbnail_cx == None`) are unaffected — no
    // reduction happens, and the transform runs on every pixel.
    let img = match wic_thumbnail_cx {
        Some(cx) => pre_reduce(img, cx),
        None => img,
    };
    ImageTierOutcome::Decoded(apply_icc_to_srgb(img, icc))
}

/// Cheap magic-byte gate for [`try_raw_preview_tier`]: does `bytes` at least start like a
/// TIFF-based RAW container (classic or BigTIFF), or one of the handful of non-TIFF RAW
/// signatures? Every HEIC/AVIF/JXR/WebP/etc. that reaches [`decode_any_with_wic_target`]
/// used to pay an O(file) embedded-JPEG scan here for a preview those containers never
/// carry — this is a byte-count check, not a decode, so it costs nothing to run first.
/// Deliberately looser than `streamsrc::rawsniff::looks_like_raw_container` (no extension
/// or IFD-marker refinement): a false positive here only means the real scan below still
/// runs, same as before, while a false negative would regress a RAW that decoded fine
/// yesterday — so this stays a strict superset of "might be RAW", not a precise classifier.
pub(super) fn looks_raw_container(bytes: &[u8]) -> bool {
    bytes.starts_with(b"II\x2A\0")
        || bytes.starts_with(b"MM\0\x2A")
        || bytes.starts_with(b"II\x2B\0")
        || bytes.starts_with(b"MM\0\x2B")
        // Canon CRW (CIFF, not TIFF at all): "II" little-endian marker followed by 0x1A00
        // rather than TIFF's 0x2A00. 2026-09-05 audit F38: missing this signature meant a
        // .crw skipped the embedded-preview carve here and fell through to the far slower
        // named-RAW/ImageMagick demosaic path even though `largest_embedded_jpeg` below
        // does not assume any TIFF/IFD structure and finds the JPEG fine once it runs.
        || bytes.starts_with(b"II\x1A\0")
        || bytes.starts_with(b"FUJIFILMCCD-RAW")
        || bytes.starts_with(b"FFF\0")
        || bytes.starts_with(b"FOVb")
        || bytes.starts_with(b"\0MRM")
        || bytes.starts_with(b"IIRO")
        || bytes.starts_with(b"MMOR")
        || bytes.starts_with(b"IIU\0")
        || has_crx_ftyp(bytes)
}

/// Does `bytes` carry the ISOBMFF `ftyp` box of a Canon CR3/CrX RAW (`crx `/`cr3 `)? The
/// non-TIFF signature check of [`looks_raw_container`].
fn has_crx_ftyp(bytes: &[u8]) -> bool {
    bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && (&bytes[8..12] == b"crx " || &bytes[8..12] == b"cr3 ")
}

/// Camera-RAW fast path: a RAW file embeds a JPEG the camera already rendered, ~10–30×
/// faster to decode than demosaicing. Shared by both the before-external and
/// after-external call sites in [`decode_any_with_wic_target`].
pub(super) fn try_raw_preview_tier(
    bytes: &[u8],
    wic_thumbnail_cx: Option<u32>,
) -> Option<DynamicImage> {
    if !looks_raw_container(bytes) {
        return None;
    }
    match decode_raw_preview(bytes, wic_thumbnail_cx) {
        Ok(img) => Some(img),
        Err(e) => {
            st2k_base::safety::log_debugf!("decode tier `raw-preview` failed: {e}");
            None
        }
    }
}

/// Outcome of [`route_isobmff_wic_quirks`] when it does NOT resolve the decode itself:
/// what the WIC fallback and the external tier below still need to know.
pub(super) struct WicQuirkRoute {
    /// Set once ImageMagick was invoked (or attempted) to route around a known-bad WIC
    /// decode, so the WIC fallback can log why colours may still be off, and the
    /// external tier below can skip a redundant magick attempt.
    pub(super) magick_attempted: bool,
    /// WIC's transfer-curve verdict for this AVIF (or `Trusted` when the file isn't
    /// AVIF/HEIC at all), needed by the WIC fallback to decide whether to invert WIC's
    /// high-bit-depth curve.
    pub(super) avif_verdict: color::AvifWicVerdict,
    /// Set when magick was attempted here and failed, so it becomes the final
    /// fallback error instead of a generic E_FAIL.
    pub(super) magick_error: Option<Error>,
}

/// Two things Microsoft's WIC codecs get wrong on ISOBMFF images, both of which we can
/// detect from the container CHEAPLY and route around when the Full install's external
/// tier is available. In both cases WIC stays the eventual fallback: on the Compact
/// install (no ImageMagick) a slightly wrong thumbnail still beats no thumbnail at all.
///
///  * HEIC: the HEVC codec accepts auxiliary-alpha files and returns an opaque image.
///    Gated on a checked `auxC` property carrying the exact HEVC alpha identifier.
///  * AVIF: the AV1 codec misreads the `nclx` colour box that libaom writes by default,
///    shifting colour on exactly the files `avifenc`/`ffmpeg` produce (issue #9).
///
/// Returns `Ok` when the decode is already resolved (avif-mf or magick succeeded), or
/// `Err(route)` with what the caller needs to continue to the WIC fallback.
pub(super) fn route_isobmff_wic_quirks(
    bytes: &[u8],
    external: bool,
    wic_thumbnail_cx: Option<u32>,
    fidelity: Fidelity,
) -> std::result::Result<DynamicImage, WicQuirkRoute> {
    let wic_hevc_alpha = isobmff_has_hevc_aux_alpha(bytes);
    // Three outcomes, not two. Most high-bit-depth AVIF used to land in the ImageMagick bucket
    // purely because the old predicate was a bool: WIC's error there is a pure transfer curve
    // we can invert in-process for microseconds, so it now stays on the cheap path and gets
    // corrected afterwards (~400 ms -> ~114 ms, and worst channel error 11 -> 1, i.e. BETTER
    // colour than the subprocess route it replaces). Only the genuinely unrecoverable case -
    // the 8-bit matrix error, where WIC clips as it converts - still pays for magick.
    let avif_verdict = if wic_hevc_alpha {
        color::AvifWicVerdict::Trusted
    } else {
        color::avif_wic_verdict(bytes)
    };
    let wic_avif_color = matches!(avif_verdict, color::AvifWicVerdict::Untrusted);
    let magick_attempted = external && (wic_hevc_alpha || wic_avif_color);
    if !magick_attempted {
        return Err(WicQuirkRoute {
            magick_attempted,
            avif_verdict,
            magick_error: None,
        });
    }
    let why = if wic_hevc_alpha {
        "HEIC auxiliary alpha"
    } else {
        "AVIF nclx colour"
    };
    st2k_base::safety::log_debugf!("decode: routing around WIC ({why})");
    // The 8-bit bucket first tries the OS's own AV1 decoder via Media Foundation
    // (decode/avifmf.rs): same correct colour as ImageMagick, no subprocess, ~150 ms of
    // the ~180 ms this route used to cost. Narrowly gated and best-effort - anything it
    // declines (alpha, wide gamut, MF absent, decode failure) proceeds to magick exactly
    // as before, so this can only ever be faster, never different.
    if wic_avif_color {
        if let Some(img) = avifmf::decode_8bit_avif_via_mf(bytes, wic_thumbnail_cx) {
            st2k_base::safety::log_debug("decode: tier `avif-mf` decoded the 8-bit AVIF");
            return Ok(img);
        }
    }
    // Ask magick for no more than the caller's target edge, exactly as the generic
    // magick tier below already does. This route used to take the uncapped
    // `decode_via_magick`, so a 256 px Explorer tile rendered the full 4096 px guard
    // and threw almost all of it away - then PNG-encoded that surface and decoded it
    // back. Measured on a 3000x2000 AVIF at a 256 px target: 10-bit 1261 ms -> 400 ms,
    // 8-bit 638 ms -> 388 ms. Nothing about the colour fix needs the larger render:
    // the ICC below is applied from the ORIGINAL container, not magick's output, and
    // full-fidelity callers reach here with `wic_thumbnail_cx == None` (uncapped) as
    // before.
    match decode_via_magick_capped(bytes, wic_thumbnail_cx, fidelity) {
        // `decode_via_magick` passes `-strip`, so the profile magick would otherwise
        // have carried into its PNG output is gone by the time we read it back. Apply
        // it here from the ORIGINAL container instead, exactly as the WIC path does,
        // or a wide-gamut file routed here would come out in raw Adobe RGB / P3
        // numbers - the same "decoded right, then threw the profile away" fault that
        // was fixed for JPEG XL in 1.7.1.
        Ok(img) => Ok(finish_magick_output(img, bytes, true)),
        Err(e) => {
            st2k_base::safety::log_debugf!("decode tier `magick ({why})` failed: {e}");
            Err(WicQuirkRoute {
                magick_attempted,
                avif_verdict,
                magick_error: Some(e),
            })
        }
    }
}

/// What ImageMagick hands back, made displayable. An HDR AVIF/HEIC (PQ or HLG `nclx`, issue
/// #39) comes back as its raw transfer-encoded signal - magick applies no EOTF - and shown as
/// sRGB that is a dark, flat picture (the grey ramp's 203-nit white reads 148 of 255). It
/// goes through the PNG `cICP` conversion and the float tone map, exactly as an HDR JPEG XL
/// does since #38, and lands at 187 like every other HDR source. Everything else is untouched
/// here except for `icc`: the routed ISOBMFF tier has always applied the container's own
/// profile afterwards (see its call site), the generic last-resort tier never has, and this
/// keeps both exactly as they were for every SDR file.
pub(super) fn finish_magick_output(img: DynamicImage, bytes: &[u8], icc: bool) -> DynamicImage {
    if let Some(cicp) = color::isobmff_hdr_cicp(bytes) {
        if let Some(linear) = cicp_hdr_to_linear(&img, &cicp) {
            return tone_map_float(&linear);
        }
    }
    if icc {
        apply_icc_to_srgb(img, color::isobmff_color_icc(bytes))
    } else {
        img
    }
}

/// Last resort (CHEAP - a linear byte scan + image-tier decode, no subprocess, so the
/// menu path runs it too): every real decoder failed (or is absent - e.g. a clean
/// compact install with no Microsoft RAW Image Extension and no bundled ImageMagick).
/// If the file still embeds ANY decodable JPEG - a camera RAW's small EXIF thumbnail, a
/// document preview - show that rather than a blank tile. Strictly additive: only
/// reached AFTER every higher-fidelity tier above has failed, so it can't downgrade a
/// good result.
pub(super) fn try_embedded_jpeg_last_resort(bytes: &[u8]) -> Option<DynamicImage> {
    let jpeg = largest_embedded_jpeg(bytes, LENIENT_RAW_PREVIEW)?;
    match decode_with_image(jpeg) {
        Ok(img) => Some(img),
        Err(e) => {
            st2k_base::safety::log_debugf!("decode tier `embedded-jpeg (lenient)` failed: {e}");
            None
        }
    }
}

/// Below this luminance standard deviation, a reduced-resolution IFD0 is a PLACEHOLDER, not a
/// preview, and must not be allowed to answer a request.
///
/// Measured across every corpus RAW that carries one
/// (`reduced_ifd0_evidence::what_every_raw_sample_holds_in_its_reduced_ifd0`):
///
/// ```text
///   sample.dcr    380x252   luma sd   0.91   <- Kodak's BLANK placeholder
///   sample.nef    320x218   luma sd  36.47   <- the least detailed REAL preview
///   sample.kdc     96x64    luma sd  41.76
///   sample.3fr    320x240   luma sd  59.23
///   sample.erf    160x120   luma sd  62.80
///   sample.fff   1217x913   luma sd  74.23
/// ```
///
/// 8.0 sits about nine times above the placeholder and four times below the faintest real
/// preview, which is as wide a gap as a threshold in this repo has ever had. It is deliberately
/// nowhere near the middle: being wrong towards "decode it properly" costs a second, and being
/// wrong towards "ship the placeholder" is the black-tile bug all over again.
pub(super) const REDUCED_IFD0_MIN_SD: f64 = 8.0;

/// May a held-back reduced-resolution IFD0 answer THIS request outright, skipping the real
/// decoders entirely?
///
/// Only when BOTH hold, and the second one is the whole reason this is a function rather than a
/// size comparison inline:
///
/// 1. **It covers the tile without being enlarged.** A thumbnail request carries its target
///    edge; a full-fidelity caller (Convert, Resize, Image-info) passes `None` and is never
///    served from here, because their output is the real image at real resolution. Enlarging a
///    320 px preview into a 768 px tile is exactly the bug 2.3.1 fixed, so the long edge must
///    already reach the target.
/// 2. **It actually contains a picture.** SIZE IS NOT EVIDENCE OF CONTENT. A Kodak `.dcr` ships
///    a 380x252 IFD0 that is blank, comfortably bigger than a 96 or 256 px tile, and returning
///    it gives a black square. That file is why the reduced IFD0 became a last resort in the
///    first place, and skipping the content test would reintroduce it verbatim.
///
/// What this buys: `.fff` (1217x913) answers all three of Explorer's sizes from the preview
/// instead of a full Hasselblad decode, and `.3fr` (320x240) answers the 96 and 256 px views,
/// still decoding properly for 768. Those two were the slowest formats in the product at
/// roughly 1300 ms and 1150 ms.
pub(super) fn reduced_ifd0_serves(img: &DynamicImage, target_edge: Option<u32>) -> bool {
    let Some(cx) = target_edge else {
        return false; // full-fidelity caller: never
    };
    let long_edge = img.width().max(img.height());
    if cx == 0 || long_edge < cx {
        return false;
    }
    luma_sd(img) >= REDUCED_IFD0_MIN_SD
}

/// Standard deviation of luminance: the one number that separates a picture from a rectangle of
/// one colour. Flat fill scores 0.00.
pub fn luma_sd(img: &DynamicImage) -> f64 {
    let g = img.to_luma8();
    let n = g.len() as f64;
    if n == 0.0 {
        return 0.0;
    }
    let mean = g.iter().map(|&p| f64::from(p)).sum::<f64>() / n;
    (g.iter()
        .map(|&p| (f64::from(p) - mean).powi(2))
        .sum::<f64>()
        / n)
        .sqrt()
}
