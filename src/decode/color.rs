//! Colour management and HDR tone mapping.
//!
//! Everything that turns a decoder's raw samples into displayable sRGB: embedded ICC
//! profiles, the ISOBMFF `colr` box (AVIF/HEIC, including the CICP Display-P3 signal
//! iPhones use), CMYK/YCCK JPEG, and the linear-float (EXR/Radiance/jxl) tone map.
//! All pure Rust - no ImageMagick, no C colour engine.

use super::*;
mod avifwic;
mod isobmff;
use isobmff::*;
mod tiffprofile;
#[cfg(test)]
pub(super) use avifwic::av1_obus_color_config;
#[cfg(test)]
pub(super) use avifwic::avif_wic_class_of;
pub(super) use avifwic::{
    avif_wic_verdict, isobmff_hdr_cicp, undo_wic_high_depth_curve, AvifWicVerdict,
};
pub(super) use isobmff::{isobmff_color_icc, isobmff_has_hevc_aux_alpha};
pub(super) use tiffprofile::tiff_icc;

/// Write every pixel of an RGBA-float source into `out` through `map`, narrowing alpha to
/// 8 bits and OR-ing into `any_alpha` whether any pixel was non-opaque (the all-transparent
/// RGB fix in `tone_map_float` depends on that flag).
fn tone_map_rgba32f(
    out: &mut image::RgbaImage,
    src: &image::Rgba32FImage,
    map: impl Fn(f32) -> u8,
    any_alpha: &mut bool,
) {
    for (o, s) in out.pixels_mut().zip(src.pixels()) {
        let [r, g, b, a] = s.0;
        let alpha = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        *any_alpha |= alpha != 0;
        *o = image::Rgba([map(r), map(g), map(b), alpha]);
    }
}

/// Tone-map a 32-bit linear-float HDR image (EXR/Radiance) to 8-bit sRGB, in pure
/// Rust: the Reinhard global operator `x/(1+x)` compresses the unbounded range,
/// then a linear→sRGB transfer encodes it for display. Replaces an ImageMagick
/// subprocess for this whole format class (and lets EXR/HDR work without magick).
/// Non-finite / negative samples are clamped to 0.
///
/// Matches the concrete `Rgb32F`/`Rgba32F` variant and iterates its own buffer directly
/// rather than calling `to_rgba32f()` first: every call site already gates on one of
/// those two variants (readers.rs, decode.rs, tiers.rs), so the conversion was always a
/// redundant full-image copy — a second W*H*16B allocation on top of the one this
/// function itself makes.
pub(super) fn tone_map_float(img: &DynamicImage) -> DynamicImage {
    let map = |c: f32| -> u8 {
        let c = if c.is_finite() && c > 0.0 { c } else { 0.0 };
        let tone = c / (1.0 + c); // Reinhard
        let srgb = if tone <= 0.003_130_8 {
            12.92 * tone
        } else {
            1.055 * tone.powf(1.0 / 2.4) - 0.055
        };
        (srgb * 255.0 + 0.5).clamp(0.0, 255.0) as u8
    };

    let (w, h) = (img.width(), img.height());
    let mut out = image::RgbaImage::new(w, h);
    let mut any_alpha = false;
    match img {
        // No alpha channel at all: every pixel is opaque, matching what `to_rgba32f()`
        // used to synthesize (a=1.0) for this variant.
        DynamicImage::ImageRgb32F(buf) => {
            for (o, s) in out.pixels_mut().zip(buf.pixels()) {
                let [r, g, b] = s.0;
                *o = image::Rgba([map(r), map(g), map(b), 255]);
            }
            any_alpha = true;
        }
        DynamicImage::ImageRgba32F(buf) => tone_map_rgba32f(&mut out, buf, map, &mut any_alpha),
        // Not reached by any current call site (all gate on the two variants above), but
        // kept total rather than panicking under panic=abort if one ever calls in unguarded.
        other => {
            let src = other.to_rgba32f();
            tone_map_rgba32f(&mut out, &src, map, &mut any_alpha);
        }
    }
    // VFX render passes (emission/environment/AOV EXRs) legitimately carry RGB with
    // the ENTIRE alpha channel at 0 — honoring that verbatim hands the caller a
    // fully-transparent image the `is_fully_transparent` watchdog then rejects, so
    // the file shows a default icon while every image viewer shows its RGB fine.
    // When ALL alpha is 0 there is no compositing intent to preserve; show the RGB
    // opaque instead. Partial alpha stays untouched. (Rgb32F sources are always
    // opaque above, so this only fires on genuinely all-transparent RGBA floats.)
    if !any_alpha {
        for px in out.pixels_mut() {
            px.0[3] = 255;
        }
    }
    DynamicImage::ImageRgba8(out)
}

/// The ICC profile a JPEG carries in its APP2 segments, reassembled, or `None` if it has none.
///
/// WIC does NOT surface it. Its JPEG decoder answers `GetColorContexts` with an Exif-flag
/// context rather than a profile one, so [`super::wic::wic_icc`] correctly returns `None` and
/// the scaled JPEG fast path handed back RAW wide-gamut numbers: an AdobeRGB file whose sRGB
/// rendering is rgb(8,200,5) came out as rgb(113,199,48), which is the same class of bug as
/// issue #9, in a different codec. The `image` tier never had it, because the crate's decoder
/// exposes `icc_profile()` and we already apply it - which is exactly why nothing caught this
/// until a JPEG small enough to have stayed on that tier was routed away from it.
///
/// The profile is split across as many APP2 segments as it needs (each capped at 64 KB), each
/// prefixed `ICC_PROFILE\0` plus a 1-based chunk number and the chunk count. A profile is only
/// returned when EVERY declared chunk is present, so a truncated read - the callers pass a
/// bounded head, not the whole file - yields nothing rather than a corrupt profile.
pub(super) fn jpeg_icc(b: &[u8]) -> Option<Vec<u8>> {
    const ID: &[u8] = b"ICC_PROFILE\0";
    /// Well past any real profile (a big CMYK one is ~2 MB) and far short of a bomb.
    const MAX_ICC: usize = 8 * 1024 * 1024;
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return None;
    }
    use core::ops::ControlFlow;
    let mut chunks: Vec<(u8, &[u8])> = Vec::new();
    let mut declared = 0u8;
    for_each_jpeg_segment(b, |marker, payload| {
        if marker == 0xE2 && payload.len() > ID.len() + 2 && payload.starts_with(ID) {
            chunks.push((payload[ID.len()], &payload[ID.len() + 2..]));
            declared = declared.max(payload[ID.len() + 1]);
        }
        ControlFlow::<()>::Continue(())
    });
    if chunks.is_empty() || chunks.len() != declared as usize {
        return None;
    }
    chunks.sort_by_key(|(seq, _)| *seq);
    if !chunks
        .iter()
        .enumerate()
        .all(|(i, (s, _))| *s as usize == i + 1)
    {
        return None;
    }
    let total: usize = chunks.iter().map(|(_, d)| d.len()).sum();
    if total == 0 || total > MAX_ICC {
        return None;
    }
    let mut out = Vec::with_capacity(total);
    for (_, d) in chunks {
        out.extend_from_slice(d);
    }
    Some(out)
}

/// Walk a JPEG's marker segments from just past SOI, handing `visit` each segment's marker
/// and its payload (the bytes after the two length bytes); `Break` stops the walk with a
/// value. Standalone markers - 0xFF padding, TEM, RSTn, SOI, EOI - carry no payload and are
/// stepped over. The walk ends at SOS (the entropy-coded scan starts there and every APP and
/// SOF segment is behind us), at a length under 2, or at a payload that overruns the buffer.
fn for_each_jpeg_segment<'b, B>(
    b: &'b [u8],
    mut visit: impl FnMut(u8, &'b [u8]) -> core::ops::ControlFlow<B>,
) -> Option<B> {
    let mut i = 2usize;
    while i + 4 <= b.len() {
        match jpeg_segment_step(b, i, &mut visit) {
            JpegSegmentStep::Next(next) => i = next,
            JpegSegmentStep::End => return None,
            JpegSegmentStep::Found(found) => return Some(found),
        }
    }
    None
}

/// Scan the ONE segment at offset `i`: the offset the walk resumes at, that the walk is over,
/// or the value a `Break` from `visit` ends it with.
fn jpeg_segment_step<'b, B>(
    b: &'b [u8],
    i: usize,
    visit: &mut impl FnMut(u8, &'b [u8]) -> core::ops::ControlFlow<B>,
) -> JpegSegmentStep<B> {
    if b[i] != 0xFF {
        return JpegSegmentStep::Next(i + 1);
    }
    let marker = b[i + 1];
    if marker == 0xFF || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
        return JpegSegmentStep::Next(i + 2);
    }
    if marker == 0xDA {
        return JpegSegmentStep::End;
    }
    let len = ((b[i + 2] as usize) << 8) | b[i + 3] as usize;
    if len < 2 {
        return JpegSegmentStep::End;
    }
    let Some(payload) = b.get(i + 4..i + 2 + len) else {
        return JpegSegmentStep::End;
    };
    match visit(marker, payload) {
        core::ops::ControlFlow::Break(found) => JpegSegmentStep::Found(found),
        core::ops::ControlFlow::Continue(()) => JpegSegmentStep::Next(i + 2 + len),
    }
}

/// What [`jpeg_segment_step`] found: resume the walk at this offset, end it with no value
/// (SOS, a length under 2, or a payload overrunning the buffer), or end it with a value.
enum JpegSegmentStep<B> {
    Next(usize),
    End,
    Found(B),
}

/// Quick check: a JPEG whose frame header declares 4 components (CMYK / YCCK). Walks the
/// markers only (no pixel decode), so it's cheap to run on every JPEG before the image tier.
pub(super) fn is_cmyk_jpeg(b: &[u8]) -> bool {
    use core::ops::ControlFlow;
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return false;
    }
    // SOFn markers carry the component count — all 0xC0..=0xCF except DHT/JPG/DAC:
    // [FFCn][len:2][precision:1][height:2][width:2][Nf:1], so Nf is payload byte 5. A frame
    // header always precedes the scan, so ending the walk at SOS loses nothing.
    for_each_jpeg_segment(b, |marker, payload| {
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            ControlFlow::Break(payload.get(5) == Some(&4))
        } else {
            ControlFlow::Continue(())
        }
    })
    .unwrap_or(false)
}

/// Would decoding a `w`×`h` CMYK JPEG through [`decode_cmyk_jpeg`] blow past `max_alloc`?
/// Sums the three transient buffers the function allocates along the way: zune's raw CMYK
/// output (4 B/px), the padded `Cmyka` copy (5 B/px), and the final RGBA buffer (4 B/px) —
/// 13 B/px, well past the 512 MiB budget every other decode tier is held to at anything near
/// the dimension cap. A plain `w * h * 13` multiply is used instead of `checked_mul` because
/// both factors are already bounded by `MAX_DIM`/`MAX_PIXELS` at every call site, so the
/// product can't approach `u64::MAX`.
fn cmyk_transient_bytes_exceed_budget(w: u32, h: u32, max_alloc: u64) -> bool {
    const CMYK_TRANSIENT_BYTES_PER_PIXEL: u64 = 13;
    (w as u64) * (h as u64) * CMYK_TRANSIENT_BYTES_PER_PIXEL > max_alloc
}

/// Decode a CMYK/YCCK JPEG to color-managed sRGB: pull the RAW 4-channel CMYK from
/// zune-jpeg (the image crate would convert it to RGB naively, dropping the profile), then
/// run it through the embedded CMYK ICC → sRGB with moxcms. Returns `None` (caller falls
/// back to the image crate's RGB) if it isn't really CMYK, lacks a usable CMYK profile, or
/// fails — so this can only ever improve a CMYK thumbnail, never blank one.
pub(super) fn decode_cmyk_jpeg(bytes: &[u8]) -> Option<DynamicImage> {
    use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};
    use zune_jpeg::zune_core::bytestream::ZCursor;
    use zune_jpeg::zune_core::colorspace::ColorSpace;
    use zune_jpeg::zune_core::options::DecoderOptions;
    use zune_jpeg::JpegDecoder;

    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::CMYK);
    let mut dec = JpegDecoder::new_with_options(ZCursor::new(bytes), opts);
    dec.decode_headers().ok()?;
    match dec.input_colorspace()? {
        ColorSpace::CMYK | ColorSpace::YCCK => {}
        _ => return None,
    }
    let info = dec.info()?;
    let (w, h) = (u32::from(info.width), u32::from(info.height));
    if !cmyk_output_within_limits(w, h) {
        return None;
    }
    // We can only color-manage with the embedded CMYK profile — without one there is no
    // sound CMYK→RGB, so defer to the image crate's existing (naive) conversion.
    let icc = dec.icc_profile()?;
    let src = ColorProfile::new_from_slice(&icc).ok()?;
    if src.color_space != DataColorSpace::Cmyk {
        return None;
    }
    let cmyk = dec.decode().ok()?; // 4 bytes/px
    let px = (w as usize) * (h as usize);
    if cmyk.len() < px * 4 {
        return None;
    }
    // moxcms takes CMYK + alpha (`Cmyka`, 5 channels); pad each pixel with an opaque alpha.
    let mut cmyka = vec![0u8; px * 5];
    for i in 0..px {
        cmyka[i * 5..i * 5 + 4].copy_from_slice(&cmyk[i * 4..i * 4 + 4]);
        cmyka[i * 5 + 4] = 255;
    }
    let dst = ColorProfile::new_srgb();
    let transform = src
        .create_transform_8bit(
            Layout::Cmyka,
            &dst,
            Layout::Rgba,
            TransformOptions::default(),
        )
        .ok()?;
    let mut rgba = vec![0u8; px * 4];
    transform.transform(&cmyka, &mut rgba).ok()?;
    image::RgbaImage::from_raw(w, h, rgba).map(DynamicImage::ImageRgba8)
}

/// Are a CMYK JPEG's `w`×`h` samples decodable here: nonzero, within `MAX_DIM`/`MAX_PIXELS`,
/// and inside the transient-byte budget of the three buffers the decode allocates?
fn cmyk_output_within_limits(w: u32, h: u32) -> bool {
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) > MAX_PIXELS {
        return false;
    }
    // MAX_DIM/MAX_PIXELS alone bound the OUTPUT shape, not what the decode actually
    // allocates on the way there. Unlike `decode_with_image_alloc`, this path runs before
    // that budget is even constructed (it's tried up front in `decode_with_image_alloc`),
    // so it has to enforce its own ceiling here rather than inherit one from a
    // caller-supplied `Limits`.
    if cmyk_transient_bytes_exceed_budget(w, h, MAX_ALLOC) {
        return false;
    }
    true
}

/// The standard sRGB electro-optical transfer function (IEC 61966-2-1) — the reference
/// curve [`icc_profile_is_srgb`] compares an embedded profile's transfer curve against.
fn srgb_eotf(x: f32) -> f32 {
    if x < 0.04045 {
        x / 12.92
    } else {
        ((x + 0.055) / 1.055).powf(2.4)
    }
}

/// Is this already-parsed profile colorimetrically indistinguishable from sRGB — same
/// primaries, same white point, same transfer curve? Checked numerically against
/// [`moxcms::ColorProfile::new_srgb`] rather than by name (a profile's description tag
/// is not evidence), so this is conservative: it recognises the overwhelmingly common
/// case of a genuinely-sRGB embedded profile — encoded either as the exact parametric
/// curve or as a sampled LUT close to it, which is what most real-world sRGB ICC
/// profiles (ArgyllCMS, Little CMS, Photoshop's "sRGB IEC61966-2.1") actually ship —
/// and otherwise falls through to the real transform, never the other way around.
fn icc_profile_is_srgb(src: &moxcms::ColorProfile) -> bool {
    use moxcms::{ColorProfile, Xyzd};

    let srgb = ColorProfile::new_srgb();
    const EPS: f64 = 0.0005; // well above ICC's S15Fixed16 (~1/65536) encoding noise
    let colorant_matches = |a: Xyzd, b: Xyzd| {
        (a.x - b.x).abs() < EPS && (a.y - b.y).abs() < EPS && (a.z - b.z).abs() < EPS
    };
    if !colorant_matches(src.red_colorant, srgb.red_colorant)
        || !colorant_matches(src.green_colorant, srgb.green_colorant)
        || !colorant_matches(src.blue_colorant, srgb.blue_colorant)
    {
        return false;
    }
    let curve_is_srgb = |trc: &Option<moxcms::ToneReprCurve>| {
        let Some(trc) = trc else { return false };
        let Ok(eval) = trc.make_linear_evaluator() else {
            return false;
        };
        [0.0f32, 0.05, 0.18, 0.5, 0.75, 1.0]
            .into_iter()
            .all(|x| (eval.evaluate_value(x) - srgb_eotf(x)).abs() < 0.01)
    };
    curve_is_srgb(&src.red_trc) && curve_is_srgb(&src.green_trc) && curve_is_srgb(&src.blue_trc)
}

/// The HDR signal of an ICC profile, in the shape the PNG `cICP` conversion takes, or `None`
/// for an SDR profile. Two sources, in order: the profile's own `cicp` tag (what a
/// moxcms-built or an ICC.1:2022 HDR profile carries), and, for the older profiles that have
/// no such tag, the transfer curve itself: PQ maps 0.5 to 0.92% and 0.75 to 9.9% of peak,
/// which no display gamma comes near (sRGB puts 0.5 at 21%). The primaries are then read off
/// the colorants against the BT.2020 and Display P3 references. HLG is only recognised from
/// the tag: its curve is too close to a gamma to name from three samples.
pub(super) fn icc_hdr_cicp(src: &moxcms::ColorProfile) -> Option<super::cicp::PngCicp> {
    use moxcms::{CicpColorPrimaries, TransferCharacteristics};
    if let Some(tag) = src.cicp {
        let transfer = match tag.transfer_characteristics {
            TransferCharacteristics::Smpte2084 => 16,
            TransferCharacteristics::Hlg => 18,
            _ => return None,
        };
        let primaries = match tag.color_primaries {
            CicpColorPrimaries::Bt2020 => 9,
            CicpColorPrimaries::Smpte432 => 12,
            CicpColorPrimaries::Bt709 => 1,
            _ => primaries_from_colorants(src),
        };
        return Some(super::cicp::PngCicp {
            primaries,
            transfer,
            full_range: tag.full_range,
        });
    }
    let curve_is_pq = |trc: &Option<moxcms::ToneReprCurve>| {
        let Some(trc) = trc else { return false };
        let Ok(eval) = trc.make_linear_evaluator() else {
            return false;
        };
        // PQ EOTF at 0.5 and 0.75, as a fraction of the 10 000-nit peak.
        (eval.evaluate_value(0.5) - 0.0092).abs() < 0.004
            && (eval.evaluate_value(0.75) - 0.0985).abs() < 0.02
            && (eval.evaluate_value(1.0) - 1.0).abs() < 0.02
    };
    if curve_is_pq(&src.red_trc) && curve_is_pq(&src.green_trc) && curve_is_pq(&src.blue_trc) {
        return Some(super::cicp::PngCicp {
            primaries: primaries_from_colorants(src),
            transfer: 16,
            full_range: true,
        });
    }
    None
}

/// CICP primaries code read off `src`'s colorants: 9 for BT.2020, 12 for Display P3, 1 otherwise.
fn primaries_from_colorants(src: &moxcms::ColorProfile) -> u8 {
    use moxcms::{ColorProfile, Xyzd};
    const EPS: f64 = 0.002;
    let close = |a: Xyzd, b: Xyzd| {
        (a.x - b.x).abs() < EPS && (a.y - b.y).abs() < EPS && (a.z - b.z).abs() < EPS
    };
    let matches = |reference: &ColorProfile| {
        close(src.red_colorant, reference.red_colorant)
            && close(src.green_colorant, reference.green_colorant)
            && close(src.blue_colorant, reference.blue_colorant)
    };
    if matches(&ColorProfile::new_bt2020()) {
        9
    } else if matches(&ColorProfile::new_display_p3()) {
        12
    } else {
        1
    }
}

/// Run `cms` over an 8-bit RGB or RGBA buffer (`layout` says which) and rebuild the image
/// from what it returns, so a colour-managed `DynamicImage` never comes back blank: `cms`
/// itself already keeps the original pixels on a transform error, so only a length mismatch
/// can reach the blank fallback.
fn cms_8bit<P>(
    buf: image::ImageBuffer<P, Vec<u8>>,
    layout: moxcms::Layout,
    cms: impl Fn(moxcms::Layout, Vec<u8>) -> Vec<u8>,
) -> DynamicImage
where
    P: image::Pixel<Subpixel = u8>,
    DynamicImage: From<image::ImageBuffer<P, Vec<u8>>>,
{
    let (w, h) = buf.dimensions();
    let raw = cms(layout, buf.into_raw());
    image::ImageBuffer::<P, Vec<u8>>::from_raw(w, h, raw)
        .unwrap_or_else(|| image::ImageBuffer::new(w, h))
        .into()
}

/// Color-manage an embedded ICC profile to sRGB so wide-gamut (Display-P3 / Adobe RGB /
/// …) thumbnails match a color-managed viewer instead of rendering over-saturated — and
/// then having Explorer cache the wrong colors. Uses the pure-Rust `moxcms` we ALREADY
/// ship (via `image`/`jxl-oxide`), so this adds no dependency and no size.
///
/// Scope: RGB/RGBA with an RGB-space profile. No-profile, CMYK, Lab and gray images pass
/// through untouched (CMYK→sRGB needs the raw CMYK samples and is a separate, harder
/// transform). Best-effort: any parse/transform failure returns the image unchanged, so
/// color management can never turn a good thumbnail into a blank.
///
/// 16-bit RGB/RGBA is narrowed to 8-bit and then transformed. That loses sub-8-bit
/// precision the thumbnail path discards anyway, and the alternative is what used to
/// happen: passing 16-bit straight through, silently un-managed. That mattered as soon as
/// AVIF started routing to ImageMagick (issue #9) — magick hands back a 16-bit PNG for any
/// 10-bit source, so an Adobe RGB AVIF came out in raw wide-gamut numbers, off by 79/255 on
/// a saturated patch, while the same file through WIC was correct.
///
/// Short-circuits to a pure pass-through when the embedded profile is already sRGB
/// (checked numerically by [`icc_profile_is_srgb`]) — most PNG/TIFF/WebP exports carry
/// one, and building a `moxcms` transform for the identity case is pure loss.
pub(super) fn apply_icc_to_srgb(img: DynamicImage, icc: Option<Vec<u8>>) -> DynamicImage {
    use moxcms::{ColorProfile, DataColorSpace, Layout};

    let Some(icc) = icc.filter(|p| !p.is_empty()) else {
        return img;
    };
    let Ok(src) = ColorProfile::new_from_slice(&icc) else {
        return img;
    };
    // Only matrix/RGB display profiles here — never mangle CMYK/Lab/etc.
    if src.color_space != DataColorSpace::Rgb {
        return img;
    }
    // An HDR profile - a `cicp` tag naming PQ or HLG (ICC.1:2022), or a transfer curve that
    // measures as PQ - describes integer samples still wearing the HDR curve. Colour-managing
    // those treats 10 000 nits as white and renders the picture near black: the shape of
    // issue #38, in a 16-bit TIFF or a JPEG instead of a JPEG XL. They take the conversion and
    // tone map an HDR PNG does, with the profile's own primaries.
    if let Some(cicp) = icc_hdr_cicp(&src) {
        if let Some(linear) = super::cicp::cicp_hdr_to_linear(&img, &cicp) {
            return tone_map_float(&linear);
        }
    }
    // Most PNG/TIFF/WebP exports carry an sRGB profile, so building a moxcms transform
    // and running it over every pixel is usually paying full CMS cost for the identity
    // transform. Short-circuit when the embedded profile IS sRGB (checked numerically,
    // not by trusting the profile's own description tag).
    if icc_profile_is_srgb(&src) {
        return img;
    }
    let dst = ColorProfile::new_srgb();

    // Transform a flat 8-bit buffer (sample count is preserved, so the ImageBuffer
    // rebuild can't fail). On any error, keep the ORIGINAL pixels — never a blank.
    let cms =
        |layout: Layout, px: Vec<u8>| -> Vec<u8> { icc_transform_8bit(&src, &dst, layout, px) };

    match img {
        DynamicImage::ImageRgb8(buf) => cms_8bit(buf, moxcms::Layout::Rgb, cms),
        DynamicImage::ImageRgba8(buf) => cms_8bit(buf, moxcms::Layout::Rgba, cms),
        // Narrow to 8-bit first (see the note above) rather than skip management entirely.
        img @ (DynamicImage::ImageRgb16(_) | DynamicImage::ImageRgba16(_)) => {
            let has_alpha = matches!(img, DynamicImage::ImageRgba16(_));
            if has_alpha {
                cms_8bit(img.to_rgba8(), moxcms::Layout::Rgba, cms)
            } else {
                cms_8bit(img.to_rgb8(), moxcms::Layout::Rgb, cms)
            }
        }
        other => other,
    }
}

/// Run the `src`→`dst` 8-bit CMS over a flat buffer in `layout`, keeping the ORIGINAL pixels
/// on any transform error — never a blank.
fn icc_transform_8bit(
    src: &moxcms::ColorProfile,
    dst: &moxcms::ColorProfile,
    layout: moxcms::Layout,
    px: Vec<u8>,
) -> Vec<u8> {
    let mut out = vec![0u8; px.len()];
    match src.create_transform_8bit(layout, dst, layout, moxcms::TransformOptions::default()) {
        Ok(t) if t.transform(&px, &mut out).is_ok() => out,
        _ => px,
    }
}

/// One `colr` box body → ICC bytes: a direct embedded profile, or a CICP `nclx` signal
/// mapped to a built-in profile (Display-P3 / sRGB) encoded as ICC. `None` for signals we
/// don't translate (leaves the image untouched — never a wrong guess).
pub(super) fn colr_profile(body: &[u8]) -> Option<Vec<u8>> {
    match body.get(0..4)? {
        b"prof" | b"rICC" => {
            let icc = &body[4..];
            (!icc.is_empty() && icc.len() <= 4 * 1024 * 1024).then(|| icc.to_vec())
        }
        b"nclx" => {
            // WIC has already used matrix_coefficients/full_range_flag while converting the
            // encoded YCbCr frame to RGBA.  The ICC we synthesize describes those RGB values,
            // so its primaries AND transfer curve must match the nclx signal.  Display P3 uses
            // the sRGB transfer curve; treating P3 primaries paired with BT.709, PQ, HLG, etc.
            // as Display P3 would apply the wrong tone curve and visibly skew the thumbnail.
            let primaries = u16::from_be_bytes(body.get(4..6)?.try_into().ok()?);
            let transfer = u16::from_be_bytes(body.get(6..8)?.try_into().ok()?);
            match (primaries, transfer) {
                (12, 13) => moxcms::ColorProfile::new_display_p3().encode().ok(),
                _ => None, // unsupported tuple: leave WIC's RGBA untouched, never guess
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests;
