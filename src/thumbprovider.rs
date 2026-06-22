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
    CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATFLAG_NONAME, STATSTG, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
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

                // VIDEO FIRST — an MP4/MOV/M4V video shares the ISO-BMFF `ftyp` box with
                // M4A/M4B audio, so the audio probe below would otherwise claim it and,
                // finding no cover art, bail before the frame-grab ever ran (every .mp4
                // got a blank icon, then the shell cached that failure forever). The
                // `is_video_magic` sniff keys off the container brand, so it tells a video
                // file from audio / HEIC. Stream one representative frame straight off the
                // IStream via Media Foundation — a multi-GB movie never lands in memory. If
                // it IS video but the OS can't decode it, stop here (default icon) rather
                // than buffering the whole file only to fail decoding it as an image.
                if unsafe { peek_is_video(stream) } {
                    // Decode by FILE PATH when we can recover it: Media Foundation reading a
                    // multi-GB movie through the shell's thumbnail IStream is catastrophically
                    // slow (30 s+, a pegged core, past Explorer's timeout → the folder never
                    // thumbnails), while opening the file directly is <1 s. Fall back to the
                    // stream only for the rare item with no real on-disk path.
                    // We decode video IN MEMORY off a bounded read, NEVER streaming the
                    // multi-GB original through the shell's thumbnail IStream (MF's random
                    // access on it pegs a core for 30 s+, past Explorer's timeout → the
                    // whole folder goes "angry"). Tiers, each fast or a fast miss:
                    //   1. by file path, if the host exposes one (non-sandboxed callers) —
                    //      MF seeks the real file to the true 30% representative frame;
                    //   2. SMART TARGETED READ (MP4/MOV): parse the moov index, build a tiny
                    //      one-keyframe MP4 for the sync sample nearest ~30%, decode that —
                    //      single-digit MB (index + one keyframe), a representative frame, and
                    //      it works regardless of moov position (faststart or moov-at-end);
                    //   3. SMART TARGETED READ (Matroska/WebM): the EBML analog — read the Cues
                    //      index, build a tiny one-cluster MKV for the keyframe nearest ~30%;
                    //   4. GENERAL targeted read (AVI/WMV/… + any unmapped MP4/MKV): let MF's own
                    //      demuxer seek the real index to ~30% over a block-caching IStream that
                    //      coalesces its reads (no per-format parser, any container MF decodes);
                    //   5. a faststart MP4 / small / unindexed video decodes from its head prefix;
                    //   6. a big *non*-faststart MP4 (moov at the very end) is remuxed —
                    //      head frames + tail moov stitched into a small valid MP4.
                    // Tiers 5–6 stay as fallbacks for anything tier 4's demuxer can't seek.
                    let frame = unsafe { stream_path(stream) }
                        .and_then(|p| crate::video::frame_from_path(&p))
                        .or_else(|| {
                            crate::mp4::keyframe_mini_mp4(
                                &mut IStreamReader { stream: stream.clone() },
                                0.30,
                            )
                            .and_then(|buf| crate::video::frame_from_bytes(&buf))
                        })
                        .or_else(|| {
                            crate::mkv::keyframe_mini_mkv(
                                &mut IStreamReader { stream: stream.clone() },
                                0.30,
                            )
                            .and_then(|buf| crate::video::frame_from_bytes(&buf))
                        })
                        .or_else(|| {
                            // MF demuxes AVI/WMV/etc. directly; the block-caching stream makes its
                            // seek-to-30% reads cheap (the old shell-IStream meltdown was thousands
                            // of tiny marshaled reads — here they coalesce into a few big ones).
                            unsafe { stream_size(stream) }.and_then(|size| {
                                crate::video::frame_from_block_stream(stream, size, 0.30)
                            })
                        })
                        .or_else(|| {
                            unsafe { video_prefix(stream) }
                                .and_then(|buf| crate::video::frame_from_bytes(&buf))
                        })
                        .or_else(|| {
                            unsafe { mp4_remux_moov(stream) }
                                .and_then(|buf| crate::video::frame_from_bytes(&buf))
                        });
                    return match frame {
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
                        // Not audio, not video: skip oversized files cheaply via the stream
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

/// Recover the backing file path from the shell's thumbnail `IStream` via `IStream::Stat`
/// (`STATFLAG_DEFAULT` fills `pwcsName` — file-backed shell streams report the full path).
/// Returned only when it names an existing file, so a stream with no / non-file name simply
/// falls back to streaming. `pwcsName` is a CoTaskMem allocation we own and must free.
unsafe fn stream_path(stream: &IStream) -> Option<String> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_DEFAULT).ok()?;
    if stat.pwcsName.is_null() {
        return None;
    }
    let s = stat.pwcsName.to_string().ok();
    CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void));
    let s = s?;
    if std::path::Path::new(&s).is_file() {
        Some(s)
    } else {
        None
    }
}

/// Read up to a bounded PREFIX off the stream head in big sequential gulps, for the
/// in-memory video decode. A *faststart* MP4 keeps its `moov` index + first seconds of
/// frames here, so Media Foundation can seek/decode freely in RAM — sidestepping the
/// catastrophically slow random access (and marshaled per-read overhead) MF otherwise
/// suffers reading the multi-GB original through the shell's thumbnail `IStream`. Returns
/// None for a too-short read; a non-faststart file (moov at the end) simply won't decode
/// from the prefix and the caller falls back. Rewinds the stream to 0 afterwards.
unsafe fn video_prefix(stream: &IStream) -> Option<Vec<u8>> {
    const PREFIX: usize = 64 * 1024 * 1024;
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let cap = stream_size(stream).map_or(PREFIX, |sz| (sz as usize).min(PREFIX));
    let mut buf = vec![0u8; cap];
    let mut filled = 0usize;
    while filled < cap {
        let mut got: u32 = 0;
        let hr = stream.Read(
            buf[filled..].as_mut_ptr() as *mut c_void,
            (cap - filled) as u32,
            Some(&mut got),
        );
        if hr.is_err() || got == 0 {
            break;
        }
        filled += got as usize;
    }
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    buf.truncate(filled);
    (filled >= 64).then_some(buf)
}

/// Read exactly `buf.len()` bytes starting at the stream's current position (looping over
/// short reads). None if the stream ends early.
unsafe fn read_full(stream: &IStream, buf: &mut [u8]) -> Option<()> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let mut got: u32 = 0;
        let want = (buf.len() - filled).min(u32::MAX as usize) as u32;
        let hr = stream.Read(buf[filled..].as_mut_ptr() as *mut c_void, want, Some(&mut got));
        if hr.is_err() || got == 0 {
            break;
        }
        filled += got as usize;
    }
    (filled == buf.len()).then_some(())
}

/// Remux a big *non-faststart* MP4 (`moov` at the very end, past the prefix) into a small
/// in-memory MP4 MF can decode an early frame from. We do the I/O ourselves in a few big
/// seeks/reads (NOT MF's slow random access through the shell IStream): keep the file head
/// (ftyp + mdat header + the first frames of mdat) verbatim, rewrite mdat's box size so it
/// ends where we append the real `moov` pulled from the tail. The moov's sample offsets are
/// absolute and point into the early mdat we kept byte-for-byte, so they still resolve;
/// only the early keyframe (≤ our 3 s seek) needs to live within the retained head. Returns
/// None unless this really is a moov-after-mdat MP4 within sane bounds.
unsafe fn mp4_remux_moov(stream: &IStream) -> Option<Vec<u8>> {
    // Early mdat retained — must reach the frame we grab. mp4 mdat interleaving isn't
    // always video-first: a real 24-min/14 GB sample put its first video chunk ~58 MB in,
    // so the ~3 s seek frame landed ~86 MB in. 128 MB covers that with margin; a file that
    // buries video even deeper just fast-fails to the default icon (no hang).
    const HEAD_KEEP: u64 = 128 * 1024 * 1024;
    const MOOV_MAX: u64 = 96 * 1024 * 1024; // sanity cap on the tail moov we'll pull

    let total = stream_size(stream)?;
    // Walk top-level boxes to find mdat (offset + header length) and moov (offset + size).
    let mut pos: u64 = 0;
    let mut mdat: Option<(u64, u64)> = None; // (offset, header_len)
    let mut moov: Option<(u64, u64)> = None; // (offset, full_size)
    while pos + 8 <= total {
        if stream.Seek(pos as i64, STREAM_SEEK_SET, None).is_err() {
            return None;
        }
        let mut hdr = [0u8; 16];
        let mut got: u32 = 0;
        if stream
            .Read(hdr.as_mut_ptr() as *mut c_void, 16, Some(&mut got))
            .is_err()
        {
            return None;
        }
        if (got as usize) < 8 {
            break;
        }
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
        let (full, hlen) = if size32 == 1 {
            if (got as usize) < 16 {
                break;
            }
            (u64::from_be_bytes(hdr[8..16].try_into().ok()?), 16u64)
        } else if size32 == 0 {
            (total - pos, 8) // extends to EOF
        } else {
            (size32, 8)
        };
        if full < hlen {
            break;
        }
        match &hdr[4..8] {
            b"mdat" => mdat = Some((pos, hlen)),
            b"moov" => {
                moov = Some((pos, full));
                break;
            }
            _ => {}
        }
        pos = pos.checked_add(full)?;
    }

    let (mdat_off, mdat_hlen) = mdat?;
    let (moov_off, moov_size) = moov?;
    // Only worth it for moov-AFTER-mdat (faststart is already handled by the prefix path).
    if moov_off <= mdat_off || moov_size == 0 || moov_size > MOOV_MAX {
        return None;
    }

    // Retain ftyp + mdat header + early mdat, ending before the moov.
    let keep = HEAD_KEEP.min(moov_off).min(total);
    if keep <= mdat_off + mdat_hlen {
        return None;
    }
    let mut head = vec![0u8; keep as usize];
    if stream.Seek(0, STREAM_SEEK_SET, None).is_err() {
        return None;
    }
    read_full(stream, &mut head)?;

    // Rewrite mdat's size so the box ends exactly at `keep` (data offset is unchanged).
    let new_mdat = keep - mdat_off;
    let o = mdat_off as usize;
    if mdat_hlen == 16 {
        head[o + 8..o + 16].copy_from_slice(&new_mdat.to_be_bytes());
    } else {
        head[o..o + 4].copy_from_slice(&(new_mdat as u32).to_be_bytes());
    }

    // Pull the moov from the tail (one seek + bulk read) and append it.
    let mut moov_buf = vec![0u8; moov_size as usize];
    if stream.Seek(moov_off as i64, STREAM_SEEK_SET, None).is_err() {
        return None;
    }
    read_full(stream, &mut moov_buf)?;
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);

    head.extend_from_slice(&moov_buf);
    Some(head)
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
