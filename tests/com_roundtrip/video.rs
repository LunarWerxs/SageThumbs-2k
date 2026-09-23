//! Video through the COM handshake: an apartment-bound stream, the block-stream worker from STA and MTA, and the refusal for H.264 4:4:4.

use super::*;

/// An in-memory read-only `IStream` that is deliberately NOT free-threaded: it aggregates no
/// free-threaded marshaler, so a thread in another apartment is handed a PROXY and every one
/// of its reads marshals back to the apartment that created the stream. That is the property
/// Explorer's thumbnail stream has, and the property the issue #35 worker must survive:
/// created on an STA thread, it can only be read from a worker while that STA thread pumps.
/// `SHCreateMemStream` would prove nothing here, since it may be free-threaded and hand the
/// worker a direct pointer.
#[implement(IStream)]
pub(super) struct ApartmentStream {
    pub(super) bytes: Vec<u8>,
    pub(super) pos: Mutex<u64>,
}

impl ApartmentStream {
    pub(super) fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes,
            pos: Mutex::new(0),
        }
    }
}

impl ISequentialStream_Impl for ApartmentStream_Impl {
    fn Read(&self, pv: *mut c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
        if pv.is_null() {
            return E_POINTER;
        }
        let mut pos = self.pos.lock().unwrap();
        let start = (*pos as usize).min(self.bytes.len());
        let n = (cb as usize).min(self.bytes.len() - start);
        unsafe { std::ptr::copy_nonoverlapping(self.bytes.as_ptr().add(start), pv as *mut u8, n) };
        *pos += n as u64;
        if !pcbread.is_null() {
            unsafe { *pcbread = n as u32 };
        }
        if n == cb as usize {
            S_OK
        } else {
            S_FALSE
        }
    }
    fn Write(&self, _pv: *const c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
        STG_E_ACCESSDENIED
    }
}

impl IStream_Impl for ApartmentStream_Impl {
    fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> Result<()> {
        let mut pos = self.pos.lock().unwrap();
        let base: i128 = match dworigin {
            STREAM_SEEK_SET => 0,
            STREAM_SEEK_CUR => *pos as i128,
            STREAM_SEEK_END => self.bytes.len() as i128,
            _ => return Err(Error::from(E_INVALIDARG)),
        };
        let np = base + dlibmove as i128;
        if np < 0 {
            return Err(Error::from(E_INVALIDARG));
        }
        *pos = np as u64;
        if !plibnewposition.is_null() {
            unsafe { *plibnewposition = *pos };
        }
        Ok(())
    }
    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> Result<()> {
        if pstatstg.is_null() {
            return Err(Error::from(E_POINTER));
        }
        unsafe {
            *pstatstg = STATSTG {
                r#type: STGTY_STREAM.0 as u32,
                cbSize: self.bytes.len() as u64,
                ..Default::default()
            };
        }
        Ok(())
    }
    fn SetSize(&self, _libnewsize: u64) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn CopyTo(
        &self,
        _pstm: windows::core::Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn Commit(&self, _grfcommitflags: &STGC) -> Result<()> {
        Ok(())
    }
    fn Revert(&self) -> Result<()> {
        Ok(())
    }
    fn LockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: &LOCKTYPE) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
    fn Clone(&self) -> Result<IStream> {
        Err(Error::from(E_NOTIMPL))
    }
}

pub(super) fn fixture_video(name: &str) -> Vec<u8> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("video")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

/// The AVI fixture is MPEG-4 Part 2, an inbox decoder on consumer Windows. A Server image
/// may lack it, so the decode tests skip rather than fail there.
pub(super) fn mpeg4_part2_decoder_present() -> bool {
    use windows::Win32::Media::MediaFoundation::MFVideoFormat_MP4V;
    sagethumbs2k_core::video::media_foundation_available()
        && sagethumbs2k_core::vcodec::decoder_installed(MFVideoFormat_MP4V) == Some(true)
}

/// ffmpeg's `testsrc` pattern is colourful: require real variety, the way the shell-surface
/// proof script does, so a quietly failing handler's flat grey tile cannot pass.
pub(super) fn assert_testsrc_frame(t: &Thumb) {
    assert!(t.w > 0 && t.h > 0, "empty thumbnail");
    let mut seen = std::collections::HashSet::new();
    for y in (0..t.h).step_by((t.h / 16).max(1)) {
        for x in (0..t.w).step_by((t.w / 16).max(1)) {
            let [b, g, r, _] = t.px(x, y);
            seen.insert((r / 32, g / 32, b / 32));
        }
    }
    assert!(
        seen.len() >= 4,
        "thumbnail is effectively blank ({} distinct sampled colours)",
        seen.len()
    );
}

/// An AVI has no MP4/MKV index our own parsers read, so through the shell it can ONLY
/// thumbnail via the block-stream tier: Media Foundation seeking its own index over our
/// block cache. The stream is created on this STA thread, so the worker holds a proxy and
/// each of its reads is dispatched HERE, which only happens while this thread pumps. A
/// thumbnail coming back at all therefore proves the pumping wait; a non-pumping wait would
/// deadlock into the 8 s budget and this would fail on both the result and the clock.
#[test]
fn video_avi_thumbnails_via_the_block_stream_worker_from_an_sta_bound_stream() {
    let _settings = settings_lock();
    if !mpeg4_part2_decoder_present() {
        eprintln!("no MPEG-4 Part 2 decoder on this Windows - skipped");
        return;
    }
    let stream: IStream = ApartmentStream::new(fixture_video("mpeg4-160x120.avi")).into();
    // THE RESULT IS THE PROOF, and the clock never was. Every one of the worker's reads is a
    // marshaled call on this thread, so an unserved wait cannot decode a single block: it
    // spends the whole `VIDEO_TIMEOUT` budget, the grab returns None, the AVI has no other
    // tier, and this `expect` fails. The 6 s wall-clock assertion that used to sit below
    // therefore tested the CI runner's load, not the code — it went red twice on hosted
    // runners (2026-09-09, and 2026-09-14 at 8.15 s) while the thumbnail itself came back
    // correct both times, which is the shape of a flaky gate rather than a defect.
    let t = unsafe { get_thumbnail_from_stream(&stream, 96) }
        .expect("the AVI must thumbnail through the shell handshake from an STA-bound stream");
    assert_testsrc_frame(&t);
}

/// The thumbnail host (dllhost) is an MTA: the worker gets the same pointer back from the
/// table and the wait is a plain wait. Same file, same tier, the other threading model.
#[test]
fn video_avi_thumbnails_via_the_block_stream_worker_on_an_mta_thread() {
    let _settings = settings_lock();
    if !mpeg4_part2_decoder_present() {
        eprintln!("no MPEG-4 Part 2 decoder on this Windows - skipped");
        return;
    }
    let bytes = fixture_video("mpeg4-160x120.avi");
    let t = std::thread::spawn(move || {
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
            .ok()
            .expect("MTA init");
        let stream: IStream = ApartmentStream::new(bytes).into();
        unsafe { get_thumbnail_from_stream(&stream, 96) }
    })
    .join()
    .expect("worker thread")
    .expect("the AVI must thumbnail through the shell handshake from an MTA thread");
    assert_testsrc_frame(&t);
}

/// The reporter's file shape: H.264 in a profile the Windows decoder does not implement.
/// Through the real shell handshake the DLL must decline at once, with the always-on log
/// line that names why, instead of handing the stream to a decoder that (on Windows 10)
/// wedges. Timing is the assertion that no decoder was asked: every path that asks one is
/// bounded by an 8 s budget, and the old behaviour paid that budget twice.
#[test]
fn video_h264_444_is_refused_at_once_through_the_shell_handshake() {
    let _settings = settings_lock();
    let bytes = fixture_video("h264-high444-320x240.mp4");
    let log = st2k_base::safety::log_file();
    let before = log
        .as_ref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len() as usize)
        .unwrap_or(0);
    let started = Instant::now();
    let r = unsafe { get_thumbnail(&bytes, 96) };
    let took = started.elapsed();
    assert!(r.is_err(), "a 4:4:4 clip must not thumbnail");
    assert!(
        took < Duration::from_secs(2),
        "the refusal took {took:?}; an 8 s wait means a decoder was asked after all"
    );
    if let Some(p) = log {
        let text = std::fs::read(&p).unwrap_or_default();
        let from = if before <= text.len() { before } else { 0 };
        let tail = String::from_utf8_lossy(&text[from..]);
        assert!(
            tail.contains("issue #35"),
            "the always-on refusal line must land in the log; new lines were: {tail}"
        );
    }
}
