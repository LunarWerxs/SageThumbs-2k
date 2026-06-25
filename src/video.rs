//! Video thumbnails via Windows **Media Foundation** — grab a representative frame using
//! the OS's installed codecs, so we bundle **zero** extra bytes (same "use the OS" stance
//! as the WIC and WinRT-PDF/OCR tiers). We never stream a multi-GB original through MF: the
//! caller feeds either a real file path ([`frame_from_path`], for non-sandboxed hosts) or a
//! small in-memory buffer ([`frame_from_bytes`]) — a bounded head prefix, a remux, or (best)
//! a one-keyframe mini-MP4 built by [`crate::mp4`] that targets the ~30% representative frame.
//!
//! Everything here is best-effort and additive: an unsupported container/codec, a missing
//! video stream, or any decode error returns `None`, and the file simply keeps its default
//! icon — never worse than before. A non-video ISO-BMFF (HEIC/AVIF, which share the `ftyp`
//! box) is excluded by [`is_video_magic`] so the image tiers still handle it.

use std::time::Duration;

use image::{DynamicImage, RgbaImage};
use windows::core::{GUID, HSTRING};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantToUInt64, PROPVARIANT};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, IStream, COINIT_MULTITHREADED,
};
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
    // MPEG-TS (.ts/.mts): 188-byte packets, each led by the 0x47 sync byte. Requiring TWO
    // syncs (head[0] AND head[188]) avoids matching any file that merely starts with 'G'.
    // M2TS (.m2ts) prefixes each packet with a 4-byte timestamp → sync at offset 4, 192 stride.
    // (Needs a head ≥197 bytes — `peek_is_video`/`decode` pass enough; a short head just skips.)
    if head.len() > 188 && head[0] == 0x47 && head[188] == 0x47 {
        return true;
    }
    if head.len() > 196 && head[4] == 0x47 && head[196] == 0x47 {
        return true;
    }
    head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3])                 // Matroska / WebM (EBML)
        || (head.starts_with(b"RIFF") && &head[8..12] == b"AVI ") // AVI
        || head.starts_with(&[0x30, 0x26, 0xB2, 0x75])          // ASF / WMV header GUID
        || head.starts_with(b"FLV")                              // Flash Video
        || head.starts_with(&[0x00, 0x00, 0x01, 0xBA])          // MPEG program-stream pack header
        || head.starts_with(&[0x00, 0x00, 0x01, 0xB3])          // MPEG video sequence header (.m2v, raw .mpg)
        // Ogg (.ogv carries Theora/VP8 video). Ogg AUDIO (Vorbis/Opus/Speex) ALSO uses this
        // magic, so a frame-grab miss must fall back to the album-art path — the CLI
        // (`decode_preview_with_raw_order`) already falls through to `extract_cover`, and the
        // thumbnail provider's video branch falls through to `audio_art` for OggS (see there).
        || head.starts_with(b"OggS")
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

/// Grab a frame by FILE PATH — Media Foundation opens the file itself (efficient seeks +
/// read-ahead), the FAST path that matches what Windows' own video thumbnailer does. Used
/// when we can recover the path; otherwise the caller decodes a bounded prefix in memory
/// ([`frame_from_bytes`]). We deliberately NEVER decode the multi-GB original *through* the
/// shell's thumbnail `IStream`: MF's random access on it pegs a core for 30 s+ (far past
/// Explorer's timeout → the folder "never thumbnails"), while the file opened directly is
/// <1 s. The path is `Send`, so it runs on the budgeted worker under [`VIDEO_TIMEOUT`] — a
/// hostile/odd file fails fast (default icon) instead of pegging the host.
pub fn frame_from_path(path: &str) -> Option<DynamicImage> {
    let owned = path.to_string();
    grab_budgeted(move || unsafe {
        let _session = MfSession::start()?;
        let attrs = grab_attrs()?;
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(owned.as_str()), &attrs).ok()?;
        // Direct file access: Media Foundation seeks efficiently via the file's own index
        // (exactly what Windows' own thumbnailer does), so we jump to the TRUE 30% mark for a
        // representative frame — no need for the bounded buffer's near-the-head seek cap.
        grab_reader(&reader, Seek { frac: 0.30, cap_hns: None })
    })
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
        // The buffer is either a bounded head prefix / remux (reach only EARLY frames — stay
        // near the head, 10% capped at 3s) or a one-keyframe mini-MP4 from `crate::mp4` (a
        // single sample, so the 10% seek of its ~one-frame duration is a harmless no-op and we
        // grab that keyframe directly). Both are served by the same near-the-head plan.
        grab(&bs, Seek { frac: 0.10, cap_hns: Some(MAX_SEEK_HNS) })
    })
}

/// Grab a frame from a full in-memory buffer at the TRUE representative mark (~30 %, no depth
/// cap). For callers that hold the WHOLE file in RAM (the size-capped CLI read), so MF can seek
/// freely via the container's own index — unlike [`frame_from_bytes`], whose 3 s cap assumes a
/// bounded head prefix. Used as the CLI/preview fallback for non-MP4/MKV containers.
pub fn frame_from_bytes_repr(bytes: &[u8]) -> Option<DynamicImage> {
    let owned = bytes.to_vec();
    grab_budgeted(move || unsafe {
        let stream = SHCreateMemStream(Some(&owned))?;
        let bs = MFCreateMFByteStreamOnStream(&stream).ok()?;
        grab(&bs, Seek { frac: 0.30, cap_hns: None })
    })
}

/// Grab a representative ~30 % frame for a video MF can demux but we have no bespoke index
/// parser for (AVI, WMV/ASF, …), by letting MF seek the file's real index over a
/// [`crate::vstream::BlockCacheStream`] wrapping the shell `IStream`. `size` is the stream
/// length (the caller already has it).
///
/// Runs **inline on the calling thread** — NOT on the budgeted worker. The shell thumbnail
/// `IStream` is apartment-bound to this thread; handing it to a worker deadlocks (the worker's
/// reads marshal back to this thread, which would be blocked waiting on the worker). Inline is
/// exactly how the old (deleted) `frame_from_istream` ran — but that was a 30 s meltdown because
/// MF made thousands of tiny marshaled reads. Block-caching collapses those into a handful of big
/// reads, and the stream's wall-clock [`VIDEO_TIMEOUT`] deadline + byte budget keep it bounded
/// even without the worker timeout. Returns `None` (→ caller falls back) on any failure.
pub fn frame_from_block_stream(shell: &IStream, size: u64, frac: f64) -> Option<DynamicImage> {
    // S_OK / S_FALSE both add a ref; RPC_E_CHANGED_MODE means COM is already up on this thread.
    let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let r = unsafe { grab_block_stream(shell.clone(), size, Seek { frac, cap_hns: None }) };
    if inited {
        unsafe { CoUninitialize() };
    }
    r
}

/// Wrap `inner` (an `IStream` valid on the current thread) in a block-caching stream and grab.
/// The block stream carries a [`VIDEO_TIMEOUT`] wall-clock deadline so its source reads are
/// bounded even when this runs inline (no worker thread).
unsafe fn grab_block_stream(inner: IStream, size: u64, seek: Seek) -> Option<DynamicImage> {
    let _session = MfSession::start()?;
    let deadline = std::time::Instant::now() + VIDEO_TIMEOUT;
    let bcs: IStream = crate::vstream::BlockCacheStream::new(inner, size, deadline).into();
    let bs = MFCreateMFByteStreamOnStream(&bcs).ok()?;
    let attrs = grab_attrs()?;
    let reader = MFCreateSourceReaderFromByteStream(&bs, &attrs).ok()?;
    grab_reader(&reader, seek)
}

/// Test-only: exercise the block-caching path over a real file (a file-backed `IStream` opened
/// on the worker, so no GIT marshaling is needed). Mirrors `frame_from_block_stream`'s decode.
#[cfg(test)]
pub fn frame_from_block_stream_file(path: &str, frac: f64) -> Option<DynamicImage> {
    use windows::Win32::System::Com::{STATFLAG_NONAME, STATSTG, STGM_READ};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
    let owned = path.to_string();
    grab_budgeted(move || unsafe {
        let inner =
            SHCreateStreamOnFileEx(&HSTRING::from(owned.as_str()), STGM_READ.0, 0, false, None)
                .ok()?;
        let mut stat = STATSTG::default();
        inner.Stat(&mut stat, STATFLAG_NONAME).ok()?;
        grab_block_stream(inner, stat.cbSize, Seek { frac, cap_hns: None })
    })
}

/// How [`grab_reader`] positions the reader before grabbing. `frac` is the fraction of the
/// running time to seek to; `cap_hns` optionally caps the seek depth (in 100-ns units) so a
/// bounded in-memory buffer never seeks past the bytes it actually contains.
#[derive(Clone, Copy)]
struct Seek {
    frac: f64,
    cap_hns: Option<i64>,
}

/// 3 s in 100-ns units — the depth cap for bounded-buffer seeks (see [`frame_from_bytes`]).
const MAX_SEEK_HNS: i64 = 3 * 10_000_000;

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
        // Pin the DLL for this detached worker's whole lifetime: on timeout we return but
        // leave it running, and `DllCanUnloadNow` ignores it, so the thumbnail host could
        // unload the DLL mid-grab and crash. Mirrors run_action_detached.
        #[allow(clippy::default_constructed_unit_structs)]
        let _module = crate::ModuleRef::default();
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

/// Wrap a byte stream in a source reader and grab (the in-memory + shell-IStream paths).
unsafe fn grab(bs: &IMFByteStream, seek: Seek) -> Option<DynamicImage> {
    let _session = MfSession::start()?;
    let attrs = grab_attrs()?;
    let reader = MFCreateSourceReaderFromByteStream(bs, &attrs).ok()?;
    grab_reader(&reader, seek)
}

/// Source-reader attributes — enable the video processor so it converts whatever the codec
/// outputs (NV12/YUV…) to the RGB32 [`grab_reader`] asks for.
unsafe fn grab_attrs() -> Option<IMFAttributes> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1).ok()?;
    let attrs = attrs?;
    attrs.SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1).ok()?;
    Some(attrs)
}

/// Core: a configured source-reader → RGB32 → first decoded frame → straight-RGBA image.
unsafe fn grab_reader(reader: &IMFSourceReader, seek: Seek) -> Option<DynamicImage> {
    let first_video = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    // Ask the first video stream for RGB32 output. Fails fast (→ None) for audio-only files
    // or codecs the OS can't decode, so they keep their default icon.
    let want = MFCreateMediaType().ok()?;
    want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
    want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_RGB32).ok()?;
    reader.SetCurrentMediaType(first_video, None, &want).ok()?;

    // Seek to a representative point before grabbing — most videos open on black / a fade-in /
    // a studio logo, so a thumbnail of frame 0 is useless. How far in (and whether the depth is
    // capped) depends on the source: a direct file path seeks to the true 30% mark; a bounded
    // in-memory buffer stays near the head. Best-effort: an unknown duration or a non-seekable
    // source just leaves us at the start. The read loop below grabs the first decoded keyframe
    // at/after the seek point.
    seek_to_fraction(reader, seek);

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

/// Best-effort seek to `seek.frac` of the running time (e.g. 0.30 = 30% in) so the grabbed
/// frame is representative rather than frame 0 (usually black / a fade-in / a logo).
/// Every step is fallible and ignored: an unknown duration, a non-seekable source, or a
/// codec that rejects the seek just leaves the reader at the start — the caller still
/// gets *a* frame. Time is in 100-ns units; an all-zero time-format GUID = the default.
unsafe fn seek_to_fraction(reader: &IMFSourceReader, seek: Seek) {
    let stream = MF_SOURCE_READER_MEDIASOURCE.0 as u32;
    let Ok(pv) = reader.GetPresentationAttribute(stream, &MF_PD_DURATION) else {
        return;
    };
    let dur_hns = PropVariantToUInt64(&pv).unwrap_or(0);
    if dur_hns == 0 {
        return;
    }
    // A bounded in-memory buffer (the `frame_from_bytes` prefix/remux tiers) passes a depth
    // `cap_hns`: a percentage seek into a long movie lands very deep (10% of a 2-hour 4K file ≈
    // hundreds of MB in), past the bytes the buffer actually holds — staying within the first
    // few seconds keeps the read inside the retained head. A direct file path passes no cap, so
    // it reaches the true representative mark. (The original shell-IStream meltdown — a deep
    // random read pegging a core for 30 s+ — is sidestepped entirely now: we never stream the
    // multi-GB original through MF; we feed it either a bounded buffer or a one-keyframe file.)
    let mut target = (dur_hns as f64 * seek.frac.clamp(0.0, 0.95)) as i64;
    if let Some(cap) = seek.cap_hns {
        target = target.min(cap);
    }
    let pos = PROPVARIANT::from(target);
    let _ = reader.SetCurrentPosition(&GUID::zeroed(), &pos);
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

#[cfg(test)]
mod tests {
    use std::path::Path;

    /// The block-caching stream must let Media Foundation decode a representative frame from
    /// containers we have no bespoke index parser for (AVI, WMV). Runs against downloaded
    /// samples if present (skips on CI, where they're absent — no fixtures committed).
    #[test]
    fn block_stream_decodes_avi_and_wmv() {
        let samples = [
            r"D:\st2k-target\_vidsamples\sample_640x360.avi",
            r"D:\st2k-target\_vidsamples\sample-avi-file.avi",
            r"D:\st2k-target\_vidsamples\sample_640x360.wmv",
        ];
        let mut tested = 0;
        for path in samples {
            if !Path::new(path).is_file() {
                continue;
            }
            tested += 1;
            let frame = super::frame_from_block_stream_file(path, 0.30)
                .unwrap_or_else(|| panic!("block stream failed to decode {path}"));
            assert!(frame.width() > 0 && frame.height() > 0);
            eprintln!(
                "block_stream: {path} → {}x{}",
                frame.width(),
                frame.height()
            );
        }
        if tested == 0 {
            eprintln!("block_stream_decodes_avi_and_wmv: no samples present — skipping");
        }
    }
}
