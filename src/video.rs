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

use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
mod strands;
use strands::*;
mod blockstream;
mod nv12;
pub(crate) use blockstream::global_interface_table;
#[cfg(test)]
use blockstream::*;
pub use blockstream::{frame_from_block_stream, frame_from_block_stream_file};
pub use nv12::{
    frame_from_bytes_repr, frame_from_owned_bytes, nv12_frame_from_owned_bytes, Nv12Frame,
};
pub use strands::{
    mf_grab_attempts, mf_usable, mf_wedged, oldest_strand_age, stranded_workers, STRAND_GRACE,
};

use image::{DynamicImage, RgbaImage};
use windows::core::{Interface, GUID, HSTRING, PCWSTR};
use windows::Win32::Foundation::{HANDLE, RPC_S_CALLPENDING, WAIT_OBJECT_0};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::StructuredStorage::{PropVariantToUInt64, PROPVARIANT};
use windows::Win32::System::Com::{
    CoCreateInstance, CoWaitForMultipleHandles, IGlobalInterfaceTable, IStream,
    CLSCTX_INPROC_SERVER, COWAIT_DEFAULT, COWAIT_DISPATCH_CALLS,
};
use windows::Win32::System::Threading::{CreateEventW, SetEvent, WaitForSingleObject};
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
    // Test/diagnostic escape hatch, the twin of `ST2K_NO_MAGICK` in `decode::magick`:
    // `ST2K_NO_MF=1` makes this process behave like a machine with no Media Foundation at
    // all, so a gate can measure what OUR OWN decoders answer for a file the OS would
    // otherwise have handled. That is not a hypothetical difference — MPEG-2 in a program
    // or transport stream is decoded by Media Foundation only when the Store "MPEG-2 Video
    // Extension" is installed, so a developer box with it silently hides whether our tier
    // works at all. Deliberately read on EVERY call rather than cached beside the probe
    // below: a test that flips it mid-process has to be able to change the answer.
    if std::env::var_os("ST2K_NO_MF").is_some_and(|v| v == "1") {
        return false;
    }
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
            if head[16..end].as_chunks::<4>().0.iter().any(|b| is_still(b)) {
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
    static_video_magic(head)
}

/// The fixed-magic container checks (MPEG-TS sync at the 188/192 stride, then the EBML/RIFF-AVI/ASF/FLV/MPEG/Ogg signatures) applied after `ftyp` declines.
fn static_video_magic(head: &[u8]) -> bool {
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
    // structured exception under `panic = "abort"`. See `media_foundation_available`, and
    // `mf_usable` for the wedged-host half of the gate (issue #35).
    if !mf_usable() {
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
/// thread in a disposable host is acceptable — same trade as `decode_svg` / `pdf`), but it
/// is recorded as a [`Strand`] so a worker that never finishes is not invisible: past
/// [`STRAND_GRACE`] it marks this host wedged ([`mf_wedged`]).
fn grab_budgeted<T, F>(f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce() -> Option<T> + Send + 'static,
{
    MF_GRAB_ATTEMPTS.fetch_add(1, Ordering::SeqCst);
    let (tx, rx) = std::sync::mpsc::channel();
    let done = Arc::new(AtomicBool::new(false));
    let finished = done.clone();
    std::thread::spawn(move || {
        // Pin the DLL for this detached worker's whole lifetime: on timeout we return but
        // leave it running, and `DllCanUnloadNow` ignores it, so the thumbnail host could
        // unload the DLL mid-grab and crash. Mirrors run_action_detached.
        let r = crate::pdf::with_mta_apartment(f);
        let _ = tx.send(r);
        // Last act, after the apartment is gone: "done" means done with Media Foundation.
        finished.store(true, Ordering::SeqCst);
    });
    match rx.recv_timeout(VIDEO_TIMEOUT) {
        Ok(r) => r,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            note_strand(done, "an in-memory frame grab");
            None
        }
        // The worker is gone (it can only get here by unwinding, which the shell build
        // turns into an abort anyway): nothing is stranded, there is just no frame.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => None,
    }
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
    scan_for_frame(reader, first_video)
}

/// Read samples (skipping ticks/format changes, keeping the first black frame as fallback) until a non-black frame or the read/decode bounds.
unsafe fn scan_for_frame(reader: &IMFSourceReader, first_video: u32) -> Option<DynamicImage> {
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
        .as_chunks::<4>()
        .0
        .iter()
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
mod still_brand_tests;
#[cfg(test)]
mod tests;
