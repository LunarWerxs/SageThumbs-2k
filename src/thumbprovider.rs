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

use windows::core::{Error, Ref, Result};
use windows::Win32::Foundation::{E_FAIL, E_POINTER};
use windows::Win32::Graphics::Gdi::HBITMAP;
use windows::Win32::System::Com::IStream;
use windows::Win32::UI::Shell::PropertiesSystem::{
    IInitializeWithStream, IInitializeWithStream_Impl,
};
use windows::Win32::UI::Shell::{
    IThumbnailProvider, IThumbnailProvider_Impl, WTSAT_ARGB, WTSAT_UNKNOWN, WTS_ALPHATYPE,
};
use windows_implement::implement;

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
            let mut slot = self
                .stream
                .try_borrow_mut()
                .map_err(|_| Error::from(E_FAIL))?;
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

        // Option: cap the generated edge at the user's max (default 256,
        // clamped to the legacy [32, 512] range). decode never upscales.
        // Resolved BEFORE the cascade: the streaming EXR tier scales as it reads,
        // so it needs to know the tile size we actually want.
        let cx = cx.min(cfg.max_thumb);

        // Acquire the source on THIS thread — the marshaled IStream is
        // apartment-bound. The shared cascade never buffers an unbounded file.
        let source = {
            let borrow = self.stream.borrow();
            let stream = borrow.as_ref().ok_or_else(|| Error::from(E_FAIL))?;
            unsafe { streamsrc::stream_source(stream, cfg.max_file_bytes, cx, "GetThumbnail") }?
        };

        // A168: unlike `pdf.rs`/`ocr.rs`, this decode runs INLINE on the calling COM thread —
        // no detached-worker + `recv_timeout` host-side wall budget. That's deliberate, not an
        // oversight: those two wrap a WinRT call that can genuinely deadlock on the wrong
        // apartment, so they need a fresh MTA thread regardless of timing; nothing here has
        // that apartment hazard. The actual gap this leaves is a pathological/malformed file
        // that makes `decode_thumbnail_opts` itself hang with no internal budget — and the
        // backstop for THAT is process isolation: `IThumbnailProvider` runs in the shell's
        // isolated `dllhost.exe` surrogate (see CLAUDE.md §4), so a wedged decode parks that
        // disposable host, not Explorer, and the shell's own per-call timeout eventually kills
        // it. Adding a second worker thread here would duplicate that host-side budget for a
        // hang the OS-level isolation already survives, at the cost of a second COM-apartment
        // hazard on whatever the tiered decoders (WIC/magick) assume about the calling thread.
        let img = match source {
            StreamSource::Frame(frame) => decode::thumbnail_from_image(frame, cx),
            StreamSource::Bytes(bytes) => {
                safety::log_debug(&format!("GetThumbnail: cx={cx} bytes={}", bytes.len()));
                decode::decode_thumbnail_opts(&bytes, cx, cfg.use_embedded)?
            }
            StreamSource::Covers(covers) => {
                safety::log_debug(&format!("GetThumbnail: cx={cx} covers={}", covers.len()));
                decode::thumbnail_from_covers(&covers, cx)?
            }
        };
        safety::log_debug(&format!(
            "GetThumbnail: decoded {}x{}",
            img.width, img.height
        ));

        // Optional transparency checkerboard (`ThumbChecker`, off by default). Runs BEFORE
        // the badge: the badge is an overlay on the finished picture, and a checkerboard
        // composited over it would sit on top of the label. This makes the tile opaque.
        let mut img = img;
        if cfg.thumb_checker {
            crate::checkerpx::compose_under(&mut img.rgba, img.width, img.height);
        }

        // Optional format badge (`FormatBadge`, off by default). Stamped HERE, on the
        // finished tile, so it is the last thing applied and every decode tier gets it for
        // free. Note the shell CACHES what we return, so a toggle only shows up on tiles
        // rendered after it — the Settings dialog clears the thumbnail cache when the
        // option changes, otherwise turning it on looks like it did nothing.
        if cfg.format_badge {
            let label = {
                let borrow = self.stream.borrow();
                borrow
                    .as_ref()
                    .and_then(|s| unsafe { crate::stream_name(s) })
                    .and_then(|n| crate::badge::label_for(&n))
            };
            if let Some(label) = label {
                crate::badge::stamp(
                    &mut img.rgba,
                    img.width,
                    img.height,
                    &label,
                    cfg.badge_style,
                );
            }
        }

        let hbmp = unsafe {
            dib::create_premultiplied_dib(img.width as i32, img.height as i32, &img.rgba)?
        };

        unsafe {
            *phbmp = hbmp;
            *pdwalpha = WTSAT_ARGB;
        }
        Ok(())
    }
}
