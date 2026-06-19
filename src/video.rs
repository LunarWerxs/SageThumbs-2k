//! Video thumbnails via Windows **Media Foundation** — grab a representative frame using
//! the OS's installed codecs, so we bundle **zero** extra bytes (same "use the OS" stance
//! as the WIC and WinRT-PDF/OCR tiers). The frame is read through an `IMFByteStream`, which
//! streams from the source on demand, so the shell path ([`frame_from_istream`]) never
//! buffers a multi-GB movie into memory.
//!
//! Everything here is best-effort and additive: an unsupported container/codec, a missing
//! video stream, or any decode error returns `None`, and the file simply keeps its default
//! icon — never worse than before. A non-video ISO-BMFF (HEIC/AVIF, which share the `ftyp`
//! box) is excluded by [`is_video_magic`] so the image tiers still handle it.

use std::time::Duration;

use image::{DynamicImage, RgbaImage};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, IStream, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::SHCreateMemStream;

/// D3DFMT_X8R8G8B8 — the format id for `MFVideoFormat_RGB32`, for the stride fallback.
const RGB32_FOURCC: u32 = 22;

/// Cheap magic sniff: does this byte head look like a video container MF might decode?
/// Gates the (relatively expensive) MF startup so only actual videos pay for it. The
/// `ftyp` brands for HEIC/AVIF images are excluded — those are decoded as images, not video.
pub fn is_video_magic(head: &[u8]) -> bool {
    if head.len() < 12 {
        return false;
    }
    if &head[4..8] == b"ftyp" {
        let brand = &head[8..12];
        // ISO-BMFF is shared by HEIC/AVIF (images) and M4A/M4B (audio); exclude those
        // brands so they're handled by the image tiers / audio-art path, not as video.
        let not_video = brand == b"heic"
            || brand == b"heix"
            || brand == b"heim"
            || brand == b"heis"
            || brand == b"mif1"
            || brand == b"msf1"
            || brand == b"avif"
            || brand == b"avis"
            || brand == b"heif"
            || brand == b"M4A "
            || brand == b"M4B "
            || brand == b"M4P ";
        return !not_video; // mp4/mov/m4v/3gp brands → video
    }
    head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])                 // Matroska / WebM (EBML)
        || (head.starts_with(b"RIFF") && &head[8..12] == b"AVI ") // AVI
        || head.starts_with(&[0x30, 0x26, 0xB2, 0x75])          // ASF / WMV header GUID
        || head.starts_with(b"FLV")                              // Flash Video
        || head.starts_with(&[0x00, 0x00, 0x01, 0xBA]) // MPEG program stream pack header
}

/// Balances `MFStartup` with `MFShutdown` (both are ref-counted, so per-call is safe).
struct MfSession;
impl MfSession {
    unsafe fn start() -> Option<Self> {
        MFStartup(MF_VERSION, MFSTARTUP_LITE).ok()?;
        Some(MfSession)
    }
}
impl Drop for MfSession {
    fn drop(&mut self) {
        unsafe {
            let _ = MFShutdown();
        }
    }
}

/// Wall-clock cap on a single in-memory video frame-grab. Media Foundation's `ReadSample`
/// has no internal timeout, so a stalling/hostile codec could otherwise spin the calling
/// thread; the 64-sample cap in [`grab`] bounds samples skipped, NOT time inside the codec.
/// We run the grab on a worker joined with this deadline (mirrors the SVG/PDF tiers); on
/// expiry we return `None` (default icon) and let the worker exit on its own.
const VIDEO_TIMEOUT: Duration = Duration::from_secs(8);

/// Grab a frame from a COM `IStream` (the shell thumbnail path) — streams from disk, never
/// buffering the whole file. Runs in the ISOLATED thumbnail host (`dllhost.exe`), so it is
/// bounded by process isolation + Explorer's own thumbnail timeout rather than an
/// in-process wall-clock cap — keeping the zero-buffering stream for multi-GB movies. The
/// in-memory [`frame_from_bytes`] path (which we DO own the thread for) is the one wrapped
/// in [`VIDEO_TIMEOUT`].
pub fn frame_from_istream(stream: &IStream) -> Option<DynamicImage> {
    unsafe {
        let bs = MFCreateMFByteStreamOnStream(stream).ok()?;
        grab(&bs)
    }
}

/// Grab a frame from in-memory bytes (the CLI / `decode_preview` path). Wraps the bytes in
/// a memory stream — fine for the size-capped CLI read, not the unbounded shell path.
/// Bounded by [`VIDEO_TIMEOUT`] so a codec that wedges inside `ReadSample` can't hang the
/// caller's thread.
pub fn frame_from_bytes(bytes: &[u8]) -> Option<DynamicImage> {
    let owned = bytes.to_vec();
    grab_budgeted(move || unsafe {
        let stream = SHCreateMemStream(Some(&owned))?;
        let bs = MFCreateMFByteStreamOnStream(&stream).ok()?;
        grab(&bs)
    })
}

/// Run a frame-grab closure on a worker thread under [`VIDEO_TIMEOUT`]. The worker owns its
/// inputs and initializes its own (MTA) COM apartment for the MF / WIC components; on
/// timeout the receiver is dropped and the worker simply finishes and exits (a leaked
/// thread in a disposable host is acceptable — same trade as `decode_svg` / `pdf`).
fn grab_budgeted<F>(f: F) -> Option<DynamicImage>
where
    F: FnOnce() -> Option<DynamicImage> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // S_OK / S_FALSE both add a ref to balance; RPC_E_CHANGED_MODE does not.
        let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
        let r = f();
        if inited {
            unsafe { CoUninitialize() };
        }
        let _ = tx.send(r);
    });
    rx.recv_timeout(VIDEO_TIMEOUT).ok().flatten()
}

/// Core: source-reader → RGB32 → first decoded frame → straight-RGBA image.
unsafe fn grab(bs: &IMFByteStream) -> Option<DynamicImage> {
    let _session = MfSession::start()?;

    // Enable the video processor so it converts whatever the codec outputs (NV12/YUV…) to
    // the RGB32 we ask for below.
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1).ok()?;
    let attrs = attrs?;
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).ok()?;

    let reader = MFCreateSourceReaderFromByteStream(bs, &attrs).ok()?;
    let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    // Ask the first video stream for RGB32 output. Fails fast (→ None) for audio-only files
    // or codecs the OS can't decode, so they keep their default icon.
    let want = MFCreateMediaType().ok()?;
    want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
    want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ok()?;
    reader.SetCurrentMediaType(first_video, None, &want).ok()?;

    // Read decoded samples until one carries a buffer — skipping stream ticks / format-change
    // notifications (a null sample with no end-of-stream flag). Bounded so a pathological
    // file can't spin.
    let mut sample: Option<IMFSample> = None;
    for _ in 0..64 {
        let mut flags: u32 = 0;
        let mut smp: Option<IMFSample> = None;
        reader.ReadSample(first_video, 0, None, Some(&mut flags), None, Some(&mut smp)).ok()?;
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        if let Some(s) = smp {
            sample = Some(s);
            break;
        }
    }
    let sample = sample?;

    // Geometry of the negotiated output frame.
    let out = reader.GetCurrentMediaType(first_video).ok()?;
    let size = out.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
    let w = (size >> 32) as u32;
    let h = (size & 0xFFFF_FFFF) as u32;
    if w == 0 || h == 0 || w > 16384 || h > 16384 {
        return None;
    }
    // Signed default stride: negative = bottom-up. Prefer the negotiated attribute, fall
    // back to the canonical RGB32 stride, then to a packed top-down row.
    let stride = out
        .GetUINT32(&MF_MT_DEFAULT_STRIDE)
        .map(|s| s as i32)
        .ok()
        .or_else(|| MFGetStrideForBitmapInfoHeader(RGB32_FOURCC, w).ok())
        .unwrap_or((w * 4) as i32);

    // Lock the contiguous frame buffer and copy BGRX → top-down straight-RGBA.
    let buffer = sample.ConvertToContiguousBuffer().ok()?;
    let mut data: *mut u8 = std::ptr::null_mut();
    let mut max_len: u32 = 0;
    buffer.Lock(&mut data, Some(&mut max_len), None).ok()?;
    let rgba = copy_bgrx_to_rgba(data, max_len as usize, w, h, stride);
    let _ = buffer.Unlock();

    let img = RgbaImage::from_raw(w, h, rgba?)?;
    Some(DynamicImage::ImageRgba8(img))
}

/// Copy an MF RGB32 (`BGRX`) frame into top-down straight-RGBA, honoring `stride` (negative
/// = bottom-up). Fully bounds-checked: returns `None` if the locked buffer is smaller than
/// the geometry claims, so a short/hostile buffer can't trigger an over-read.
unsafe fn copy_bgrx_to_rgba(data: *const u8, len: usize, w: u32, h: u32, stride: i32) -> Option<Vec<u8>> {
    if data.is_null() {
        return None;
    }
    let (w, h) = (w as usize, h as usize);
    let abs_stride = stride.unsigned_abs() as usize;
    if abs_stride < w * 4 || abs_stride.checked_mul(h)? > len {
        return None;
    }
    let src = std::slice::from_raw_parts(data, len);
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        // Bottom-up source when stride < 0: read the last row first.
        let srow = if stride < 0 { (h - 1 - y) * abs_stride } else { y * abs_stride };
        let drow = y * w * 4;
        for x in 0..w {
            let s = srow + x * 4;
            let d = drow + x * 4;
            out[d] = src[s + 2]; // R (BGRX byte 2)
            out[d + 1] = src[s + 1]; // G
            out[d + 2] = src[s]; // B
            out[d + 3] = 255; // X → opaque
        }
    }
    Some(out)
}
