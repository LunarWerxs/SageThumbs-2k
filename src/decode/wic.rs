//! The Windows Imaging Component tier: OS codecs for the formats the `image` crate
//! cannot read (HEIC/HEIF, AVIF, camera RAW, JPEG 2000, JPEG XR).
//!
//! Bounded by pixel count rather than allocation size (see [`super::limits::MAX_PIXELS`]):
//! WIC decodes in its own memory and hands back one final frame we copy out, so the
//! meaningful guard is how many pixels we copy.

use super::*;

/// Decode via Windows Imaging Component using whatever codecs the OS has
/// installed — this is what gives HEIC/HEIF, AVIF, camera RAW (with the
/// Microsoft Raw Image Extension), and JPEG 2000 without bundling C/LGPL Rust
/// crates. Output is straight (non-premultiplied) RGBA8 so it flows through
/// the same resize/orientation/DIB path as the `image` tier.
pub(super) fn wic_fallback(bytes: &[u8], thumbnail_cx: Option<u32>) -> Result<DynamicImage> {
    unsafe { wic_decode_with_thumbnail(bytes, thumbnail_cx) }
}

/// WIC decode without a target edge, retained for focused tests.
#[cfg(test)]
pub(super) unsafe fn wic_decode(bytes: &[u8]) -> Result<DynamicImage> {
    wic_decode_with_thumbnail(bytes, None)
}

pub(super) unsafe fn wic_decode_with_thumbnail(
    bytes: &[u8],
    thumbnail_cx: Option<u32>,
) -> Result<DynamicImage> {
    // The host thread has COM initialized; in unit tests we CoInitialize first.
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;

    let stream = SHCreateMemStream(Some(bytes)).ok_or_else(|| Error::from(E_FAIL))?;
    let decoder =
        factory.CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnLoad)?;
    let frame = decoder.GetFrame(0)?;
    wic_decode_frame(&factory, &frame, thumbnail_cx, bytes)
}

pub(super) unsafe fn wic_decode_frame(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
    thumbnail_cx: Option<u32>,
    container_bytes: &[u8],
) -> Result<DynamicImage> {
    // Convert to straight 32bpp RGBA (dib.rs handles the premultiply).
    let converter = factory.CreateFormatConverter()?;
    converter.Initialize(
        frame,
        &GUID_WICPixelFormat32bppRGBA,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        // Palette args are unused for a non-indexed (32bppRGBA) destination;
        // Custom is the idiomatic "no palette" value.
        WICBitmapPaletteTypeCustom,
    )?;

    let mut w: u32 = 0;
    let mut h: u32 = 0;
    converter.GetSize(&mut w, &mut h)?;
    // Bomb guard for the WIC tier: per-edge MAX_DIM and total MAX_PIXELS, both
    // from `limits`. MAX_PIXELS (~1 GiB RGBA) is intentionally a higher ceiling
    // than the `image` tier's 512 MiB alloc cap — see the reconciliation note on
    // `limits::MAX_ALLOC` for why the two ceilings differ (single final
    // OS-decoded buffer vs. multiplied in-process transients).
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err(Error::from(E_FAIL));
    }

    // `IWICBitmapScaler` is deliberately placed after format conversion: it receives a
    // straight-RGBA source, produces only the requested thumbnail pixels, and avoids the
    // old full-resolution RGBA `CopyPixels` allocation. ICC conversion still runs on the
    // scaled samples below; resizing is a geometric operation, so this preserves the
    // profile/orientation result while keeping thumbnail peak memory bounded. WIC may still
    // decode internally (codec-dependent), but it no longer hands that full frame to us.
    let source: IWICBitmapSource = if let Some(cx) = thumbnail_cx {
        let long = w.max(h);
        if long > cx {
            let target_w = ((w as u64 * cx as u64 + long as u64 / 2) / long as u64).max(1) as u32;
            let target_h = ((h as u64 * cx as u64 + long as u64 / 2) / long as u64).max(1) as u32;
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(
                &converter,
                target_w,
                target_h,
                WICBitmapInterpolationModeFant,
            )?;
            scaler.cast()?
        } else {
            converter.cast()?
        }
    } else {
        converter.cast()?
    };
    // `IWICBitmapScaler` does NOT promise to preserve its source's pixel format, and with
    // Fant it does not: it hands back WIC's own native 32bpp BGRA order. The buffer below
    // is handed straight to `RgbaImage::from_raw`, so those bytes are then read as RGBA and
    // every SCALED WIC thumbnail comes out with red and blue swapped — HEIC, AVIF, JPEG XR,
    // any Explorer tile smaller than its source. Unscaled decodes were unaffected (no
    // scaler in the chain), which is exactly why it survived: the full-fidelity paths pass
    // `thumbnail_cx = None` and stayed correct while the thumbnail path did not.
    //
    // Re-assert the format on whatever we actually ended up with instead of trusting the
    // scaler's contract. On the already-scaled image this second conversion is cheap, and
    // it is a no-op (same object) whenever the source is already 32bppRGBA.
    let source = ensure_rgba32(factory, source)?;
    source.GetSize(&mut w, &mut h)?;
    let stride = w.checked_mul(4).ok_or_else(|| Error::from(E_FAIL))?;
    let mut buf = vec![0u8; (stride as usize) * (h as usize)];
    source.CopyPixels(std::ptr::null(), stride, &mut buf)?;

    let img = image::RgbaImage::from_raw(w, h, buf).ok_or_else(|| Error::from(E_FAIL))?;
    // Color-manage to sRGB: HEIC/AVIF/RAW carry their wide-gamut profile (iPhone photos
    // are Display P3) in a WIC color context. The format converter above is pixel-format
    // only — NOT color-space — so without this the P3 values render mis-saturated (and
    // Explorer caches the wrong colors). Reuses the image tier's moxcms `apply_icc_to_srgb`.
    // AVIF/HEIC keep their profile in the ISOBMFF `colr` box — WIC's AV1/HEVC codecs do
    // NOT surface it via GetColorContexts (verified: count=0) — so read it ourselves first;
    // fall back to a WIC color context for the other WIC formats (RAW/JXR).
    let icc = isobmff_color_icc(container_bytes).or_else(|| wic_icc(factory, frame));
    Ok(apply_icc_to_srgb(DynamicImage::ImageRgba8(img), icc))
}

/// Guarantee `source` really is 32bppRGBA, converting it if it is not.
///
/// The one caller is the tail of [`wic_decode_frame`], which copies raw bytes out and hands
/// them to `RgbaImage::from_raw` — a step that silently mis-orders channels for any other
/// 32bpp layout. WIC components are free to return a different pixel format than the one
/// they were given (the Fant scaler returns BGRA), so the format is checked here rather
/// than assumed from whatever produced `source`.
unsafe fn ensure_rgba32(
    factory: &IWICImagingFactory,
    source: IWICBitmapSource,
) -> Result<IWICBitmapSource> {
    if source.GetPixelFormat()? == GUID_WICPixelFormat32bppRGBA {
        return Ok(source);
    }
    crate::safety::log_debug("decode: WIC source was not 32bppRGBA — converting");
    let converter = factory.CreateFormatConverter()?;
    converter.Initialize(
        &source,
        &GUID_WICPixelFormat32bppRGBA,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        WICBitmapPaletteTypeCustom,
    )?;
    converter.cast()
}

/// The embedded ICC profile from a WIC frame's first PROFILE-type color context (where
/// HEIC/AVIF/RAW keep their wide-gamut profile). `None` for an Exif-flag-only context, no
/// context, or any COM hiccup — best-effort, so a failure just means "no color management".
pub(super) unsafe fn wic_icc(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
) -> Option<Vec<u8>> {
    let mut count: u32 = 0;
    frame.GetColorContexts(&mut [], &mut count).ok()?;
    let count = (count as usize).min(8); // a sane image has 1-2; cap the pathological
    if count == 0 {
        return None;
    }
    let mut ctxs: Vec<Option<IWICColorContext>> = Vec::with_capacity(count);
    for _ in 0..count {
        ctxs.push(Some(factory.CreateColorContext().ok()?));
    }
    let mut got = count as u32;
    frame.GetColorContexts(&mut ctxs, &mut got).ok()?;
    for ctx in ctxs.into_iter().flatten() {
        let Ok(kind) = ctx.GetType() else { continue };
        if kind != WICColorContextProfile {
            continue; // an Exif color-space FLAG, not an ICC profile — skip
        }
        let mut n: u32 = 0;
        if ctx.GetProfileBytes(&mut [], &mut n).is_err() || n == 0 || n as u64 > 4 * 1024 * 1024 {
            continue;
        }
        let mut buf = vec![0u8; n as usize];
        if ctx.GetProfileBytes(&mut buf, &mut n).is_ok() {
            return Some(buf);
        }
    }
    None
}
