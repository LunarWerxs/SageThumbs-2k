//! Colour management and HDR tone mapping.
//!
//! Everything that turns a decoder's raw samples into displayable sRGB: embedded ICC
//! profiles, the ISOBMFF `colr` box (AVIF/HEIC, including the CICP Display-P3 signal
//! iPhones use), CMYK/YCCK JPEG, and the linear-float (EXR/Radiance/jxl) tone map.
//! All pure Rust - no ImageMagick, no C colour engine.

use super::*;

/// Tone-map a 32-bit linear-float HDR image (EXR/Radiance) to 8-bit sRGB, in pure
/// Rust: the Reinhard global operator `x/(1+x)` compresses the unbounded range,
/// then a linear→sRGB transfer encodes it for display. Replaces an ImageMagick
/// subprocess for this whole format class (and lets EXR/HDR work without magick).
/// Non-finite / negative samples are clamped to 0.
pub(super) fn tone_map_float(img: &DynamicImage) -> DynamicImage {
    let src = img.to_rgba32f();
    let mut out = image::RgbaImage::new(src.width(), src.height());
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
    let mut any_alpha = false;
    for (o, s) in out.pixels_mut().zip(src.pixels()) {
        let [r, g, b, a] = s.0;
        let alpha = (a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
        any_alpha |= alpha != 0;
        *o = image::Rgba([map(r), map(g), map(b), alpha]);
    }
    // VFX render passes (emission/environment/AOV EXRs) legitimately carry RGB with
    // the ENTIRE alpha channel at 0 — honoring that verbatim hands the caller a
    // fully-transparent image the `is_fully_transparent` watchdog then rejects, so
    // the file shows a default icon while every image viewer shows its RGB fine.
    // When ALL alpha is 0 there is no compositing intent to preserve; show the RGB
    // opaque instead. Partial alpha stays untouched. (Rgb32F sources convert with
    // a=1.0, so this only fires on genuinely all-transparent RGBA floats.)
    if !any_alpha {
        for px in out.pixels_mut() {
            px.0[3] = 255;
        }
    }
    DynamicImage::ImageRgba8(out)
}

/// Quick check: a JPEG whose frame header declares 4 components (CMYK / YCCK). Walks the
/// markers only (no pixel decode), so it's cheap to run on every JPEG before the image tier.
pub(super) fn is_cmyk_jpeg(b: &[u8]) -> bool {
    if b.len() < 4 || b[0] != 0xFF || b[1] != 0xD8 {
        return false;
    }
    let mut i = 2usize;
    while i + 9 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        let marker = b[i + 1];
        // Standalone markers (no length payload): 0xFF padding, SOI, EOI, RSTn, TEM.
        if marker == 0xFF || marker == 0x01 || (0xD0..=0xD9).contains(&marker) {
            i += 2;
            continue;
        }
        // SOFn markers carry the component count — all 0xC0..=0xCF except DHT/JPG/DAC.
        if (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC {
            // [FFCn][len:2][precision:1][height:2][width:2][Nf:1] → Nf at offset +9.
            return b.get(i + 9) == Some(&4);
        }
        let len = ((b[i + 2] as usize) << 8) | b[i + 3] as usize;
        if len < 2 {
            return false;
        }
        i += 2 + len;
    }
    false
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
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) > MAX_PIXELS {
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
pub(super) fn apply_icc_to_srgb(img: DynamicImage, icc: Option<Vec<u8>>) -> DynamicImage {
    use moxcms::{ColorProfile, DataColorSpace, Layout, TransformOptions};

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
    let dst = ColorProfile::new_srgb();

    // Transform a flat 8-bit buffer (sample count is preserved, so the ImageBuffer
    // rebuild can't fail). On any error, keep the ORIGINAL pixels — never a blank.
    let cms = |layout: Layout, px: Vec<u8>| -> Vec<u8> {
        let mut out = vec![0u8; px.len()];
        match src.create_transform_8bit(layout, &dst, layout, TransformOptions::default()) {
            Ok(t) if t.transform(&px, &mut out).is_ok() => out,
            _ => px,
        }
    };

    match img {
        DynamicImage::ImageRgb8(buf) => {
            let (w, h) = buf.dimensions();
            let out = cms(Layout::Rgb, buf.into_raw());
            image::RgbImage::from_raw(w, h, out)
                .map(DynamicImage::ImageRgb8)
                .unwrap_or_else(|| DynamicImage::new_rgb8(w, h))
        }
        DynamicImage::ImageRgba8(buf) => {
            let (w, h) = buf.dimensions();
            let out = cms(Layout::Rgba, buf.into_raw());
            image::RgbaImage::from_raw(w, h, out)
                .map(DynamicImage::ImageRgba8)
                .unwrap_or_else(|| DynamicImage::new_rgba8(w, h))
        }
        // Narrow to 8-bit first (see the note above) rather than skip management entirely.
        img @ (DynamicImage::ImageRgb16(_) | DynamicImage::ImageRgba16(_)) => {
            let has_alpha = matches!(img, DynamicImage::ImageRgba16(_));
            if has_alpha {
                let buf = img.to_rgba8();
                let (w, h) = buf.dimensions();
                let out = cms(Layout::Rgba, buf.into_raw());
                image::RgbaImage::from_raw(w, h, out)
                    .map(DynamicImage::ImageRgba8)
                    .unwrap_or_else(|| DynamicImage::new_rgba8(w, h))
            } else {
                let buf = img.to_rgb8();
                let (w, h) = buf.dimensions();
                let out = cms(Layout::Rgb, buf.into_raw());
                image::RgbImage::from_raw(w, h, out)
                    .map(DynamicImage::ImageRgb8)
                    .unwrap_or_else(|| DynamicImage::new_rgb8(w, h))
            }
        }
        other => other,
    }
}

/// Extract the display color profile from an ISOBMFF (AVIF/HEIC) `colr` box. WIC's AV1/HEVC
/// codecs don't surface it via `GetColorContexts`, so wide-gamut AVIF/HEIC would otherwise
/// render mis-saturated. Handles an embedded ICC (`prof`/`rICC`) directly, AND maps the
/// common CICP `nclx` signal (Display-P3 / sRGB) to a built-in profile so even nclx-only
/// files (e.g. iPhone HEIC) color-manage. Returns ICC bytes for [`apply_icc_to_srgb`].
pub(super) fn isobmff_color_icc(bytes: &[u8]) -> Option<Vec<u8>> {
    // Only walk real ISOBMFF (starts with an `ftyp` box) — never chew through a RAW/JXR.
    if bytes.get(4..8) != Some(b"ftyp") {
        return None;
    }
    fn walk(buf: &[u8], depth: u8) -> Option<Vec<u8>> {
        if depth > 6 {
            return None;
        }
        let mut p = 0usize;
        while p + 8 <= buf.len() {
            let size = u32::from_be_bytes(buf[p..p + 4].try_into().ok()?) as usize;
            let typ = &buf[p + 4..p + 8];
            let (hdr, end) = match size {
                1 => (
                    16usize,
                    p.checked_add(
                        u64::from_be_bytes(buf.get(p + 8..p + 16)?.try_into().ok()?) as usize
                    )?,
                ),
                0 => (8usize, buf.len()),
                n if n >= 8 => (8usize, p.checked_add(n)?),
                _ => return None,
            };
            if end > buf.len() || end < p + hdr {
                return None;
            }
            let body = &buf[p + hdr..end];
            match typ {
                b"colr" => {
                    if let Some(icc) = colr_profile(body) {
                        return Some(icc);
                    }
                }
                // `meta` is a FullBox (4-byte version+flags precede its children).
                b"meta" => {
                    if let Some(r) = body.get(4..).and_then(|c| walk(c, depth + 1)) {
                        return Some(r);
                    }
                }
                b"iprp" | b"ipco" => {
                    if let Some(r) = walk(body, depth + 1) {
                        return Some(r);
                    }
                }
                _ => {}
            }
            p = end;
        }
        None
    }
    walk(bytes, 0)
}

/// Does this AVIF need ImageMagick because Microsoft's AV1 WIC codec gets its colour wrong?
///
/// Issue #9. WIC (AV1 Video Extension 2.0.24.0) misreads the colour signalling that libaom
/// writes, so `avifenc` and `ffmpeg` output decodes with visibly shifted colour while libavif
/// and ImageMagick read the very same file correctly. That is why the reporter's other viewers
/// and Icaros were fine: they use libavif, and we were the only one asking Windows.
///
/// Measured against libavif AND ImageMagick on a six-patch target, worst channel error of 255:
///
/// | file | WIC error |
/// |---|---|
/// | `nclx`, 8-bit, `matrix=1` (BT.709) | 1–3, correct |
/// | `nclx`, 8-bit, `matrix=6` (BT.601, avifenc's default) | 19, greys hold and saturated colour shifts |
/// | `nclx`, 10-bit, any matrix, any range | 14–15, mid grey reads 128 as 139 |
/// | no `nclx`, 8-bit | 19, WIC assumes BT.709 where libaom encoded BT.601 |
/// | no `nclx`, 10/12-bit | 1, correct |
///
/// Note the shape of that: WIC is wrong in four of the five cases and the two correct ones do
/// not share a rule. So this is a WHITELIST, not a blacklist. We keep the cheap WIC path only
/// for the one case that is both measurably correct AND overwhelmingly common (ordinary 8-bit
/// BT.709 web AVIF, what Chrome and Squoosh emit), and hand everything else to ImageMagick.
/// Anything unparseable or unrecognised also goes to ImageMagick, so a WIC version that changes
/// its behaviour cannot silently reintroduce the bug.
///
/// Callers use this exactly like [`isobmff_has_hevc_aux_alpha`]: prefer ImageMagick when the
/// external tier is available, and fall back to WIC when it is not, so the Compact install
/// keeps the thumbnail it has today rather than losing it.
pub(super) fn avif_wic_misreads_color(bytes: &[u8]) -> bool {
    if bytes.get(4..8) != Some(b"ftyp") {
        return false;
    }
    struct Found {
        matrix: Option<u16>,
        high_bitdepth: bool,
        is_av1: bool,
    }

    fn walk(buf: &[u8], depth: u8, f: &mut Found) {
        if depth > 6 {
            return;
        }
        let mut p = 0usize;
        while p + 8 <= buf.len() {
            let Ok(raw) = buf[p..p + 4].try_into() else {
                return;
            };
            let size = u32::from_be_bytes(raw) as usize;
            let typ = &buf[p + 4..p + 8];
            let (hdr, end) = match size {
                0 => (8usize, buf.len()),
                n if n >= 8 => match p.checked_add(n) {
                    Some(e) => (8usize, e),
                    None => return,
                },
                // 64-bit sizes only ever wrap `mdat` here, which holds no colour metadata.
                _ => return,
            };
            if end > buf.len() || end < p + hdr {
                return;
            }
            let body = &buf[p + hdr..end];
            match typ {
                // ColourInformationBox: `nclx` carries CICP as 3 × u16 then a full-range bit.
                b"colr" if body.get(..4) == Some(b"nclx") => {
                    if let Some(raw) = body.get(8..10).and_then(|b| b.try_into().ok()) {
                        f.matrix = Some(u16::from_be_bytes(raw));
                    }
                }
                // AV1CodecConfigurationBox: byte 2 is
                // seq_tier(1) high_bitdepth(1) twelve_bit(1) monochrome(1) subx(1) suby(1) pos(2).
                b"av1C" => {
                    f.is_av1 = true;
                    if let Some(b) = body.get(2) {
                        f.high_bitdepth |= (b >> 6) & 1 == 1;
                    }
                }
                // `meta` is a FullBox: 4 bytes of version+flags precede its children.
                b"meta" => {
                    if let Some(children) = body.get(4..) {
                        walk(children, depth + 1, f);
                    }
                }
                b"iprp" | b"ipco" => walk(body, depth + 1, f),
                _ => {}
            }
            p = end;
        }
    }
    let mut found = Found {
        matrix: None,
        high_bitdepth: false,
        is_av1: false,
    };
    walk(bytes, 0, &mut found);

    if !found.is_av1 {
        return false; // HEIC and friends carry `hvcC`, and are not ours to route.
    }
    // The single trusted case: an explicit BT.709 (or identity) matrix at 8 bits.
    let wic_is_trustworthy =
        !found.high_bitdepth && matches!(found.matrix, Some(0) | Some(1));
    !wic_is_trustworthy
}

/// Does this ISOBMFF image advertise an HEVC auxiliary alpha item?
///
/// Microsoft's HEIC WIC codec can decode these files while silently flattening
/// the auxiliary alpha plane. Decode paths that allow the Full install's external
/// tier therefore use this cheap predicate to prefer ImageMagick before WIC.
/// This is deliberately a bounded, association-aware box walk rather than a
/// byte search: an exact `auxC` property must be assigned to an item by `ipma`,
/// and that item must be an `auxl` auxiliary of the primary (`pitm`) item.
pub(super) fn isobmff_has_hevc_aux_alpha(bytes: &[u8]) -> bool {
    const HEVC_ALPHA_AUX_TYPE: &[u8] = b"urn:mpeg:hevc:2015:auxid:1";
    const MAX_BOXES: usize = 512;

    /// Parse a complete box sequence, retaining at most the small, bounded
    /// metadata tree this predicate needs. A malformed sibling makes the whole
    /// predicate decline rather than attempting to recover into media payload.
    fn boxes<'a>(buf: &'a [u8], boxes_left: &mut usize) -> Option<Vec<([u8; 4], &'a [u8])>> {
        let mut p = 0usize;
        let mut out = Vec::new();
        while p != buf.len() {
            if *boxes_left == 0 || buf.len() - p < 8 {
                return None;
            }
            *boxes_left -= 1;

            let size32 = u32::from_be_bytes(buf.get(p..p + 4)?.try_into().ok()?);
            let typ = buf.get(p + 4..p + 8)?.try_into().ok()?;
            let (header_len, size) = match size32 {
                0 => (8usize, buf.len() - p),
                1 => {
                    let raw = buf.get(p + 8..p + 16)?.try_into().ok()?;
                    let size = usize::try_from(u64::from_be_bytes(raw)).ok()?;
                    (16usize, size)
                }
                size => (8usize, size as usize),
            };
            if size < header_len {
                return None;
            }
            let end = p.checked_add(size)?;
            let body = buf.get(p + header_len..end)?;
            out.push((typ, body));
            p = end;
        }
        Some(out)
    }

    fn item_id(body: &[u8], version: u8, p: &mut usize) -> Option<u32> {
        let id = match version {
            0 => u16::from_be_bytes(body.get(*p..*p + 2)?.try_into().ok()?) as u32,
            1 => u32::from_be_bytes(body.get(*p..*p + 4)?.try_into().ok()?),
            _ => return None,
        };
        *p += if version == 0 { 2 } else { 4 };
        Some(id)
    }

    fn associated_items(
        body: &[u8],
        property_count: usize,
        alpha_properties: &[usize],
    ) -> Option<Vec<u32>> {
        let version = *body.first()?;
        if version > 1 {
            return None;
        }
        let flags = u32::from_be_bytes([0, *body.get(1)?, *body.get(2)?, *body.get(3)?]);
        let large_indices = flags & 1 != 0;
        let count = u32::from_be_bytes(body.get(4..8)?.try_into().ok()?) as usize;
        let min_entry_len = if version == 0 { 3 } else { 5 }; // item ID + association count
        if count > body.len().saturating_sub(8) / min_entry_len {
            return None;
        }
        let mut p = 8usize;
        let mut out = Vec::new();
        for _ in 0..count {
            let id = item_id(body, version, &mut p)?;
            let associations = *body.get(p)? as usize;
            p += 1;
            let mut has_alpha_property = false;
            for _ in 0..associations {
                let raw = if large_indices {
                    let raw = u16::from_be_bytes(body.get(p..p + 2)?.try_into().ok()?);
                    p += 2;
                    (raw & 0x7FFF) as usize
                } else {
                    let raw = *body.get(p)?;
                    p += 1;
                    (raw & 0x7F) as usize
                };
                // Property index 0 is reserved; a value past ipco is malformed.
                if raw == 0 || raw > property_count {
                    return None;
                }
                has_alpha_property |= alpha_properties.contains(&raw);
            }
            if has_alpha_property {
                out.push(id);
            }
        }
        (p == body.len()).then_some(out)
    }

    fn auxl_targets_primary(
        body: &[u8],
        alpha_items: &[u32],
        primary: u32,
        boxes_left: &mut usize,
    ) -> Option<bool> {
        let version = *body.first()?;
        if version > 1 {
            return None;
        }
        let mut found = false;
        for (typ, reference) in boxes(body.get(4..)?, boxes_left)? {
            if typ != *b"auxl" {
                continue;
            }
            let mut p = 0usize;
            let from = item_id(reference, version, &mut p)?;
            let count = u16::from_be_bytes(reference.get(p..p + 2)?.try_into().ok()?) as usize;
            p += 2;
            for _ in 0..count {
                let target = item_id(reference, version, &mut p)?;
                found |= alpha_items.contains(&from) && target == primary;
            }
            if p != reference.len() {
                return None;
            }
        }
        Some(found)
    }

    // A structurally valid FileTypeBox must lead the file. Its body is major
    // brand + minor version, followed by zero or more compatible brands.
    let Some(first_size) = bytes.get(0..4) else {
        return false;
    };
    let Ok(first_size) = first_size.try_into() else {
        return false;
    };
    let first_size = u32::from_be_bytes(first_size) as usize;
    if bytes.get(4..8) != Some(b"ftyp")
        || first_size < 16
        || first_size > bytes.len()
        || !(first_size - 16).is_multiple_of(4)
    {
        return false;
    }

    let mut boxes_left = MAX_BOXES;
    let top = match boxes(bytes, &mut boxes_left) {
        Some(boxes) => boxes,
        None => return false,
    };
    let meta = match top.iter().find(|(typ, _)| typ == b"meta") {
        Some((_, body)) => *body,
        None => return false,
    };
    let children = match meta.get(4..).and_then(|body| boxes(body, &mut boxes_left)) {
        Some(boxes) => boxes,
        None => return false,
    };
    let primary = match children.iter().find(|(typ, _)| typ == b"pitm") {
        Some((_, body)) => {
            let version = match body.first() {
                Some(version) if *version <= 1 => *version,
                _ => return false,
            };
            let mut p = 4usize;
            match item_id(body, version, &mut p) {
                Some(id) if p == body.len() => id,
                _ => return false,
            }
        }
        None => return false,
    };
    let iprp = match children.iter().find(|(typ, _)| typ == b"iprp") {
        Some((_, body)) => *body,
        None => return false,
    };
    let properties = match boxes(iprp, &mut boxes_left) {
        Some(boxes) => boxes,
        None => return false,
    };
    let ipco = match properties.iter().find(|(typ, _)| typ == b"ipco") {
        Some((_, body)) => *body,
        None => return false,
    };
    let ipco_properties = match boxes(ipco, &mut boxes_left) {
        Some(boxes) => boxes,
        None => return false,
    };
    let alpha_properties: Vec<usize> = ipco_properties
        .iter()
        .enumerate()
        .filter_map(|(index, (typ, body))| {
            if typ != b"auxC" {
                return None;
            }
            let aux_type = body.get(4..)?;
            let nul = aux_type.iter().position(|&byte| byte == 0)?;
            (&aux_type[..nul] == HEVC_ALPHA_AUX_TYPE).then_some(index + 1)
        })
        .collect();
    if alpha_properties.is_empty() {
        return false;
    }
    let ipma = match properties.iter().find(|(typ, _)| typ == b"ipma") {
        Some((_, body)) => *body,
        None => return false,
    };
    let alpha_items = match associated_items(ipma, ipco_properties.len(), &alpha_properties) {
        Some(items) if !items.is_empty() => items,
        _ => return false,
    };
    children
        .iter()
        .filter(|(typ, _)| typ == b"iref")
        .try_fold(false, |found, (_, body)| {
            auxl_targets_primary(body, &alpha_items, primary, &mut boxes_left)
                .map(|matches| found || matches)
        })
        .unwrap_or(false)
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
