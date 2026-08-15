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

/// Decode straight off the FILE — WIC opens it itself, so nothing buffers the document.
///
/// Everything downstream is shared with [`wic_decode_with_thumbnail`]: the same frame path,
/// the same `IWICBitmapScaler` (which already produces only the requested thumbnail pixels
/// rather than a full-resolution copy), the same bomb guards and colour management. The ONLY
/// difference is where the bytes come from, and that is the whole point: a document past
/// [`super::limits::MAX_INPUT_BYTES`] is refused before any decoder sees it on the buffered
/// path, so a 500 MB scan or panorama got the stock icon no matter what the OS could do
/// with it.
///
/// `head` is a bounded PREFIX of the file, not the file: `wic_decode_frame` uses those bytes
/// only to look for an ISOBMFF `colr` box (AVIF/HEIC wide-gamut), which lives near the start.
/// A short read there just means we fall back to WIC's own colour context, exactly as the
/// non-ISOBMFF formats already do.
/// Decode straight off an EXISTING `IStream` -- the one the shell handed the provider.
///
/// This is what makes the oversized rescue work in Explorer. A thumbnail provider is
/// initialised with a stream, and that stream reports only a leaf file NAME, so there is no
/// path to hand to [`wic_decode_path`]. There does not need to be one: WIC reads a stream
/// lazily, so pointing it at the shell's own stream gets the same "never buffer the document"
/// behaviour without knowing where the document lives. Combined with the scale-first ordering
/// in [`wic_decode_frame`], the codec also decodes at reduced size.
///
/// `head` is a bounded prefix for the ISOBMFF colour box, exactly as in [`wic_decode_path`];
/// the caller reads it off the same stream and rewinds.
pub(super) unsafe fn wic_decode_stream(
    stream: &windows::Win32::System::Com::IStream,
    thumbnail_cx: Option<u32>,
    head: &[u8],
) -> Result<DynamicImage> {
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let decoder =
        factory.CreateDecoderFromStream(stream, std::ptr::null(), WICDecodeMetadataCacheOnLoad)?;
    let frame = decoder.GetFrame(0)?;
    wic_decode_frame(&factory, &frame, thumbnail_cx, head)
}

pub(super) unsafe fn wic_decode_path(
    path: &str,
    thumbnail_cx: Option<u32>,
    head: &[u8],
) -> Result<DynamicImage> {
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let wide = crate::wide(path);
    // `OnDemand`, not `OnLoad`: we want the pixels, and eagerly slurping every metadata block
    // is exactly the cost this path exists to avoid on a very large file. THE COMMENT SAID THIS
    // WHILE THE CODE PASSED `OnLoad` - so the by-path rescue, which exists precisely for files
    // past the 256 MiB ceiling, was walking and caching the whole metadata graph during
    // `GetFrame` before the MAX_DIM/MAX_PIXELS guard below had even seen the dimensions.
    // Nothing on this path reads WIC metadata (EXIF comes from `kamadak-exif` over the raw
    // bytes, and the ICC profile comes from `GetColorContexts`, which is a frame API and not
    // the metadata reader), so deferring it costs nothing.
    let decoder = factory.CreateDecoderFromFilename(
        windows::core::PCWSTR(wide.as_ptr()),
        None,
        windows::Win32::Foundation::GENERIC_READ,
        WICDecodeMetadataCacheOnDemand,
    )?;
    let frame = decoder.GetFrame(0)?;
    wic_decode_frame(&factory, &frame, thumbnail_cx, head)
}

/// The same decode, but ONLY if the codec can genuinely produce a reduced size itself.
///
/// **Why this exists.** Scaling through WIC is a huge win when the codec does it in its own
/// domain and no win at all when it cannot. Measured on this machine: a 12 MP JPEG decoded to
/// 2048 px in 68 ms against 270 ms for a full decode (4x, the DCT trick), while a 24 MP PNG
/// took 605 ms against 690 ms — because a PNG decoder has no reduced-size mode, so WIC decodes
/// the whole image and resamples. A caller that runs this as a fast PRE-PASS before a normal
/// decode therefore doubles the work on PNG while barely moving the first paint.
///
/// So ask the codec instead of guessing. `IWICBitmapSourceTransform::GetClosestSize` is its own
/// answer: hand it the size you want and it writes back the closest it can emit directly. A
/// JPEG answers with a DCT-scaled size; a PNG hands the full dimensions straight back. That
/// beats an extension allowlist, which would be wrong the moment a machine has a different
/// codec installed for the same format, and it beats timing heuristics entirely.
///
/// `None` means "not worth it, decode normally" — no transform interface, no reduction offered,
/// an image already under `target_edge`, or a format WIC declines.
pub(super) unsafe fn wic_decode_path_if_codec_scales(
    path: &str,
    target_edge: u32,
    head: &[u8],
) -> Result<DynamicImage> {
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let wide = crate::wide(path);
    // `OnDemand` for the same reason, and it matters more here: this function often decides it
    // cannot help (PNG) and returns without decoding anything, so any metadata parsed eagerly
    // during `GetFrame` would be pure loss on top of the full decode the caller then runs.
    let decoder = factory.CreateDecoderFromFilename(
        windows::core::PCWSTR(wide.as_ptr()),
        None,
        windows::Win32::Foundation::GENERIC_READ,
        WICDecodeMetadataCacheOnDemand,
    )?;
    let frame = decoder.GetFrame(0)?;
    if !codec_scales_natively(&frame, target_edge) {
        return Err(Error::from(E_FAIL));
    }
    wic_decode_frame(&factory, &frame, Some(target_edge), head)
}

/// [`wic_decode_path_if_codec_scales`] over BYTES rather than a path.
///
/// The shell's thumbnail provider is handed an `IStream`, never a filename (that is what lets
/// it run in the isolated host at all), so the by-path variant — the only one that existed —
/// was unreachable from the surface that draws every thumbnail in Explorer. It was wired into
/// the Quick preview viewer and nowhere else, which is why a folder of large JPEGs paid a full
/// decode per tile while the fast path sat there tested and unused.
///
/// `SHCreateMemStream` copies the buffer. That is deliberate: `IWICStream::InitializeFromMemory`
/// borrows, and would need the caller's slice to outlive a COM object whose lifetime WIC owns —
/// a lifetime bug waiting for the one input that makes the decoder hold on. A copy of an
/// already-buffered image is nothing next to the decode this avoids.
pub(super) unsafe fn wic_decode_bytes_if_codec_scales(
    bytes: &[u8],
    target_edge: u32,
    head: &[u8],
) -> Result<DynamicImage> {
    let factory: IWICImagingFactory =
        CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
    let stream = windows::Win32::UI::Shell::SHCreateMemStream(Some(bytes))
        .ok_or_else(|| Error::from(E_FAIL))?;
    // `OnDemand` for the same reason as the by-path twin: this often decides it cannot help and
    // returns without decoding, so eagerly walking the metadata graph would be pure loss on top
    // of the normal decode the caller then runs.
    let decoder = factory.CreateDecoderFromStream(
        &stream,
        std::ptr::null(),
        WICDecodeMetadataCacheOnDemand,
    )?;
    let frame = decoder.GetFrame(0)?;
    if !codec_scales_natively(&frame, target_edge) {
        return Err(Error::from(E_FAIL));
    }
    wic_decode_frame(&factory, &frame, Some(target_edge), head)
}

/// Whether the codec behind `frame` can decode at a REDUCED size natively, rather than
/// decoding everything and resampling afterwards. See [`wic_decode_path_if_codec_scales`].
unsafe fn codec_scales_natively(frame: &IWICBitmapFrameDecode, target_edge: u32) -> bool {
    let Ok(transform) = frame.cast::<IWICBitmapSourceTransform>() else {
        return false; // codec exposes no transform interface at all
    };
    let (mut w, mut h) = (0u32, 0u32);
    if frame.GetSize(&mut w, &mut h).is_err() || w == 0 || h == 0 {
        return false;
    }
    if w.max(h) <= target_edge {
        return false; // already small enough — nothing to gain
    }
    // Ask "can you reduce AT ALL", not "can you hit exactly this size". `GetClosestSize`
    // answers with the nearest size it supports, and a JPEG supports only halvings — so
    // requesting 2048 from a 4000 px image gets 4000 back (2000 being below the request), and
    // a probe keyed on the exact target rejects the very codec it exists to accept. That
    // happened; every JPEG silently lost the fast path. Requesting 1x1 asks the real question,
    // and `IWICBitmapScaler` then picks whichever supported size actually helps.
    let (mut cw, mut ch) = (1u32, 1u32);
    if transform.GetClosestSize(&mut cw, &mut ch).is_err() {
        return false;
    }
    // A codec with no reduced-size support hands the full dimensions straight back.
    cw < w || ch < h
}

pub(super) unsafe fn wic_decode_frame(
    factory: &IWICImagingFactory,
    frame: &IWICBitmapFrameDecode,
    thumbnail_cx: Option<u32>,
    container_bytes: &[u8],
) -> Result<DynamicImage> {
    let mut w: u32 = 0;
    let mut h: u32 = 0;
    frame.GetSize(&mut w, &mut h)?;
    // Bomb guard for the WIC tier: per-edge MAX_DIM and total MAX_PIXELS, both
    // from `limits`. MAX_PIXELS (~1 GiB RGBA) is intentionally a higher ceiling
    // than the `image` tier's 512 MiB alloc cap — see the reconciliation note on
    // `limits::MAX_ALLOC` for why the two ceilings differ (single final
    // OS-decoded buffer vs. multiplied in-process transients).
    if w == 0 || h == 0 || w > MAX_DIM || h > MAX_DIM || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err(Error::from(E_FAIL));
    }

    // SCALE FIRST, CONVERT SECOND — and the order is the whole performance story.
    //
    // `IWICBitmapScaler` asks its SOURCE for `IWICBitmapSourceTransform`, and a codec that
    // implements it can decode at a reduced size natively: JPEG does exactly the DCT-domain
    // trick fast viewers are built on (decode only the coefficients needed for 1/2, 1/4, 1/8),
    // and several other codecs do their own equivalent. A frame exposes that interface; a
    // FORMAT CONVERTER does not. So with the converter in between, the scaler could only ever
    // resize an already-fully-decoded frame — the codec still did all the work, and the
    // saving was limited to the final `CopyPixels` allocation.
    //
    // Scaling first also shrinks the format conversion and the ICC pass, which now run over
    // the small image instead of the large one.
    let scaled: IWICBitmapSource = match thumbnail_cx {
        Some(cx) if w.max(h) > cx => {
            let long = w.max(h);
            let target_w = ((w as u64 * cx as u64 + long as u64 / 2) / long as u64).max(1) as u32;
            let target_h = ((h as u64 * cx as u64 + long as u64 / 2) / long as u64).max(1) as u32;
            let scaler = factory.CreateBitmapScaler()?;
            scaler.Initialize(frame, target_w, target_h, WICBitmapInterpolationModeFant)?;
            scaler.cast()?
        }
        _ => frame.cast()?,
    };

    // Convert to straight 32bpp RGBA (dib.rs handles the premultiply). This has to come after
    // the scaler precisely because the scaler does NOT promise to preserve its source's pixel
    // format — with Fant it hands back WIC's native BGRA, which read as RGBA swaps red and
    // blue on every scaled thumbnail. Converting afterwards makes that unrepresentable rather
    // than something to remember (`wic_thumbnail_scaling_keeps_rgba_channel_order` pins it).
    let converter = factory.CreateFormatConverter()?;
    converter.Initialize(
        &scaled,
        &GUID_WICPixelFormat32bppRGBA,
        WICBitmapDitherTypeNone,
        None,
        0.0,
        // Palette args are unused for a non-indexed (32bppRGBA) destination;
        // Custom is the idiomatic "no palette" value.
        WICBitmapPaletteTypeCustom,
    )?;
    let source: IWICBitmapSource = converter.cast()?;
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
