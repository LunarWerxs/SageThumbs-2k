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

const MAX_BYTES: usize = 256 * 1024 * 1024;

#[implement(IThumbnailProvider, IInitializeWithStream)]
pub struct ThumbnailProvider {
    _ref: crate::ModuleRef,
    stream: RefCell<Option<IStream>>,
}

impl Default for ThumbnailProvider {
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
            // Reject null out-params up front (mirrors DllGetClassObject) so the
            // later writes are provably safe and no HBITMAP is allocated/leaked.
            if phbmp.is_null() || pdwalpha.is_null() {
                return Err(Error::from(E_POINTER));
            }
            unsafe {
                *phbmp = HBITMAP::default();
                *pdwalpha = WTSAT_UNKNOWN;
            }

            // Option: master switch. Returning a failure lets the shell fall
            // back to the file's default icon.
            if !settings::thumbnails_enabled() {
                safety::log_debug("GetThumbnail: disabled via EnableThumbs=0");
                return Err(Error::from(E_FAIL));
            }

            let bytes = {
                let borrow = self.stream.borrow();
                let stream = borrow.as_ref().ok_or_else(|| Error::from(E_FAIL))?;

                // Audio: the album art lives in the metadata, so we seek straight
                // to it and read ONLY the art (not the whole file). This both
                // sidesteps the size cap — a multi-GB audiobook still thumbnails —
                // and avoids buffering the file. If it's audio but has no art, we
                // stop here (the raw audio isn't a decodable image anyway).
                if let Some(art) = unsafe { audio_art(stream) } {
                    art
                } else {
                    // Everything else: skip oversized files cheaply via the stream
                    // length before reading into memory. The effective cap is the
                    // user's MaxSize but never above the hard MAX_BYTES ceiling
                    // ("0 = unlimited" means "up to MAX_BYTES").
                    let max = settings::max_file_size_bytes().min(MAX_BYTES as u64);
                    if let Some(size) = unsafe { stream_size(stream) } {
                        if size > max {
                            safety::log_debug(&format!("GetThumbnail: skip, {size} bytes over limit"));
                            return Err(Error::from(E_FAIL));
                        }
                    }
                    unsafe { read_all(stream, MAX_BYTES)? }
                }
            };

            // Option: cap the generated edge at the user's max (default 256,
            // clamped to the legacy [32, 512] range). decode never upscales.
            let cx = cx.min(settings::max_thumb_size());

            safety::log_debug(&format!("GetThumbnail: cx={cx} bytes={}", bytes.len()));
            let img = decode::decode_thumbnail_opts(&bytes, cx, settings::use_embedded())?;
            safety::log_debug(&format!("GetThumbnail: decoded {}x{}", img.width, img.height));
            let hbmp =
                unsafe { dib::create_premultiplied_dib(img.width as i32, img.height as i32, &img.rgba)? };

            unsafe {
                *phbmp = hbmp;
                *pdwalpha = WTSAT_ARGB;
            }
            Ok(())
        })
    }
}

/// The stream's total size in bytes via `IStream::Stat`, or None if the stream
/// doesn't support it (then we just read up to the hard `MAX_BYTES` cap).
unsafe fn stream_size(stream: &IStream) -> Option<u64> {
    let mut stat = STATSTG::default();
    stream.Stat(&mut stat, STATFLAG_NONAME).ok()?;
    Some(stat.cbSize)
}

/// Sniff the stream for audio and, if so, extract only the embedded art via a
/// seek-only read (lofty seeks to the metadata — we never buffer the whole file,
/// so even a multi-GB audiobook thumbnails). Returns None for non-audio (caller
/// takes the normal path); rewinds the stream to 0 either way.
unsafe fn audio_art(stream: &IStream) -> Option<Vec<u8>> {
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    let mut head = [0u8; 16];
    let mut got: u32 = 0;
    let hr = stream.Read(head.as_mut_ptr() as *mut c_void, head.len() as u32, Some(&mut got));
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    if hr.is_err() || (got as usize) < 12 || !crate::container::looks_like_audio(&head[..got as usize]) {
        return None;
    }
    crate::container::audio_art_from_reader(IStreamReader { stream: stream.clone() })
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
        Ok(got as usize)
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
unsafe fn read_all(stream: &IStream, max: usize) -> Result<Vec<u8>> {
    let mut out: Vec<u8> = Vec::new();
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
