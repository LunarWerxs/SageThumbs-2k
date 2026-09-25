//! The shell's IStream, read safely: the cached head, stat / path / extension, bounded prefix reads, and a Read + Seek adapter.

use super::*;

/// One head read and one `Stat` per stream, taken at the top of the cascade and handed to
/// every probe. Before this each probe seeked, read and rewound its own copy of the same
/// first bytes and called `Stat` again for the same size and name.
pub struct StreamHead {
    /// The first bytes of the stream: at most [`HEAD_BYTES`], fewer for a shorter stream
    /// (or none when the stream refuses the read).
    pub(super) bytes: Vec<u8>,
    /// `STATSTG::cbSize`, when the stream reports one.
    pub(super) size: Option<u64>,
    /// Lowercased extension of `STATSTG::pwcsName`, when the stream reports a name.
    pub(super) ext: Option<String>,
}

impl StreamHead {
    pub(super) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The first `n` head bytes, or all of them when the head is shorter than `n`.
    pub(super) fn first(&self, n: usize) -> &[u8] {
        self.bytes.get(..n).unwrap_or(self.bytes.as_slice())
    }

    /// Does the extension the stream reported satisfy `pred`? False for a name-less
    /// stream.
    pub(super) fn extension_is(&self, pred: impl FnOnce(&str) -> bool) -> bool {
        self.ext.as_deref().is_some_and(pred)
    }

    /// A video container we can frame-grab (Matroska/WebM, MP4/MOV, AVI, ASF/WMV, ...).
    /// HEIC/AVIF and M4A/M4B share MP4's `ftyp` box but are excluded by `is_video_magic`.
    pub(super) fn is_video(&self) -> bool {
        crate::video::is_video_magic(&self.bytes)
    }

    /// The OpenEXR magic.
    pub(super) fn is_exr(&self) -> bool {
        self.bytes.len() >= 4 && decode::is_exr_magic(self.first(4))
    }

    /// The Ogg container magic (`OggS`). Ogg carries both video (.ogv) and audio
    /// (Vorbis/Opus/Speex), so a video frame-grab miss on an Ogg means it is audio-only
    /// and the caller falls back to the album-art path instead of failing.
    pub(super) fn is_ogg(&self) -> bool {
        self.bytes.starts_with(b"OggS")
    }

    /// The ASF header GUID. ASF carries Windows Media video (.wmv) and audio (.wma) alike, so
    /// a frame-grab miss on one is most often a WMA, whose cover lives in its tags.
    pub(super) fn is_asf(&self) -> bool {
        self.bytes
            .starts_with(&[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11])
    }

    /// The 7z signature. Used only for the unknown-size fail-closed gate; ZIP remains
    /// eligible for its deliberate seek-only CBZ cover rescue.
    pub(super) fn is_7z(&self) -> bool {
        self.bytes
            .starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C])
    }
}

/// Read the [`StreamHead`] for `stream`: the head bytes (rewinding afterwards) and one
/// `Stat`. A stream that refuses the read yields an empty head, one that refuses `Stat`
/// yields no size and no extension; every probe then simply misses.
pub(super) unsafe fn stream_head(stream: &IStream) -> StreamHead {
    let mut bytes = vec![0u8; HEAD_BYTES];
    let got = read_head(stream, &mut bytes).unwrap_or(0);
    bytes.truncate(got);
    let (size, ext) = stream_stat(stream);
    StreamHead { bytes, size, ext }
}

/// Seek to 0, fill as much of `buf` as the stream yields (looping over short reads,
/// stopping at the end of the stream), and seek back to 0. Returns the filled length,
/// never more than `buf.len()` whatever count the stream reports; None when a read fails.
pub(super) unsafe fn read_head(stream: &IStream, buf: &mut [u8]) -> Option<usize> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let filled = fill_from_current(stream, buf);
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    filled
}

/// One `IStream::Stat` (`STATFLAG_DEFAULT`, so the name comes with the size): the reported
/// size and the lowercased extension of the reported name. `(None, None)` when the stream
/// does not support `Stat`. `pwcsName` is a CoTaskMem allocation we own and must free.
pub(super) unsafe fn stream_stat(stream: &IStream) -> (Option<u64>, Option<String>) {
    let mut stat = STATSTG::default();
    if stream.Stat(&mut stat, STATFLAG_DEFAULT).is_err() {
        return (None, None);
    }
    let name = if stat.pwcsName.is_null() {
        None
    } else {
        let name = stat.pwcsName.to_string().ok();
        CoTaskMemFree(Some(stat.pwcsName.0 as *const c_void));
        name
    };
    let ext = name.as_deref().and_then(|name| {
        std::path::Path::new(name)
            .extension()
            .and_then(|ext| ext.to_str())
            .map(|ext| ext.to_ascii_lowercase())
    });
    (Some(stat.cbSize), ext)
}

/// Recover the backing file path from the shell's `IStream` via `IStream::Stat`
/// (`STATFLAG_DEFAULT` fills `pwcsName` — file-backed shell streams report the full path).
/// Returned only when it names an existing file, so a stream with no / non-file name simply
/// falls back to streaming. The `Stat` and the `pwcsName` free are [`st2k_base::host::stream_name`]'s.
///
/// TEST-ONLY, deliberately. Nothing in production may depend on this, because for the streams
/// the shell actually hands our handlers it always returns `None` (they report a bare leaf
/// name, and resolving a relative name against our own working directory would risk opening a
/// DIFFERENT file of the same name). It survives so tests can PIN that fact: see
/// `nameless_oversized_7z_is_not_streamed_past_max_size`, which asserts no path is
/// recoverable while the file's extension still is. Anything wanting a file TYPE wants
/// [`stream_extension`]; anything wanting to avoid buffering wants the stream itself.
#[cfg(test)]
pub(super) unsafe fn stream_path(stream: &IStream) -> Option<String> {
    let s = st2k_base::host::stream_name(stream)?;
    let p = std::path::Path::new(&s);
    // ABSOLUTE, then existing — in that order, and the absolute check is not cosmetic.
    // Streams routinely report only a LEAF NAME rather than a path: `SHCreateStreamOnFileEx`
    // does, and so does a shell item bound via `BHID_Stream` (both verified here). A bare
    // name reaching `is_file()` is resolved against OUR PROCESS'S working directory, so it
    // either fails — or, far worse, silently matches a DIFFERENT file that happens to share
    // the name, and we would then decode and cache that one as the user's thumbnail. Refusing
    // a relative name costs only a fast path we can retake from the stream itself.
    if p.is_absolute() && p.is_file() {
        Some(s)
    } else {
        safety::log_debugf!("stream_path: {s:?} is not an absolute path to an existing file");
        None
    }
}

/// File-name extension reported by a shell stream, without requiring that the
/// name be an absolute, currently-existing path.  Virtual shell sources often
/// report only a display name; that is still enough for a conservative format
/// gate, whereas [`stream_path`] intentionally rejects it for direct file I/O.
///
/// # Safety
/// `stream` must be a live COM `IStream` on a thread where COM is initialised.
pub unsafe fn stream_extension(stream: &IStream) -> Option<String> {
    stream_stat(stream).1
}

/// Read up to `max` bytes off the stream head in big sequential gulps, rewinding to 0
/// before and after. `size` is the stream's reported size, which caps the buffer so a
/// small file does not allocate the whole `max`. Shared by the video-prefix decode and
/// the head-preview rescue — the bounded read is the same, only the cap differs. None for
/// a failed read or fewer than 64 bytes.
pub(super) unsafe fn stream_prefix(
    stream: &IStream,
    size: Option<u64>,
    max: usize,
) -> Option<Vec<u8>> {
    stream_prefix_from(stream, Vec::new(), size, max)
}

/// [`stream_prefix`] continuing from bytes already in hand: `out` holds the stream's first
/// `out.len()` bytes verbatim and only the remainder up to the cap is read, so a probe
/// that has already pulled part of the head does not read those bytes a second time.
/// Rewinds to 0 afterwards.
pub(super) unsafe fn stream_prefix_from(
    stream: &IStream,
    mut out: Vec<u8>,
    size: Option<u64>,
    max: usize,
) -> Option<Vec<u8>> {
    let cap = size.map_or(max, |sz| usize::try_from(sz).map_or(max, |sz| sz.min(max)));
    let start = out.len().min(cap);
    out.truncate(start);
    if start < cap {
        stream
            .Seek(i64::try_from(start).ok()?, STREAM_SEEK_SET, None)
            .ok()?;
        out.resize(cap, 0);
        let filled = fill_from_current(stream, &mut out[start..]);
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);
        out.truncate(start.saturating_add(filled?));
    } else {
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    }
    (out.len() >= 64).then_some(out)
}

/// Read exactly `buf.len()` bytes starting at the stream's current position (looping over
/// short reads). None if the stream ends early or a read fails.
///
/// # Safety
/// `stream` must be a live COM `IStream` on a thread where COM is initialised.
pub unsafe fn read_full(stream: &IStream, buf: &mut [u8]) -> Option<()> {
    (fill_from_current(stream, buf)? == buf.len()).then_some(())
}

/// Read into `buf` from the stream's current position until it is full or the stream
/// ends. Returns the filled length, clamped to `buf.len()` whatever count the stream
/// reports; None when a read fails.
pub(super) unsafe fn fill_from_current(stream: &IStream, buf: &mut [u8]) -> Option<usize> {
    let mut filled = 0usize;
    while filled < buf.len() {
        let mut got: u32 = 0;
        let want = (buf.len() - filled).min(u32::MAX as usize) as u32;
        let hr = stream.Read(
            buf[filled..].as_mut_ptr() as *mut c_void,
            want,
            Some(&mut got),
        );
        if hr.is_err() {
            return None;
        }
        if got == 0 {
            break;
        }
        filled = filled.saturating_add(got as usize).min(buf.len());
    }
    Some(filled)
}

/// `std::io` Read + Seek over a COM IStream, so lofty can parse tags by seeking
/// instead of us draining the file into memory.
pub(super) struct IStreamReader {
    pub(super) stream: IStream,
}

impl std::io::Read for IStreamReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut got: u32 = 0;
        unsafe {
            self.stream.Read(
                buf.as_mut_ptr() as *mut c_void,
                buf.len() as u32,
                Some(&mut got),
            )
        }
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
        unsafe { self.stream.Seek(off, origin, Some(&mut newpos)) }
            .map_err(std::io::Error::other)?;
        Ok(newpos)
    }
}

/// Drain an IStream into a Vec, bounded by `max`.
pub(super) unsafe fn read_all(
    stream: &IStream,
    max: usize,
    size_hint: Option<u64>,
) -> Result<Vec<u8>> {
    read_all_append(stream, max, size_hint, Vec::new())
}

/// Continue draining an IStream after an already-read prefix, bounded by `max`.
/// The caller positions the stream immediately after `out` before entering.
pub(super) unsafe fn read_all_append(
    stream: &IStream,
    max: usize,
    size_hint: Option<u64>,
    mut out: Vec<u8>,
) -> Result<Vec<u8>> {
    if out.len() > max {
        return Err(Error::from(E_FAIL));
    }
    // Pre-size from the (already size-checked) stream length so every read lands straight
    // in the buffer's spare capacity: no scratch chunk, no second copy, and no doubling
    // reallocation at 64/128/256 MiB. The reservation is capped by `max`, so a stream that
    // lies about its size cannot force a giant allocation, and the `max` test in the loop
    // still bounds the true read. A refused reservation is reported, not aborted on.
    let hint = size_hint.map_or(0, |h| usize::try_from(h).map_or(max, |h| h.min(max)));
    if hint > out.len() {
        out.try_reserve(hint - out.len())
            .map_err(|_| Error::from(E_OUTOFMEMORY))?;
    }
    loop {
        if !read_step(stream, &mut out, max)? {
            break;
        }
    }
    Ok(out)
}

/// One bounded read step of `read_all_append`: tops `out` up by at most one 1 MiB chunk,
/// refusing a stream of more than `max` bytes with `E_FAIL`, and reports whether more bytes
/// may remain (`true`) or the stream is exhausted (`false`). `out` keeps its prefix on every
/// error path, exactly as the inlined loop did.
unsafe fn read_step(stream: &IStream, out: &mut Vec<u8>, max: usize) -> Result<bool> {
    // 1 MiB steps: the stream is marshaled (often cross-process), so per-Read overhead is
    // real — 64 KiB steps cost a 100 MB file ~1,600 round trips. Each step is zeroed and
    // then filled in place; within the reservation that is a memset, never a copy.
    const STEP: usize = 1 << 20;
    let len = out.len();
    let room = max.saturating_sub(len);
    if room == 0 {
        // At the cap: one probe read tells a stream of exactly `max` bytes from a
        // longer one, which is refused exactly as it always was.
        let mut probe = [0u8; 1];
        let mut got: u32 = 0;
        stream
            .Read(probe.as_mut_ptr() as *mut c_void, 1, Some(&mut got))
            .ok()?;
        return if got == 0 {
            Ok(false)
        } else {
            Err(Error::from(E_FAIL))
        };
    }
    let want = room.min(STEP);
    out.try_reserve(want)
        .map_err(|_| Error::from(E_OUTOFMEMORY))?;
    out.resize(len.saturating_add(want), 0);
    let mut got: u32 = 0;
    let hr = stream.Read(
        out[len..].as_mut_ptr() as *mut c_void,
        want as u32,
        Some(&mut got),
    );
    // S_OK and S_FALSE are both successes; a failing HRESULT is a real transport
    // error (network/cloud-placeholder stream), NOT end-of-stream — don't mistake
    // it for EOF and silently feed a truncated buffer to the decoder.
    if let Err(e) = hr.ok() {
        out.truncate(len);
        return Err(e);
    }
    let n = (got as usize).min(want); // never trust got > buffer
    out.truncate(len.saturating_add(n));
    Ok(n != 0) // success + 0 bytes == genuine EOF
}
