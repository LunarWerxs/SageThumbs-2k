//! The dialog's life: create and destroy, the WM_APP results that arrive from worker threads, and its timers.

use super::*;

/// Window-lifecycle + app-posted messages: creation, teardown, resize limits, the
/// background update/sync callbacks, and the sponsor feed arriving. `None` means the
/// message isn't one of these — fall through to the next dispatch group.
pub(super) unsafe fn on_lifecycle_msg(
    hwnd: HWND,
    msg: u32,
    _wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        WM_CREATE => Some(on_create(hwnd)),
        crate::update::WM_APP_UPDATE => Some(on_update_available(hwnd, lparam)),
        WM_APP_SYNC => Some(on_app_sync(hwnd, lparam)),
        WM_APP_CACHE => Some(on_app_cache(hwnd, lparam)),
        WM_APP_LICENCE => Some(on_app_licence(hwnd, lparam)),
        WM_GETMINMAXINFO => Some(on_getminmaxinfo(lparam)),
        WM_SIZE => {
            let client_h = ((lparam.0 >> 16) & 0xFFFF) as i32;
            on_resize(hwnd, client_h);
            Some(LRESULT(0))
        }
        WM_APP_SPONSORS => Some(on_app_sponsors(hwnd, lparam)),
        WM_DPICHANGED => {
            wm_dpichanged(hwnd, lparam);
            Some(LRESULT(0))
        }
        WM_CLOSE => {
            close_settings(hwnd);
            Some(LRESULT(0))
        }
        WM_DESTROY => Some(on_destroy(hwnd)),
        _ => None,
    }
}

/// The dialog's one exit path — WM_CLOSE (the window X / Alt+F4) and IDCANCEL (the "Close"
/// button) used to each carry their own copy of this. Blocks up to 6s flushing any pending
/// sync push before tearing the window down, so a Save right before closing isn't lost to a
/// race with the background push.
pub(super) unsafe fn close_settings(hwnd: HWND) {
    crate::sync_client::flush_pending(std::time::Duration::from_secs(6));
    let _ = DestroyWindow(hwnd);
}

pub(super) unsafe fn on_create(hwnd: HWND) -> LRESULT {
    // Bring up GDI+ for this window's lifetime so the dark-mode owner-draw can
    // render its toggle switches / icons / rounded buttons anti-aliased.
    GDIP_TOKEN.with(|t| t.set(crate::gdip::startup()));
    let hinst: HINSTANCE = windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
        .unwrap()
        .into();
    build_controls(hwnd, hinst);
    // Keep the hotkey-service status line live (so a self-heal on open,
    // or a later stop, is reflected without reopening).
    let _ = SetTimer(Some(hwnd), TIMER_SHOT_STATUS, 1000, None);
    // Lazy, throttled, background update check: it never blocks this window
    // opening, hits GitHub at most once a day (cached on disk in between), and
    // stays silent unless a newer release exists — then it posts WM_APP_UPDATE
    // to quietly nudge (no popup). See `update::lazy_check`.
    let target = hwnd.0 as isize;
    crate::update::lazy_check(move |tag| {
        let raw = Box::into_raw(Box::new(tag));
        let posted = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(HWND(target as *mut core::ffi::c_void)),
            crate::update::WM_APP_UPDATE,
            WPARAM(0),
            LPARAM(raw as isize),
        );
        if posted.is_err() {
            // The window vanished before delivery — reclaim the boxed tag.
            drop(Box::from_raw(raw));
        }
    });
    // If already signed in for settings sync, pull the cloud copy in the
    // background (applies to HKCU; takes effect for new thumbnails). No-op and
    // zero network when signed out.
    spawn_sync_pull(hwnd);
    LRESULT(0)
}

pub(super) unsafe fn on_update_available(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // A lazy background check found a newer release. Reclaim the boxed tag and
    // NON-intrusively relabel the "Check for updates" button into a quiet nudge
    // (no popup); clicking it still opens the About box, whose status pill shows the
    // update and offers the one-click install.
    let tag = if lparam.0 != 0 {
        *Box::from_raw(lparam.0 as *mut String)
    } else {
        String::new()
    };
    if let Ok(btn) = GetDlgItem(Some(hwnd), ID_CHECK_UPDATES) {
        let label = if tag.is_empty() {
            wide("Update available")
        } else {
            wide(&format!("Update to v{tag}"))
        };
        let _ = SetWindowTextW(btn, PCWSTR(label.as_ptr()));
    }
    LRESULT(0)
}

pub(super) unsafe fn on_app_sync(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // A background sync op (sign-in / pull / disconnect) finished on a worker
    // thread. Reclaim the boxed event and update the UI on this message thread.
    if lparam.0 != 0 {
        let event = *Box::from_raw(lparam.0 as *mut SyncEvent);
        handle_sync_event(hwnd, event);
    }
    LRESULT(0)
}

/// A background licence op (redeem / check-now) finished on a worker thread. Reclaim the
/// boxed event and update the Licence page on this message thread — same reclaim shape as
/// [`on_app_sync`] just above.
pub(super) unsafe fn on_app_licence(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    if lparam.0 != 0 {
        let event = *Box::from_raw(lparam.0 as *mut LicenceEvent);
        handle_licence_event(hwnd, event);
    }
    LRESULT(0)
}

/// A background `spawn_cache_rebuild` worker finished (thumbnail cache clear + Explorer
/// restart). Reclaim the boxed event, re-enable the window, and show its follow-up message
/// (if any) now that the restart has actually completed.
pub(super) unsafe fn on_app_cache(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
    if lparam.0 != 0 {
        let event = *Box::from_raw(lparam.0 as *mut CacheRebuiltEvent);
        let _ = EnableWindow(hwnd, true);
        if let Some((text, caption)) = event.after {
            msg(hwnd, text, caption, MB_ICONINFORMATION);
        }
    }
    LRESULT(0)
}

pub(super) unsafe fn on_getminmaxinfo(lparam: LPARAM) -> LRESULT {
    // Lock the WIDTH (vertical resize only) + a minimum height = the design
    // size. (No-op until the first WM_SIZE captures the design dimensions.)
    if let Some((w, h0)) = RESIZE.with(|s| s.borrow().as_ref().map(|st| (st.win_w, st.win_h0))) {
        let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
        mmi.ptMinTrackSize.x = w;
        mmi.ptMaxTrackSize.x = w;
        mmi.ptMinTrackSize.y = h0;
    }
    LRESULT(0)
}

/// The sponsor feed arrived from the download thread: take ownership, show
/// the first sponsor (replacing the placeholder), and start the timers.
pub(super) unsafe fn on_app_sponsors(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
        let rot = lparam.0 as *mut SponsorRotator;
        if !rot.is_null() {
            // Swap in the new feed, freeing any prior one.
            let prev = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut SponsorRotator;
            let _ = KillTimer(Some(hwnd), TIMER_ROTATE);
            SetWindowLongPtrW(banner, GWLP_USERDATA, rot as isize);
            let r = &*rot;
            // Free the bitmap currently in the static ONLY on the first
            // swap (prev null = it still holds the embedded placeholder).
            // A later feed's frames are rotator-owned and freed by
            // drop_sponsor_rotator below, so freeing them here too would
            // double-free that GDI object.
            show_current_image(hwnd, banner, r, prev.is_null());
            if r.rotates() {
                let _ = SetTimer(Some(hwnd), TIMER_ROTATE, r.rotate_ms, None);
            }
            if !prev.is_null() {
                // The banner tooltip pulls its text by pointer from the
                // shown sponsor (callback-driven). If a hint for the *prev*
                // feed is on screen, dismiss it (TTM_POP) before freeing
                // that feed — otherwise it would point at freed memory.
                let tip = HWND(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut c_void);
                if !tip.is_invalid() {
                    SendMessageW(tip, TTM_POP, None, None);
                }
                drop_sponsor_rotator(prev);
            }
        }
    } else {
        drop_sponsor_rotator(lparam.0 as *mut SponsorRotator); // window gone
    }
    LRESULT(0)
}

pub(super) unsafe fn on_destroy(hwnd: HWND) -> LRESULT {
    let _ = KillTimer(Some(hwnd), TIMER_SHOT_STATUS);
    // Stop + free the sponsor rotation (both timers + every sponsor's bitmaps).
    if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
        let _ = KillTimer(Some(hwnd), TIMER_BANNER);
        let _ = KillTimer(Some(hwnd), TIMER_ROTATE);
        let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut SponsorRotator;
        if !rot.is_null() {
            SetWindowLongPtrW(banner, GWLP_USERDATA, 0);
            drop_sponsor_rotator(rot);
        } else {
            // No sponsor feed ever installed (the gate passed — the manifest
            // listed sponsors — but every image download/decode failed, so
            // WM_APP_SPONSORS never posted a rotator). The banner still holds
            // the embedded placeholder set in build_controls; a STATIC does
            // NOT free a STM_SETIMAGE bitmap, so reclaim it here or it leaks
            // one GDI bitmap per opened Settings window.
            let prev = SendMessageW(
                banner,
                STM_SETIMAGE,
                Some(WPARAM(IMAGE_BITMAP.0 as usize)),
                Some(LPARAM(0)),
            );
            if prev.0 != 0 {
                let _ = DeleteObject(HGDIOBJ(prev.0 as *mut c_void));
            }
        }
    }
    scroll::SCROLL.with(|s| *s.borrow_mut() = scroll::ScrollData::default());
    GDIP_TOKEN.with(|t| {
        let tok = t.replace(0);
        if tok != 0 {
            crate::gdip::shutdown(tok);
        }
    });
    PostQuitMessage(0);
    LRESULT(0)
}

/// The three WM_TIMER chords (status refresh / GIF frame advance / sponsor rotate)
/// plus the left-column scrollbar + mouse wheel.
pub(super) unsafe fn on_timer_or_scroll_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    match msg {
        // Keep the hotkey-service status line honest while the dialog is open.
        WM_TIMER if wparam.0 == TIMER_SHOT_STATUS => {
            refresh_shot_status(hwnd);
            Some(LRESULT(0))
        }
        WM_TIMER if wparam.0 == TIMER_BANNER => Some(on_timer_banner(hwnd)),
        WM_TIMER if wparam.0 == TIMER_ROTATE => Some(on_timer_rotate(hwnd)),
        // Left-column scrolling (dark mode): the scrollbar + the mouse wheel.
        WM_VSCROLL => {
            scroll::on_vscroll(hwnd, wparam, lparam);
            Some(LRESULT(0))
        }
        WM_MOUSEWHEEL => {
            let wheel = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
            let pos = scroll::SCROLL.with(|s| s.borrow().pos);
            scroll::scroll_to(hwnd, pos - wheel / 120 * dpi_scale(hwnd, 42));
            Some(LRESULT(0))
        }
        _ => None,
    }
}

/// Advance the current image's GIF animation one frame (frames are reused
/// each loop, so don't free the prior one; WM_DESTROY frees them all).
pub(super) unsafe fn on_timer_banner(hwnd: HWND) -> LRESULT {
    if let Some((banner, rot)) = banner_rotator(hwnd) {
        let r = &mut *rot;
        let (cur, imgi) = (r.cur, r.img);
        let nframes = r
            .sponsors
            .get(cur)
            .and_then(|a| a.images.get(imgi))
            .map_or(0, |im| im.frames.len());
        if nframes > 1 {
            r.frame = (r.frame + 1) % nframes;
            let f = r.sponsors[cur].images[imgi].frames[r.frame];
            SendMessageW(
                banner,
                STM_SETIMAGE,
                Some(WPARAM(IMAGE_BITMAP.0 as usize)),
                Some(LPARAM(f)),
            );
        }
    }
    LRESULT(0)
}

/// Rotate to the next sponsor / image: advance the rotator, then show the
/// new art (raw STM_SETIMAGE so the prior bitmap survives — the rotator
/// still owns it). The tooltip pulls the fresh text on the next hover.
pub(super) unsafe fn on_timer_rotate(hwnd: HWND) -> LRESULT {
    if let Some((banner, rot)) = banner_rotator(hwnd) {
        (*rot).advance();
        show_current_image(hwnd, banner, &*rot, false);
    }
    LRESULT(0)
}
