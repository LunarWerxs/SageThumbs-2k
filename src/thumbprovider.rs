//! The thumbnail provider: IThumbnailProvider + IInitializeWithStream.
//!
//! The shell hands us an IStream via `Initialize`; we stash it (methods take
//! `&self`, hence the `RefCell`) and decode it in `GetThumbnail`. Using
//! IInitializeWithStream is what lets the shell run us in its isolated
//! out-of-process host without `DisableProcessIsolation`.
//!
//! The stream → decodable-source cascade (video frame-grab tiers, seek-only
//! audio album art, streamed archive covers, the head-preview prefix rescue,
//! the bounded whole-file read) lives in [`crate::streamsrc`], shared with the
//! preview-pane handler.

use core::cell::RefCell;

use windows_implement::implement;
use windows::core::{Error, Ref, Result};
use windows::Win32::Foundation::{E_FAIL, E_POINTER};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::IStream;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTS_ALPHATYPE, WTSAT_ARGB, WTSAT_UNKNOWN,
};

use crate::streamsrc::{self, StreamSource};
use crate::{decode, dib, safety, settings};

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

        // Acquire the source on THIS thread — the marshaled IStream is
        // apartment-bound. The shared cascade never buffers an unbounded file.
        let source = {
            let borrow = self.stream.borrow();
            let stream = borrow.as_ref().ok_or_else(|| Error::from(E_FAIL))?;
            unsafe { streamsrc::stream_source(stream, cfg.max_file_bytes, "GetThumbnail") }?
        };

        // Option: cap the generated edge at the user's max (default 256,
        // clamped to the legacy [32, 512] range). decode never upscales.
        let cx = cx.min(cfg.max_thumb);

        let img = match source {
            StreamSource::Frame(frame) => decode::thumbnail_from_image(frame, cx),
            StreamSource::Bytes(bytes) => {
                safety::log_debug(&format!("GetThumbnail: cx={cx} bytes={}", bytes.len()));
                decode::decode_thumbnail_opts(&bytes, cx, cfg.use_embedded)?
            }
        };
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
