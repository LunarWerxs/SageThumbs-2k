//! Shared shell-`IStream` source acquisition for the thumbnail + preview handlers.
//!
//! Both handlers receive the same kind of shell `IStream` and need the same
//! "get me something decodable WITHOUT buffering a multi-GB file" cascade:
//! video frame-grab tiers, seek-only audio album art, streamed archive covers,
//! the head-preview prefix rescue, and the bounded whole-file read. This module
//! owns that cascade ([`stream_source`]) plus the low-level `IStream` helpers
//! it is built from, so the two handlers can't drift apart. Everything here
//! runs on the CALLING (COM apartment) thread — the marshaled stream is
//! apartment-bound and must not be touched from a worker.

use core::ffi::c_void;

use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Com::{
    CoTaskMemFree, IStream, STATFLAG_DEFAULT, STATFLAG_NONAME, STATSTG, STREAM_SEEK,
    STREAM_SEEK_CUR, STREAM_SEEK_END, STREAM_SEEK_SET,
};

use crate::{decode, safety};

// The whole-file read ceiling, shared with the path-reading verbs via
// `decode::limits::MAX_INPUT_BYTES` (one DoS budget, not two copies).
const MAX_BYTES: usize = decode::limits::MAX_INPUT_BYTES as usize;

/// What [`stream_source`] hands back: either a video frame Media Foundation
/// already decoded (no bytes to re-decode), or bounded raw bytes for the
/// caller's tiered byte decoder.
pub enum StreamSource {
    Frame(image::DynamicImage),
    Bytes(Vec<u8>),
}

/// Turn the shell's `IStream` into a decodable source without ever buffering an
/// unbounded file. `who` prefixes the debug-log lines ("GetThumbnail" /
/// "DoPreview"); `max_file_bytes` is the user's MaxSize cap (the streaming tiers
/// deliberately sidestep it — a multi-GB video/audiobook/archive still previews).
///
/// The cascade, in order:
/// 1. VIDEO — an MP4/MOV/M4V video shares the ISO-BMFF `ftyp` box with M4A/M4B
///    audio, so the audio probe below would otherwise claim it and, finding no
///    cover art, bail before the frame-grab ever ran (every .mp4 got a blank
///    icon, then the shell cached that failure forever). The `is_video_magic`
///    sniff keys off the container brand, so it tells a video file from audio /
///    HEIC. If it IS video but no tier decodes a frame, stop (default icon /
///    blank pane) rather than buffering the whole file only to fail decoding it
///    as an image — EXCEPT ambiguous OggS, which falls through to the audio path.
/// 2. AUDIO — the album art lives in the metadata, so seek straight to it and
///    read ONLY the art (not the whole file). Sidesteps the size cap AND avoids
///    buffering; artless audio stops here (raw audio bytes are not a decodable
///    image, a full read + decode would just burn time and fail).
/// 3. OVERSIZED (past the cap) — streamed container cover (CBZ/CB7 central
///    directory + one entry; Clip Studio `.clip` tail database) or the
///    head-preview prefix rescue (.blend/PSD-PSB baked previews sit in the
///    first bytes); otherwise skip.
/// 4. Everything else: bounded whole-file read.
pub unsafe fn stream_source(
    stream: &IStream,
    max_file_bytes: u64,
    who: &str,
) -> Result<StreamSource> {
    if peek_is_video(stream) {
        // Decode by FILE PATH when we can recover it: Media Foundation reading a
        // multi-GB movie through the shell's IStream is catastrophically slow
        // (30 s+, a pegged core, past Explorer's timeout), while opening the file
        // directly is <1 s. We otherwise decode video IN MEMORY off a bounded
        // read, NEVER streaming the multi-GB original through the shell IStream.
        // Tiers, each fast or a fast miss:
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
        let frame = stream_path(stream)
            .and_then(|p| crate::video::frame_from_path(&p))
            .or_else(|| {
                crate::mp4::keyframe_mini_mp4(&mut IStreamReader { stream: stream.clone() }, 0.30)
                    .and_then(|buf| crate::video::frame_from_bytes(&buf))
            })
            .or_else(|| {
                crate::mkv::keyframe_mini_mkv(&mut IStreamReader { stream: stream.clone() }, 0.30)
                    .and_then(|buf| crate::video::frame_from_bytes(&buf))
            })
            .or_else(|| {
                // MF demuxes AVI/WMV/etc. directly; the block-caching stream makes its
                // seek-to-30% reads cheap (the old shell-IStream meltdown was thousands
                // of tiny marshaled reads — here they coalesce into a few big ones).
                stream_size(stream)
                    .and_then(|size| crate::video::frame_from_block_stream(stream, size, 0.30))
            })
            .or_else(|| video_prefix(stream).and_then(|buf| crate::video::frame_from_bytes(&buf)))
            .or_else(|| mp4_remux_moov(stream).and_then(|buf| crate::video::frame_from_bytes(&buf)));
        if let Some(frame) = frame {
            safety::log_debug(&format!(
                "{who}: video frame {}x{}",
                frame.width(),
                frame.height()
            ));
            return Ok(StreamSource::Frame(frame));
        }
        // No decodable frame. OggS is ambiguous — an audio-only .ogg/.opus matches
        // the video magic too, so fall THROUGH to the album-art path below instead of
        // failing. A genuine video container the OS can't decode stops here.
        if !peek_is_ogg(stream) {
            safety::log_debug(&format!("{who}: video with no decodable frame"));
            return Err(Error::from(E_FAIL));
        }
        safety::log_debug(&format!("{who}: OggS not video — trying album art"));
    }

    match audio_art(stream) {
        AudioArt::Art(art) => return Ok(StreamSource::Bytes(art)),
        AudioArt::NoArt => {
            safety::log_debug(&format!("{who}: audio file has no embedded art"));
            return Err(Error::from(E_FAIL));
        }
        AudioArt::NotAudio => {}
    }

    // Not audio, not video: skip oversized files cheaply via the stream length
    // before reading into memory. The effective cap is the user's MaxSize but
    // never above the hard MAX_BYTES ceiling ("0 = unlimited" means "up to
    // MAX_BYTES").
    let max = max_file_bytes.min(MAX_BYTES as u64);
    let size = stream_size(stream);
    match size {
        // Oversized: the whole-file read is a DoS risk, so we skip it —
        // EXCEPT a seek-streamable container: a giant comic ARCHIVE
        // (CBZ/CB7) reads only its central directory + one cover entry
        // over the IStream, and a Clip Studio .clip seeks to the SQLite
        // database at its tail and reads only that. (CBR can't — `rars`
        // needs the full buffer — so a huge .cbr still gets the default
        // icon.) Head-preview containers (.blend / PSD-PSB) get a second
        // rescue: their baked thumbnail sits in the first bytes, so a
        // bounded prefix read suffices no matter the file size (issue #1).
        Some(size) if size > max => {
            if let Some(cover) = archive_cover_streamed(stream) {
                safety::log_debug(&format!("{who}: streamed cover from {size}-byte archive"));
                return Ok(StreamSource::Bytes(cover));
            }
            if let Some(prefix) = head_preview_prefix(stream) {
                safety::log_debug(&format!(
                    "{who}: head-preview prefix ({} bytes) of {size}-byte file",
                    prefix.len()
                ));
                return Ok(StreamSource::Bytes(prefix));
            }
            safety::log_debug(&format!("{who}: skip, {size} bytes over limit"));
            Err(Error::from(E_FAIL))
        }
        _ => {
            let _ = stream.Seek(0, STREAM_SEEK_SET, None);
            Ok(StreamSource::Bytes(read_all(stream, MAX_BYTES, size)?))
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
    // 208 bytes: enough to verify the MPEG-TS / M2TS sync-byte STRIDE (a second 0x47 at
    // offset 188 / 196) so we don't false-match any file that merely starts with 'G'.
    let mut head = [0u8; 208];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, head.len() as u32, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    hr.is_ok() && crate::video::is_video_magic(&head[..got])
}

/// Is the stream head the Ogg container magic (`OggS`)? Ogg carries both video (.ogv) and
/// audio (Vorbis/Opus/Speex), so a video frame-grab miss on an Ogg means it's audio-only —
/// the caller then falls back to the album-art path instead of failing.
unsafe fn peek_is_ogg(stream: &IStream) -> bool {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 4];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, 4, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    hr.is_ok() && got == 4 && &head == b"OggS"
}

/// Recover the backing file path from the shell's `IStream` via `IStream::Stat`
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
/// suffers reading the multi-GB original through the shell's `IStream`. Returns
/// None for a too-short read; a non-faststart file (moov at the end) simply won't decode
/// from the prefix and the caller falls back. Rewinds the stream to 0 afterwards.
unsafe fn video_prefix(stream: &IStream) -> Option<Vec<u8>> {
    const PREFIX: usize = 64 * 1024 * 1024;
    stream_prefix(stream, PREFIX)
}

/// Read up to `max` bytes off the stream head in big sequential gulps, rewinding to 0
/// before and after. Shared by the video-prefix decode and the head-preview rescue —
/// the bounded read is the same, only the cap differs.
unsafe fn stream_prefix(stream: &IStream, max: usize) -> Option<Vec<u8>> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let cap = stream_size(stream).map_or(max, |sz| (sz as usize).min(max));
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

/// For an OVERSIZED file (past the in-memory cap): if its magic marks a container
/// whose baked preview lives in the head — Blender `.blend` (`TEST` block ~100 bytes
/// in) or Photoshop PSD/PSB (image resource 1036 just past the header) — read a
/// bounded [`decode::HEAD_PREVIEW_BYTES`] prefix and thumbnail from THAT, instead of
/// skipping to the default icon. Big Blender scenes and PSBs routinely exceed the
/// 100 MB default cap while their thumbnails sit in the first kilobytes (GitHub
/// issue #1). Every container extractor is bounds-checked, so a truncated tail just
/// means "no preview found" (default icon — same as before), never a mis-decode.
unsafe fn head_preview_prefix(stream: &IStream) -> Option<Vec<u8>> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 8];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, head.len() as u32, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let got = (got as usize).min(head.len());
    if hr.is_err() || got < head.len() || !crate::container::has_head_preview(&head) {
        return None;
    }
    stream_prefix(stream, decode::HEAD_PREVIEW_BYTES)
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

/// For an OVERSIZED file (past the in-memory cap), sniff whether it's a seek-
/// streamable container — a comic archive (CBZ/ZIP/CB7: central directory + one
/// cover entry) or a Clip Studio `.clip` (the tail SQLite database holding the
/// canvas preview) — and, if so, pull just the cover over the IStream, never the
/// whole file. Returns None for anything else (incl. CBR, which `rars` can't read
/// without a full buffer), so the caller skips it. Rewinds the stream to 0 before
/// handing it to the container reader.
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
