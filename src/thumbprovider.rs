//! The thumbnail provider: IThumbnailProvider + IInitializeWithStream.
//!
//! The shell hands us an IStream via `Initialize`; we stash it (methods take
//! `&self`, hence the `RefCell`) and decode it in `GetThumbnail`. Using
//! IInitializeWithStream is what lets the shell run us in its isolated
//! out-of-process host without `DisableProcessIsolation`.

use core::cell::RefCell;
use core::ffi::c_void;

use windows_implement::implement;
use windows::core::{Error, Ref, Result};
use windows::Win32::Foundation::{E_FAIL, E_POINTER};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::{
    IStream, STATFLAG_NONAME, STATSTG, STREAM_SEEK, STREAM_SEEK_CUR, STREAM_SEEK_END,
    STREAM_SEEK_SET,
};
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTS_ALPHATYPE, WTSAT_ARGB, WTSAT_UNKNOWN,
};

use crate::{decode, dib, safety, settings};

// The whole-file read ceiling, shared with the path-reading verbs via
// `decode::limits::MAX_INPUT_BYTES` (one DoS budget, not two copies).
const MAX_BYTES: usize = decode::limits::MAX_INPUT_BYTES as usize;

#[implement(IThumbnailProvider, IInitializeWithStream)]
pub struct ThumbnailProvider {
    _ref: crate::ModuleRef,
    stream: RefCell<Option<IStream>>,
}

impl Default for ThumbnailProvider {
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
    fn default() -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            stream: RefCell::new(None),
        }
    }
}

impl IInitializeWithStream_Impl for ThumbnailProvider_Impl {
    fn Initialize(&self, pstream: Ref<'_, IStream>, _grfmode: u32) -> Result<()> {
        safety::guard(|| {
            let stream = pstream.ok()?;
            // try_borrow_mut turns any (even theoretical) re-entrant borrow into an
            // HRESULT instead of a panic across the COM ABI.
            let mut slot = self.stream.try_borrow_mut().map_err(|_| Error::from(E_FAIL))?;
            *slot = Some(stream.clone());
            safety::log_debug("Initialize: stream stored");
            Ok(())
        })
    }
}

impl IThumbnailProvider_Impl for ThumbnailProvider_Impl {
    fn GetThumbnail(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        safety::guard(|| {
            let r = self.get_thumbnail_inner(cx, phbmp, pdwalpha);
            if let Err(e) = &r {
                // Leave a one-line breadcrumb so a failed thumbnail isn't
                // diagnostically silent even with Debug=1 (the shell swallows
                // the HRESULT and just falls back to the default icon).
                safety::log_debug(&format!("GetThumbnail: failed hr={:#010x}", e.code().0));
            }
            r
        })
    }
}

impl ThumbnailProvider_Impl {
    fn get_thumbnail_inner(
        &self,
        cx: u32,
        phbmp: *mut HBITMAP,
        pdwalpha: *mut WTS_ALPHATYPE,
    ) -> Result<()> {
        {
            // Reject null out-params up front (mirrors DllGetClassObject) so the
            // later writes are provably safe and no HBITMAP is allocated/leaked.
            if phbmp.is_null() || pdwalpha.is_null() {
                return Err(Error::from(E_POINTER));
            }
            unsafe {
                *phbmp = HBITMAP::default();
                *pdwalpha = WTSAT_UNKNOWN;
            }

            // One HKCU key open for ALL four settings this call needs (master
            // switch, size cap, thumb edge, embedded pref) instead of ~5 separate
            // opens — see `settings::thumb_settings`. Still a fresh read per request,
            // so Settings changes take effect immediately for the next thumbnail.
            let cfg = settings::thumb_settings();

            // Option: master switch. Returning a failure lets the shell fall
            // back to the file's default icon.
            if !cfg.enabled {
                safety::log_debug("GetThumbnail: disabled via EnableThumbs=0");
                return Err(Error::from(E_FAIL));
            }

            let bytes = {
                let borrow = self.stream.borrow();
                let stream = borrow.as_ref().ok_or_else(|| Error::from(E_FAIL))?;

                // Audio: the album art lives in the metadata, so we seek straight
                // to it and read ONLY the art (not the whole file). This both
                // sidesteps the size cap — a multi-GB audiobook still thumbnails —
                // and avoids buffering the file.
                match unsafe { audio_art(stream) } {
                    AudioArt::Art(art) => art,
                    // Audio with no usable art: stop here. The raw audio bytes are
                    // not a decodable image, so falling through to a whole-file read
                    // + decode would just waste time (up to the magick timeout) and
                    // fail anyway. Default icon, fast.
                    AudioArt::NoArt => {
                        safety::log_debug("GetThumbnail: audio file has no embedded art");
                        return Err(Error::from(E_FAIL));
                    }
                    AudioArt::NotAudio => {
                        // Video: stream a representative frame straight off the IStream via
                        // Media Foundation (a multi-GB movie never lands in memory). If it
                        // IS video but the OS lacks the codec, stop here (default icon) like
                        // the audio-no-art case, rather than buffering the whole file only
                        // to fail decoding it as an image.
                        if unsafe { peek_is_video(stream) } {
                            return match crate::video::frame_from_istream(stream) {
                                Some(frame) => {
                                    let decoded = decode::thumbnail_from_image(frame, cx.min(cfg.max_thumb));
                                    let hbmp = unsafe {
                                        dib::create_premultiplied_dib(
                                            decoded.width as i32,
                                            decoded.height as i32,
                                            &decoded.rgba,
                                        )?
                                    };
                                    unsafe {
                                        *phbmp = hbmp;
                                        *pdwalpha = WTSAT_ARGB;
                                    }
                                    safety::log_debug(&format!(
                                        "GetThumbnail: video frame {}x{}",
                                        decoded.width, decoded.height
                                    ));
                                    Ok(())
                                }
                                None => {
                                    safety::log_debug("GetThumbnail: video with no decodable frame");
                                    Err(Error::from(E_FAIL))
                                }
                            };
                        }
                        // Everything else: skip oversized files cheaply via the stream
                        // length before reading into memory. The effective cap is the
                        // user's MaxSize but never above the hard MAX_BYTES ceiling
                        // ("0 = unlimited" means "up to MAX_BYTES").
                        let max = cfg.max_file_bytes.min(MAX_BYTES as u64);
                        let size = unsafe { stream_size(stream) };
                        match size {
                            // Oversized: the whole-file read is a DoS risk, so we skip it —
                            // EXCEPT a giant comic ARCHIVE (CBZ/CB7), which we stream: read
                            // only its central directory + one cover entry over the IStream,
                            // never buffering the whole archive. (CBR can't — `rars` needs the
                            // full buffer — so a huge .cbr still gets the default icon.)
                            Some(size) if size > max => {
                                match unsafe { archive_cover_streamed(stream) } {
                                    Some(cover) => {
                                        safety::log_debug(&format!(
                                            "GetThumbnail: streamed cover from {size}-byte archive"
                                        ));
                                        cover
                                    }
                                    None => {
                                        safety::log_debug(&format!(
                                            "GetThumbnail: skip, {size} bytes over limit"
                                        ));
                                        return Err(Error::from(E_FAIL));
                                    }
                                }
                            }
                            _ => unsafe { read_all(stream, MAX_BYTES, size)? },
                        }
                    }
                }
            };

            // Option: cap the generated edge at the user's max (default 256,
            // clamped to the legacy [32, 512] range). decode never upscales.
            let cx = cx.min(cfg.max_thumb);

            safety::log_debug(&format!("GetThumbnail: cx={cx} bytes={}", bytes.len()));
            let img = decode::decode_thumbnail_opts(&bytes, cx, cfg.use_embedded)?;
            safety::log_debug(&format!("GetThumbnail: decoded {}x{}", img.width, img.height));
            let hbmp =
                unsafe { dib::create_premultiplied_dib(img.width as i32, img.height as i32, &img.rgba)? };

            unsafe {
                *phbmp = hbmp;
                *pdwalpha = WTSAT_ARGB;
            }
            Ok(())
        }
    }
}

/// The stream's total size in bytes via `IStream::Stat`, or None if the stream
/// doesn't support it (then we just read up to the hard `MAX_BYTES` cap).
unsafe fn stream_size(stream: &IStream) -> Option<u64> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
    Some(stat.cbSize)
}

/// Sniff the stream head for a video container we can frame-grab (Matroska/WebM, MP4/MOV,
/// AVI, ASF/WMV, …). Rewinds to 0 either way so the subsequent MF / whole-file read starts
/// clean. HEIC/AVIF and M4A/M4B share MP4's `ftyp` box but are excluded by `is_video_magic`.
unsafe fn peek_is_video(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 16];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, head.len() as u32, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    hr.is_ok() && crate::video::is_video_magic(&head[..got])
}

/// For an OVERSIZED file (past the in-memory cap), sniff whether it's a streamable
/// comic archive (CBZ/ZIP/CB7) and, if so, pull just the cover over the IStream — the
/// central directory + one entry, never the whole archive. Returns None for anything
/// else (incl. CBR, which `rars` can't read without a full buffer), so the caller
/// skips it. Rewinds the stream to 0 before handing it to the archive reader.
unsafe fn archive_cover_streamed(stream: &IStream) -> Option<Vec<u8>> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 8];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, head.len() as u32, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    if hr.is_err() || got < head.len() {
        return None;
    }
    crate::container::archive_cover_seek(IStreamReader { stream: stream.clone() }, &head[..got])
}

/// Result of the audio-art probe. The three cases are distinct so the caller can
/// tell "this isn't audio" (take the normal whole-file path) from "this IS audio
/// but carries no usable art" (stop — the raw audio bytes are not a decodable
/// image, so a full read + decode would just burn time and fail).
enum AudioArt {
    NotAudio,
    NoArt,
    Art(Vec<u8>),
}

/// Sniff the stream for audio and, if so, extract only the embedded art via a
/// seek-only read (lofty seeks to the metadata — we never buffer the whole file,
/// so even a multi-GB audiobook thumbnails). Rewinds the stream to 0 either way.
unsafe fn audio_art(stream: &IStream) -> AudioArt {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 16];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, head.len() as u32, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    // Never trust the IStream-reported count past the buffer it filled.
    let got = (got as usize).min(head.len());
    if hr.is_err() || got < 12 || !crate::container::looks_like_audio(&head[..got]) {
        return AudioArt::NotAudio;
    }
    match crate::container::audio_art_from_reader(IStreamReader { stream: stream.clone() }) {
        Some(art) => AudioArt::Art(art),
        None => AudioArt::NoArt,
    }
}

/// `std::io` Read + Seek over a COM IStream, so lofty can parse tags by seeking
/// instead of us draining the file into memory.
struct IStreamReader {
    stream: IStream,
}

impl std::io::Read for IStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut got: u32 = 0;
        unsafe { self.stream.Read(buf.as_mut_ptr() as *mut c_void, buf.len() as u32, Some(&mut got)) }
            .ok()
            .map_err(std::io::Error::other)?;
        // Never trust the IStream-reported count past the buffer it filled (the
        // sibling reads at `audio_art`/`read_all` clamp the same way) — returning
        // more than `buf.len()` violates the `Read` contract on a hostile stream.
        Ok((got as usize).min(buf.len()))
    }
}

impl std::io::Seek for IStreamReader {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        let (origin, off): (STREAM_SEEK, i64) = match pos {
            std::io::SeekFrom::Start(n) => (STREAM_SEEK_SET, n as i64),
            std::io::SeekFrom::Current(n) => (STREAM_SEEK_CUR, n),
            std::io::SeekFrom::End(n) => (STREAM_SEEK_END, n),
        };
        let mut newpos: u64 = 0;
        unsafe { self.stream.Seek(off, origin, Some(&mut newpos)) }.map_err(std::io::Error::other)?;
        Ok(newpos)
    }
}

/// Drain an IStream into a Vec, bounded by `max`.
unsafe fn read_all(stream: &IStream, max: usize, size_hint: Option<u64>) -> Result<Vec<u8>> {
    // Pre-size from the (already size-checked) stream length to skip the doubling
    // realloc churn on multi-MB images. Cap the upfront reservation so a stream that
    // lies about its size can't trick us into a giant allocation — the growth loop +
    // the `max` check below still bound the true read.
    let cap = size_hint.map_or(0, |h| (h as usize).min(max).min(64 << 20));
    let mut out: Vec<u8> = Vec::with_capacity(cap);
    let mut chunk = vec![0u8; 1 << 16];
    loop {
        let mut got: u32 = 0;
        let hr = stream.Read(
            chunk.as_mut_ptr() as *mut c_void,
            chunk.len() as u32,
            Some(&mut got),
        );
        // S_OK and S_FALSE are both successes; a failing HRESULT is a real transport
        // error (network/cloud-placeholder stream), NOT end-of-stream — don't mistake
        // it for EOF and silently feed a truncated buffer to the decoder.
        hr.ok()?;
        if got == 0 {
            break; // success + 0 bytes == genuine EOF
        }
        let n = (got as usize).min(chunk.len()); // never trust got > buffer
        out.extend_from_slice(&chunk[..n]);
        if out.len() > max {
            return Err(Error::from(E_FAIL));
        }
    }
    Ok(out)
}
