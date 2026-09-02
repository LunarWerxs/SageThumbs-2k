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
use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, IStream, COINIT_MULTITHREADED};
use windows::Win32::UI::Shell::SHCreateMemStream;

/// D3DFMT_X8R8G8B8 — the format id for `MFVideoFormat_RGB32`, for the stride fallback.
const RGB32_FOURCC: u32 = 22;

/// Turn a decoded frame the way the container's display matrix asks (issue #32), given the
/// CLOCKWISE angle from [`crate::mp4::display_rotation`].
///
/// The image counterpart is `decode::apply_exif_orientation`, and this is deliberately the
/// same shape: metadata says which way is up, and the pixels get turned once, at the end,
/// where every producer of them converges.
///
/// **It cannot double-rotate, and that is a property of our Media Foundation setup rather
/// than luck.** Every frame in this module comes from an `IMFSourceReader` asked for
/// NV12/RGB32 output, whose `MF_MT_FRAME_SIZE` is the CODED size: the Source Reader hands
/// back the decoder's pixels untouched, because applying the display matrix is a renderer's
/// job (the EVR / Media Engine), not a reader's. Measured before this was written — the four
/// files from the issue's own `ffmpeg -display_rotation` commands all came back 640x360.
/// The remuxing tiers cannot rotate either: `mp4::build_mini_mp4` writes a unity matrix.
///
/// Anything but 90/180/270 returns the frame untouched, so a caller may pass a rotation it
/// did not check.
pub fn apply_display_rotation(img: DynamicImage, clockwise_degrees: u32) -> DynamicImage {
    match clockwise_degrees {
        90 => img.rotate90(),
        180 => img.rotate180(),
        270 => img.rotate270(),
        _ => img,
    }
}

/// Is Media Foundation actually present on this machine?
///
/// `mfplat.dll` / `mfreadwrite.dll` are **delay-loaded** (see `delay_load_media_foundation`
/// in both build scripts) precisely so that a Windows edition without Media Foundation can
/// still LOAD the binary. Those editions are real and shipping: the **"N" and "KN" SKUs**
/// sold in the EU and Korea, and Server core. As a static import, a missing `mfplat.dll`
/// makes the loader refuse the entire shell extension — every format loses its thumbnail,
/// the context menu never appears, and Windows reports nothing at all.
///
/// **Every MF call site must be gated on this.** A delay-load stub for a DLL that cannot be
/// found raises a *structured exception*, and this crate builds `panic = "abort"`, so an
/// unguarded call would abort the host process rather than degrade. Checking first turns
/// that into a plain `None` and the file keeps its default icon.
///
/// The probe deliberately does **not** `FreeLibrary`: keeping the module pinned for the
/// process lifetime means the delay-load resolution that follows cannot then fail, and it
/// costs one handle on a machine that was going to load MF anyway.
pub fn media_foundation_available() -> bool {
    use std::sync::OnceLock;
    use windows::core::PCWSTR;
    use windows::Win32::System::LibraryLoader::LoadLibraryW;
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        ["mfplat.dll\0", "mfreadwrite.dll\0"].iter().all(|name| {
            let wide: Vec<u16> = name.encode_utf16().collect();
            unsafe { LoadLibraryW(PCWSTR(wide.as_ptr())).is_ok() }
        })
    })
}

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
        //
        // CHECK THE COMPATIBLE BRANDS TOO, not just the major one. A major brand is whatever
        // the encoder felt like declaring, and getting this wrong is not a soft failure: the
        // shell cascade STOPS on "video that decoded no frame" rather than falling through to
        // the image tiers, so one unrecognised image brand means a stock icon in Explorer
        // forever. That shipped: libheif writes `mif3` (MIAF, ISO/IEC 23000-22) as the major
        // brand of an alpha AVIF, `mif3` was not in the list below, and the file thumbnailed
        // perfectly through the CLI while Explorer showed nothing. Every real image file also
        // lists a KNOWN still brand among its compatible brands, so reading them turns an
        // allowlist that must be exhaustive into one that merely has to be representative.
        let is_still = |b: &[u8]| {
            matches!(
                b,
                b"heic"
                    | b"heix"
                    | b"heim"
                    | b"heis"
                    | b"hevc"
                    | b"hevx"
                    | b"mif1"
                    | b"mif2"
                    | b"mif3"
                    | b"msf1"
                    | b"miaf"
                    | b"MA1A"
                    | b"MA1B"
                    | b"avif"
                    | b"avio"
                    | b"avis"
                    | b"heif"
                    | b"jxl "
            )
        };
        // The ftyp box: [size:4][ftyp:4][major:4][minor_version:4][compatible brands...].
        // Bounded by the declared box size AND by what we were actually handed.
        //
        // The `head.len() >= 16` guard is LOAD-BEARING and must stay OUTSIDE the clamp, not
        // folded into it. Compatible brands start at offset 16, but the function only
        // requires 12 bytes (the major brand ends there), so a 12..=15 byte head reaches
        // here — and `clamp(16, head.len())` is then `min > max`, which PANICS by contract.
        // This parser runs in-process inside `explorer.exe` under `panic = "abort"` (see
        // safety.rs), so a 13-byte ftyp-shaped file or stream would abort the user's shell.
        // Skipping the scan is the correct degrade: a head that short carries no compatible
        // brands at all, and the major-brand check below still classifies it.
        // Found by the always-on `fuzz::parsers_survive_mutation_of_synthetic_seeds` gate.
        if head.len() >= 16 {
            let box_end = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
            let end = box_end.clamp(16, head.len());
            if head[16..end].chunks_exact(4).any(is_still) {
                return false;
            }
        }
        let not_video = is_still(brand)
            || brand == b"M4A "
            || brand == b"M4B "
            || brand == b"M4P "
            // Canon CR3 RAW is ISO-BMFF too (both brands are seen in the wild — see
            // rawsniff.rs's own `crx `/`cr3 ` check). Without this exclusion a CR3 is
            // misrouted into the video cascade, every MF tier fails to demux a RAW photo,
            // and streamsrc.rs returns E_FAIL directly with no fall-through to the RAW/WIC
            // cascade that already knows this format.
            || brand == b"crx "
            || brand == b"cr3 ";
        return !not_video; // mp4/mov/m4v/3gp brands → video
    }
    // MPEG-TS (.ts/.mts): 188-byte packets, each led by the 0x47 sync byte. Requiring TWO
    // syncs (head[0] AND head[188]) avoids matching any file that merely starts with 'G'.
    // M2TS (.m2ts) prefixes each packet with a 4-byte timestamp → sync at offset 4, 192 stride.
    // (Needs a head ≥197 bytes — `StreamHead::is_video`/`decode` pass enough; a short head just skips.)
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
/// `pub(crate)` for `vcodec`'s decoder probe, which is the only other MF call site.
pub(crate) struct MfSession;
impl MfSession {
    pub(crate) unsafe fn start() -> Option<Self> {
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

/// Grab a frame by FILE PATH — Media Foundation opens the file itself and seeks via its own
/// index. **Current role: a last resort**, not the hot path the name once implied — the only
/// caller left is `strip::read_info_verbose`'s unbounded width/height rescue when nothing
/// cheaper found dimensions (`strip.rs:250`); every shell-facing thumbnail/preview goes
/// through the shell `IStream` tiers instead ([`frame_from_bytes`]/[`frame_from_block_stream`]),
/// because this path can spawn a long-lived MF worker that has no place in the in-shell
/// budget. We deliberately NEVER decode the multi-GB original *through* the shell's thumbnail
/// `IStream`: MF's random access on it pegs a core for 30 s+ (far past Explorer's timeout →
/// the folder "never thumbnails"), while the file opened directly is <1 s. The path is
/// `Send`, so it runs on the budgeted worker under [`VIDEO_TIMEOUT`] — a hostile/odd file
/// fails fast (default icon) instead of pegging the host.
pub(crate) fn frame_from_path(path: &str) -> Option<DynamicImage> {
    // Media Foundation is delay-loaded; calling into it when absent would raise a
    // structured exception under `panic = "abort"`. See `media_foundation_available`.
    if !media_foundation_available() {
        return None;
    }
    let owned = path.to_string();
    grab_budgeted(move || unsafe {
        let _session = MfSession::start()?;
        let attrs = grab_attrs()?;
        let reader = MFCreateSourceReaderFromURL(&HSTRING::from(owned.as_str()), &attrs).ok()?;
        // Direct file access: Media Foundation seeks efficiently via the file's own index
        // (exactly what Windows' own thumbnailer does), so we jump to the TRUE representative
        // mark — no need for the bounded buffer's near-the-head seek cap.
        grab_reader(
            &reader,
            Seek {
                frac: crate::settings::video_offset_frac(),
                cap_hns: None,
            },
        )
    })
}

/// Grab a frame from in-memory bytes (the CLI / `decode_preview` path). Wraps the bytes in
/// a memory stream — fine for the size-capped CLI read, not the unbounded shell path.
/// Bounded by [`VIDEO_TIMEOUT`] so a codec that wedges inside `ReadSample` can't hang the
/// caller's thread.
///
/// Takes a borrowed slice and clones it, so a caller that already owns an unused `Vec<u8>`
/// pays for a second copy it doesn't need. [`frame_from_owned_bytes`] is the same grab
/// without that copy — prefer it when the buffer is already an owned, otherwise-unused
/// `Vec<u8>` (every mp4/mkv/flv remux buffer, `mp4_remux_moov`'s output). This borrowing
/// form stays because some callers only ever hold a slice.
pub fn frame_from_bytes(bytes: &[u8]) -> Option<DynamicImage> {
    frame_from_owned_bytes(bytes.to_vec())
}

/// A decoded frame in the codec's own NV12 layout, colour conversion NOT applied.
pub struct Nv12Frame {
    /// The full contiguous NV12 buffer: `height` rows of Y, then `height/2` rows of
    /// interleaved UV, each row `stride` bytes.
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

/// Decode the first frame and hand back the RAW NV12 pixels — no video processor, no
/// RGB conversion. For callers that must own the YUV→RGB matrix themselves.
///
/// Exists because of the 8-bit BT.601 AVIF path (`decode/avifmf.rs`): both the WIC HEIF
/// glue AND Media Foundation's video processor convert those with BT.709 coefficients
/// (measured: identical wrong numbers from each, worst channel 39/255), so the ONLY
/// component that can be trusted with the matrix is us, fed by the decoder's untouched
/// output. The source reader is created WITHOUT `MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING`
/// on purpose: the AV1 decoder MFT emits NV12 natively, and enabling the processor would
/// put the component this function exists to bypass back into the chain.
pub fn nv12_frame_from_owned_bytes(owned: Vec<u8>) -> Option<Nv12Frame> {
    if !media_foundation_available() {
        return None;
    }
    grab_budgeted(move || unsafe {
        let _session = MfSession::start()?;
        let (reader, stream_index, w, h, stride) = nv12_reader_for_bytes(&owned)?;
        read_first_nv12_sample(&reader, stream_index, w, h, stride)
    })
}

/// Set up a source reader over `owned`, negotiate NV12 as the media type (no video
/// processing — the AV1 decoder MFT emits NV12 natively, and enabling the processor would
/// put the component this function exists to bypass back into the chain), and read back the
/// negotiated frame dimensions and row stride. Returns `(reader, stream_index, w, h, stride)`.
unsafe fn nv12_reader_for_bytes(owned: &[u8]) -> Option<(IMFSourceReader, u32, u32, u32, u32)> {
    let stream = SHCreateMemStream(Some(owned))?;
    let bs = MFCreateMFByteStreamOnStream(&stream).ok()?;
    let reader = MFCreateSourceReaderFromByteStream(&bs, None).ok()?;
    let first = MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32;

    let want = MFCreateMediaType().ok()?;
    want.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).ok()?;
    want.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).ok()?;
    reader.SetCurrentMediaType(first, None, &want).ok()?;

    let cur = reader.GetCurrentMediaType(first).ok()?;
    let frame_size = cur.GetUINT64(&MF_MT_FRAME_SIZE).ok()?;
    let (w, h) = ((frame_size >> 32) as u32, frame_size as u32);
    if w == 0 || h == 0 || (w as u64) * (h as u64) > crate::decode::limits::MAX_PIXELS {
        return None;
    }
    let stride = cur.GetUINT32(&MF_MT_DEFAULT_STRIDE).unwrap_or(w);
    // A negative default stride means bottom-up, which NV12 never is; treat as w.
    let stride = if (stride as i32) < 0 {
        w
    } else {
        stride.max(w)
    };
    Some((reader, first, w, h, stride))
}

/// Read the first real sample from `reader` (bounded like `grab_reader`'s read loop — a
/// one-sample mini-MP4 has nothing to skip, and this path's callers build exactly that) and
/// copy out its NV12 bytes: `h` rows of Y then `h/2` rows of UV, `stride` bytes each.
unsafe fn read_first_nv12_sample(
    reader: &IMFSourceReader,
    stream_index: u32,
    w: u32,
    h: u32,
    stride: u32,
) -> Option<Nv12Frame> {
    for _ in 0..16 {
        let mut flags: u32 = 0;
        let mut smp: Option<IMFSample> = None;
        if reader
            .ReadSample(
                stream_index,
                0,
                None,
                Some(&mut flags),
                None,
                Some(&mut smp),
            )
            .is_err()
        {
            return None;
        }
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            return None;
        }
        let Some(sample) = smp else { continue };
        let Ok(buf) = sample.ConvertToContiguousBuffer() else {
            continue;
        };
        let mut ptr = std::ptr::null_mut();
        let mut len = 0u32;
        if buf.Lock(&mut ptr, None, Some(&mut len)).is_err() {
            continue;
        }
        // NV12: `h` rows of Y then `h/2` rows of UV, `stride` bytes each. A short
        // buffer means the stride guess is wrong — refuse rather than misread.
        let need = (stride as usize) * (h as usize) * 3 / 2;
        let data = if (len as usize) >= need {
            Some(std::slice::from_raw_parts(ptr, need).to_vec())
        } else {
            None
        };
        let _ = buf.Unlock();
        return data.map(|data| Nv12Frame {
            data,
            width: w,
            height: h,
            stride,
        });
    }
    None
}

/// As [`frame_from_bytes`], but takes ownership of the buffer instead of cloning it —
/// `grab_budgeted`'s `'static` bound needs an owned buffer to move onto its worker thread
/// either way, so a caller that already has one should hand it over directly.
pub fn frame_from_owned_bytes(owned: Vec<u8>) -> Option<DynamicImage> {
    // Media Foundation is delay-loaded; calling into it when absent would raise a
    // structured exception under `panic = "abort"`. See `media_foundation_available`.
    if !media_foundation_available() {
        return None;
    }
    grab_budgeted(move || unsafe {
        let stream = SHCreateMemStream(Some(&owned))?;
        let bs = MFCreateMFByteStreamOnStream(&stream).ok()?;
        // The buffer is either a bounded head prefix / remux (reach only EARLY frames — stay
        // near the head, 10% capped at 3s) or a one-keyframe mini-MP4 from `crate::mp4` (a
        // single sample, so the 10% seek of its ~one-frame duration is a harmless no-op and we
        // grab that keyframe directly). Both are served by the same near-the-head plan.
        grab(
            &bs,
            Seek {
                frac: 0.10,
                cap_hns: Some(MAX_SEEK_HNS),
            },
        )
    })
}

/// Grab a frame from a full in-memory buffer at the TRUE representative mark
/// ([`crate::settings::video_offset_frac`], no depth cap). For callers that hold the WHOLE file
/// in RAM (the size-capped CLI read), so MF can seek freely via the container's own index —
/// unlike [`frame_from_bytes`], whose 3 s cap assumes a bounded head prefix. Used as the
/// CLI/preview fallback for non-MP4/MKV containers.
pub fn frame_from_bytes_repr(bytes: &[u8]) -> Option<DynamicImage> {
    // Media Foundation is delay-loaded; calling into it when absent would raise a
    // structured exception under `panic = "abort"`. See `media_foundation_available`.
    if !media_foundation_available() {
        return None;
    }
    let owned = bytes.to_vec();
    grab_budgeted(move || unsafe {
        let stream = SHCreateMemStream(Some(&owned))?;
        let bs = MFCreateMFByteStreamOnStream(&stream).ok()?;
        grab(
            &bs,
            Seek {
                frac: crate::settings::video_offset_frac(),
                cap_hns: None,
            },
        )
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
    // Media Foundation is delay-loaded; calling into it when absent would raise a
    // structured exception under `panic = "abort"`. See `media_foundation_available`.
    if !media_foundation_available() {
        return None;
    }
    // S_OK / S_FALSE both add a ref; RPC_E_CHANGED_MODE means COM is already up on this thread.
    let inited = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.is_ok();
    let shell = shell.clone();
    // BlockCacheStream's own deadline only bounds time spent inside ITS Read calls; time
    // spent inside MFCreateSourceReaderFromByteStream / ReadSample itself (codec init/parsing
    // that never calls back into the stream) has no interrupt of its own — unlike every other
    // frame_from_* tier, which runs on grab_budgeted's worker and is bounded by its
    // recv_timeout. This one can't move there: the shell IStream is apartment-bound to THIS
    // thread (see the doc comment above). Watch it from the side instead, so a stuck call
    // leaves a log line rather than total silence. Logging, not a forced abort: killing a
    // thread mid-COM-call can leave process-wide CRT/COM locks held forever, turning one slow
    // file into a dead host for every OTHER file it's asked to thumbnail — and this only ever
    // pins the isolated dllhost/prevhost host (CLAUDE.md §5), never Explorer's own UI thread.
    let r = with_watchdog(
        VIDEO_TIMEOUT,
        || {
            crate::safety::log(
                "frame_from_block_stream: still running past VIDEO_TIMEOUT (no interrupt inside MF's own calls)",
            );
        },
        || unsafe {
            grab_block_stream(
                shell,
                size,
                Seek {
                    frac,
                    cap_hns: None,
                },
            )
        },
    );
    if inited {
        unsafe { CoUninitialize() };
    }
    r
}

/// Run `f` on the CALLING thread while a side thread watches the wall clock: if `f` hasn't
/// finished by `timeout`, `on_timeout` fires once (the watchdog keeps waiting for `f` after
/// that — it never touches `f`'s thread, it only reports). The watchdog is always joined
/// before returning, so it can never outlive this call. Used for grabs that can't run on
/// [`grab_budgeted`]'s own worker (an apartment-bound `IStream`) but still want the same
/// "a stuck call is visible, not silent" guarantee that worker gives every other tier.
fn with_watchdog<T>(
    timeout: Duration,
    on_timeout: impl FnOnce() + Send + 'static,
    f: impl FnOnce() -> T,
) -> T {
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::Builder::new()
        .name("st2k-video-watchdog".into())
        .spawn(move || {
            if done_rx.recv_timeout(timeout).is_err() {
                on_timeout();
            }
        });
    let r = f();
    // Unblock the watchdog before f's caller can be logged as "still running" — a fast `f`
    // must never race a slow watchdog thread startup into a false-positive log line.
    let _ = done_tx.send(());
    if let Ok(w) = watchdog {
        let _ = w.join();
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
    // Media Foundation is delay-loaded; calling into it when absent would raise a
    // structured exception under `panic = "abort"`. See `media_foundation_available`.
    if !media_foundation_available() {
        return None;
    }
    use windows::Win32::System::Com::{STATFLAG_NONAME, STATSTG, STGM_READ};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
    let owned = path.to_string();
    grab_budgeted(move || unsafe {
        let inner =
            SHCreateStreamOnFileEx(&HSTRING::from(owned.as_str()), STGM_READ.0, 0, false, None)
                .ok()?;
        let mut stat = STATSTG::default();
        inner.Stat(&mut stat, STATFLAG_NONAME).ok()?;
        grab_block_stream(
            inner,
            stat.cbSize,
            Seek {
                frac,
                cap_hns: None,
            },
        )
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
fn grab_budgeted<T, F>(f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> Option<T> + Send + 'static,
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
    attrs
        .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
        .ok()?;
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

    // Read decoded samples, skipping stream ticks / format-change notifications (a null sample
    // with no end-of-stream flag), and skipping frames that decode to BLACK.
    //
    // The black skip is why this is a loop over IMAGES rather than "take the first sample that
    // has a buffer". XviD packed-bitstream AVIs carry N-VOP placeholder frames — a real sample,
    // with a real buffer, whose picture is empty — and landing on one produced a completely
    // black thumbnail even though the user's offset setting was working correctly (issue #26).
    // The same shape covers any file whose seek point happens to sit on a fade or a black lead-in.
    //
    // Bounded twice over: at most `MAX_SAMPLES` reads, and at most `MAX_DECODES` of those get
    // converted, so a pathological file cannot spin and a long run of black frames cannot turn
    // one thumbnail into sixty-four decodes. The FIRST black frame is kept as a fallback, so a
    // genuinely black video still gets its (correct, black) thumbnail rather than none at all.
    const MAX_SAMPLES: usize = 64;
    const MAX_DECODES: usize = 8;

    let mut fallback: Option<DynamicImage> = None;
    let mut decodes = 0usize;
    for _ in 0..MAX_SAMPLES {
        let mut flags: u32 = 0;
        let mut smp: Option<IMFSample> = None;
        // BREAK, never `?`. A read error this far in must not throw away a frame we already
        // decoded: once `fallback` holds a black frame, propagating None would turn "we found
        // only a black frame" into "we found nothing" and lose the thumbnail entirely. A
        // truncated or malformed stream erroring on a later sample is exactly when that bites.
        if reader
            .ReadSample(first_video, 0, None, Some(&mut flags), None, Some(&mut smp))
            .is_err()
        {
            break;
        }
        if flags & (MF_SOURCE_READERF_ENDOFSTREAM.0 as u32) != 0 {
            break;
        }
        let Some(sample) = smp else { continue };
        let Some(img) = image_from_sample(reader, first_video, &sample) else {
            continue;
        };
        decodes += 1;
        if !is_near_black(img.as_bytes()) {
            return Some(img);
        }
        if fallback.is_none() {
            fallback = Some(img);
        }
        if decodes >= MAX_DECODES {
            break;
        }
    }
    fallback
}

/// How dark every sampled pixel must be for a frame to count as "black". Deliberately not zero:
/// a real decoded black frame carries a little noise, and a 16-235 studio-range black lands
/// around 16 rather than 0.
const BLACK_LEVEL: u8 = 18;

/// Whether an RGBA frame is entirely (near-)black, i.e. carries no picture worth showing.
///
/// Scans EVERY pixel and returns the moment it finds one that is not black. A strided sampler
/// was tried first and rejected: a stride of N can step clean over a bright region up to N-1
/// pixels wide, and the failure is asymmetric. Answering "black" makes the caller DISCARD the
/// frame, so a wrong yes throws away the only picture we had, while a wrong no merely keeps
/// today's behaviour. So this errs, deliberately, toward "there is a picture here".
///
/// The full scan is not the cost it looks like. A real picture exits on one of the first few
/// pixels; only a genuinely black frame is read to the end, which is the rare case and a flat
/// linear pass. Alpha is ignored: `copy_bgrx_to_rgba` forces it opaque, so it says nothing
/// about the picture.
fn is_near_black(rgba: &[u8]) -> bool {
    if rgba.len() < 4 {
        return false; // nothing to judge — don't call an unusable buffer black and skip it
    }
    !rgba
        .chunks_exact(4)
        .any(|px| px[0] > BLACK_LEVEL || px[1] > BLACK_LEVEL || px[2] > BLACK_LEVEL)
}

/// Convert one decoded Media Foundation sample into an image, using the reader's CURRENT output
/// geometry. Read per sample rather than once up front because a format change mid-stream
/// re-negotiates width/height/stride, and using stale geometry would shear the picture.
unsafe fn image_from_sample(
    reader: &IMFSourceReader,
    stream: u32,
    sample: &IMFSample,
) -> Option<DynamicImage> {
    let out = reader.GetCurrentMediaType(stream).ok()?;
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
unsafe fn copy_bgrx_to_rgba(
    data: *const u8,
    len: usize,
    w: u32,
    h: u32,
    stride: i32,
) -> Option<Vec<u8>> {
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
        let srow = if stride < 0 {
            (h - 1 - y) * abs_stride
        } else {
            y * abs_stride
        };
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
    use super::is_near_black;
    use std::path::Path;

    /// Build an RGBA buffer of `n` pixels, every channel set to `v`.
    fn flat(n: usize, v: u8) -> Vec<u8> {
        let mut b = Vec::with_capacity(n * 4);
        for _ in 0..n {
            b.extend_from_slice(&[v, v, v, 255]);
        }
        b
    }

    /// THE case this exists for: an XviD packed-bitstream N-VOP placeholder decodes to a real
    /// buffer with no picture in it. Accepting it is how issue #26 got an all-black thumbnail
    /// while the user's offset setting was working perfectly.
    #[test]
    fn an_empty_frame_reads_as_black() {
        assert!(is_near_black(&flat(4000, 0)));
        // Studio-range black sits near 16, not 0, and carries a little decode noise.
        assert!(is_near_black(&flat(4000, 16)));
    }

    /// A real picture must NEVER be discarded as black, or a legitimate dark frame would be
    /// skipped and the thumbnail would come from somewhere the user did not ask for.
    #[test]
    fn a_real_picture_is_not_black() {
        assert!(!is_near_black(&flat(4000, 200)));
        // Just past the threshold, uniformly — the tightest true negative.
        assert!(!is_near_black(&flat(4000, 19)));
    }

    /// A frame that is black almost everywhere but has real content somewhere is a PICTURE
    /// (letterboxed, a fade-in with a logo, a dark scene). The sampler must find it, whichever
    /// row it lands on — so probe every offset a single bright pixel could occupy.
    #[test]
    fn one_bright_region_anywhere_defeats_the_black_verdict() {
        for offset in 0..400usize {
            let mut b = flat(400, 0);
            // Wrap rather than clamp, so the run is genuinely 120px wide at EVERY offset —
            // clamping shortened it near the end and tested the harness, not the code.
            for k in 0..120usize {
                b[((offset + k) % 400) * 4] = 240;
            }
            assert!(
                !is_near_black(&b),
                "a 120px bright run at offset {offset} was called black"
            );
        }
        // And the tightest possible case: ONE bright pixel, at every position. A strided
        // sampler fails this; a full scan cannot.
        for offset in 0..400usize {
            let mut b = flat(400, 0);
            b[offset * 4 + 1] = 240; // green channel, to prove it is not just red that counts
            assert!(
                !is_near_black(&b),
                "a single bright pixel at offset {offset} was called black"
            );
        }
    }

    /// Degenerate buffers must not be called black: "black" makes us SKIP a frame, so a wrong
    /// yes throws away the only picture we had. An empty buffer is unknown, not black.
    #[test]
    fn a_buffer_too_small_to_judge_is_not_called_black() {
        assert!(!is_near_black(&[]));
        assert!(!is_near_black(&[0, 0]));
    }

    /// The block-caching stream must let Media Foundation decode a representative frame from
    /// containers we have no bespoke index parser for (AVI, WMV). Runs against the corpus
    /// samples `scripts\build-corpus.ps1` downloads (`sample.avi`/`sample.wmv`); skips
    /// wherever the corpus isn't present (e.g. CI, or before that script has run).
    #[test]
    fn block_stream_decodes_avi_and_wmv() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let dirs: Vec<_> = ["test-corpus-real", "test-corpus"]
            .into_iter()
            .map(|d| base.join(d))
            .filter(|p| p.exists())
            .collect();
        let samples: Vec<_> = ["sample.avi", "sample.wmv"]
            .into_iter()
            .filter_map(|name| dirs.iter().map(|d| d.join(name)).find(|p| p.is_file()))
            .collect();
        if dirs.is_empty() {
            eprintln!("block_stream_decodes_avi_and_wmv: no test corpus present — skipping");
            return;
        }
        let mut tested = 0;
        for path in &samples {
            tested += 1;
            let path = path.to_str().expect("corpus path is valid UTF-8");
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
            eprintln!("block_stream_decodes_avi_and_wmv: no avi/wmv samples in corpus — skipping");
        }
    }

    /// A CR3 is ISO-BMFF (shares the `ftyp` box with HEIC/AVIF/MP4), so without this
    /// exclusion it gets routed into the video cascade, every MF tier fails to demux a RAW
    /// photo, and `streamsrc.rs` returns `E_FAIL` directly with no fall-through to the
    /// RAW/WIC cascade that already recognizes `crx `/`cr3 ` (`rawsniff.rs`).
    #[test]
    fn cr3_and_crx_ftyp_brands_are_not_video() {
        let head_for = |brand: &[u8; 4]| -> Vec<u8> {
            let mut h = vec![0u8; 12];
            h[4..8].copy_from_slice(b"ftyp");
            h[8..12].copy_from_slice(brand);
            h
        };
        assert!(!super::is_video_magic(&head_for(b"crx ")));
        assert!(!super::is_video_magic(&head_for(b"cr3 ")));
        // A real MP4 brand must still be treated as video — the exclusion list must stay narrow.
        assert!(super::is_video_magic(&head_for(b"isom")));
    }

    /// The watchdog must report an `f` that outlives the timeout, and must never mistake a
    /// fast one for stuck — either mistake defeats its whole purpose (a silent-forever wedge,
    /// or a log line that cries wolf on the normal case).
    #[test]
    fn with_watchdog_fires_only_when_f_outlives_the_timeout() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        use std::time::{Duration, Instant};

        let fast_fired = Arc::new(AtomicBool::new(false));
        let flag = fast_fired.clone();
        // TEN SECONDS, not the 50 ms this used to allow, and the size is the whole point.
        // `f` here returns instantly, so the only way the watchdog can fire is if the test
        // thread is descheduled for longer than the timeout between arming and returning.
        // At 50 ms that is entirely reachable on a loaded machine: this test failed exactly
        // once in the whole suite, while `cargo mutants` was saturating every core, and
        // passed alone immediately afterwards. A flaky gate is worse than no gate, because
        // the next person reads a real failure here as "that one is just noisy".
        //
        // Widening the FAST half costs nothing: an instant `f` is nowhere near 10 s, so the
        // assertion still fails the moment the watchdog genuinely misfires. Only the SLOW
        // half needs its timeout to be small, and it stays small.
        let r = super::with_watchdog(
            Duration::from_secs(10),
            move || flag.store(true, Ordering::SeqCst),
            || 42,
        );
        assert_eq!(r, 42);
        // Give a spuriously-firing watchdog time to prove itself before asserting it didn't.
        std::thread::sleep(Duration::from_millis(120));
        assert!(
            !fast_fired.load(Ordering::SeqCst),
            "watchdog fired even though f returned well inside the timeout"
        );

        let slow_fired = Arc::new(AtomicBool::new(false));
        let flag = slow_fired.clone();
        let observed = slow_fired.clone();
        // WAIT for the watchdog rather than racing it against a fixed sleep. This half used
        // to sleep 150 ms and assume the watchdog thread would be scheduled somewhere inside
        // that window. On a machine saturated by a release build that assumption is simply
        // untrue, and it is how this test went red on 2026-08-15 for a reason that had
        // nothing to do with the code under test. The pass before that widened the FAST half
        // against exactly this hazard and left this half racy, so the flake survived a fix
        // aimed at it.
        //
        // Blocking until the flag is set asks the question the contract actually makes, "does
        // the watchdog fire at all for an `f` that overruns", instead of the scheduling
        // question "did it fire inside 150 ms". The deadline is a backstop that only a
        // watchdog which never fires can reach, not a timing budget: a working watchdog
        // releases this loop after one 5 ms tick no matter how loaded the box is.
        //
        // Note on how this was verified, because the previous pass got this wrong: an
        // artificial load harness (96 CPU burners on 32 cores) could NOT reproduce the
        // original failure, passing 20/20 against the OLD racy shape as well as this one. So
        // "it passed N times under load" is not evidence here and was not treated as any. The
        // claim this fix rests on is structural: there is no longer a window to lose. Do not
        // re-tighten it into a fixed sleep on the strength of a green load run.
        let r = super::with_watchdog(
            Duration::from_millis(20),
            move || flag.store(true, Ordering::SeqCst),
            move || {
                let deadline = Instant::now() + Duration::from_secs(10);
                while !observed.load(Ordering::SeqCst) && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(5));
                }
                7
            },
        );
        assert_eq!(r, 7);
        assert!(
            slow_fired.load(Ordering::SeqCst),
            "watchdog never fired for an f that ran past the timeout"
        );
    }

    /// `frame_from_bytes` is now a thin `.to_vec()` + delegate over
    /// [`super::frame_from_owned_bytes`] (the A202 fix: callers that already own a `Vec<u8>`
    /// — every mp4/mkv/flv remux buffer, `mp4_remux_moov`'s output — call the owned entry
    /// point directly instead of paying for a second copy). Both entry points must still
    /// agree on the same input regardless of which one a caller reaches for; empty bytes can
    /// never produce a keyframe on any host, Media Foundation present or not, so this holds
    /// without needing the video corpus.
    #[test]
    fn frame_from_bytes_and_frame_from_owned_bytes_agree_on_the_same_input() {
        assert!(super::frame_from_bytes(&[]).is_none());
        assert!(super::frame_from_owned_bytes(Vec::new()).is_none());
    }
}

#[cfg(test)]
mod still_brand_tests {
    use super::is_video_magic;

    /// Build an `ftyp` box: size, "ftyp", major brand, minor version, compatible brands.
    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let size = 16 + 4 * compat.len();
        let mut v = (size as u32).to_be_bytes().to_vec();
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(major);
        v.extend_from_slice(&[0, 0, 0, 1]); // minor version
        for c in compat {
            v.extend_from_slice(*c);
        }
        v.resize(size.max(16), 0);
        v
    }

    /// A still image must NEVER be classified as video, and the shell is why this is not a
    /// cosmetic distinction: `streamsrc::stream_source` STOPS when something sniffs as video
    /// and then decodes no frame, rather than falling through to the image tiers. So one
    /// unrecognised image brand is a permanent stock icon in Explorer, while the CLI (which
    /// reads by path and never consults this) renders the same file perfectly.
    ///
    /// That shipped. libheif writes `mif3` as the MAJOR brand of an alpha AVIF; `mif3` was
    /// absent from the allowlist, and `sample-avif-alpha.avif` returned 0x8004B200 from
    /// `IShellItemImageFactory` while `st2k thumbnail` produced a perfect 256x256 tile.
    #[test]
    fn miaf_still_brands_are_never_mistaken_for_video() {
        // The exact regression: major brand mif3, no compatible brands at all.
        assert!(!is_video_magic(&ftyp(b"mif3", &[])));

        // The MIAF/HEIF/AVIF family, as major brands.
        for major in [
            b"heic", b"heix", b"heim", b"heis", b"mif1", b"mif2", b"mif3", b"msf1", b"miaf",
            b"MA1A", b"MA1B", b"avif", b"avio", b"avis", b"heif",
        ] {
            assert!(
                !is_video_magic(&ftyp(major, &[])),
                "{} must not sniff as video",
                String::from_utf8_lossy(major)
            );
        }

        // The robustness half: an UNKNOWN major brand still reads as a still when a known one
        // appears among the compatible brands. This is what stops the next exotic brand from
        // costing another silent Explorer regression.
        assert!(!is_video_magic(&ftyp(b"zzzz", &[b"mif1", b"avif"])));
        assert!(!is_video_magic(&ftyp(b"1234", &[b"miaf"])));

        // ...and real video is still video, both by major brand and with video-only compat.
        assert!(is_video_magic(&ftyp(b"isom", &[b"isom", b"iso2", b"mp41"])));
        assert!(is_video_magic(&ftyp(b"mp42", &[])));
        assert!(is_video_magic(&ftyp(b"qt  ", &[])));

        // Audio and Canon RAW keep their existing exclusions.
        assert!(!is_video_magic(&ftyp(b"M4A ", &[])));
        assert!(!is_video_magic(&ftyp(b"crx ", &[])));

        // A declared box size larger than the buffer must not panic or over-read.
        let mut lying = ftyp(b"zzzz", &[b"mif1"]);
        lying[0..4].copy_from_slice(&9999u32.to_be_bytes());
        assert!(!is_video_magic(&lying));
    }

    /// A head that STOPS INSIDE the ftyp box must not panic.
    ///
    /// The function's own entry guard is `len >= 12` (enough for `ftyp` + the major brand),
    /// but compatible brands start at offset 16 — so a 12..=15 byte head used to reach
    /// `box_end.clamp(16, head.len())`, which is `min > max` and panics by `clamp`'s
    /// contract. Not a soft failure: this parser runs in-process inside `explorer.exe`
    /// under `panic = "abort"`, so a 13-byte ftyp-shaped file or stream aborted the user's
    /// whole shell. Found by `fuzz::parsers_survive_mutation_of_synthetic_seeds`
    /// ("seed 'stub15' iter 0: min > max. min = 16, max = 13").
    ///
    /// Every prefix is swept, not just the guilty lengths, because the next edit to this
    /// function is as likely to move the boundary as to remove it.
    #[test]
    fn a_head_truncated_inside_the_ftyp_box_does_not_panic() {
        for major in [b"mp42", b"heic", b"zzzz"] {
            let full = ftyp(major, &[b"mif1", b"isom"]);
            for n in 0..=full.len() {
                let _ = is_video_magic(&full[..n]);
            }
        }

        // The exact fuzz seed: 13 bytes of an ISO-BMFF head, box size declaring more than
        // arrived. The major brand is all that's readable, and it must still be honoured —
        // skipping the compatible-brands scan may not silently reclassify the file.
        let heic13 = &ftyp(b"heic", &[b"mif1"])[..13];
        assert_eq!(heic13.len(), 13);
        assert!(
            !is_video_magic(heic13),
            "a short HEIC head is still a still"
        );
        assert!(
            is_video_magic(&ftyp(b"mp42", &[b"isom"])[..13]),
            "a short mp4 head is still video"
        );

        // The boundary either side of 16, where the clamp's min and max meet.
        for n in 12..=16 {
            assert!(!is_video_magic(&ftyp(b"avif", &[b"mif1"])[..n]));
            assert!(is_video_magic(&ftyp(b"isom", &[b"iso2"])[..n]));
        }
    }
}
