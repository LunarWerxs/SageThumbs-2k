//! The message groups the window procedure hands off: geometry, app messages, timers, size, activation and teardown.

use super::*;

/// Window geometry/paint messages: hit-testing, sizing constraints, paint/print, and
/// resize-drag bookkeeping. `None` when `msg` isn't one of these; the caller tries the next
/// category.
pub(super) unsafe fn on_geometry_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    Some(match msg {
        WM_NCHITTEST => on_nchittest(hwnd, wparam, lparam),
        WM_GETMINMAXINFO => {
            let mmi = &mut *(lparam.0 as *mut MINMAXINFO);
            mmi.ptMinTrackSize.x = crate::win::dpi_scale(hwnd, MIN_W);
            mmi.ptMinTrackSize.y = crate::win::dpi_scale(hwnd, MIN_H);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // WM_PAINT fills the whole client; skip the erase flash
        WM_PAINT => {
            paint(hwnd);
            LRESULT(0)
        }
        WM_PRINTCLIENT => {
            paint_into(hwnd, HDC(wparam.0 as *mut _));
            LRESULT(0)
        }
        WM_SIZE => on_size(hwnd),
        WM_SIZING => {
            // A real frame drag (never our own SetWindowPos) — flag it so WM_EXITSIZEMOVE
            // knows this was a RESIZE and not just a move, and remembers the size.
            (*state(hwnd)).user_sized.set(true);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_EXITSIZEMOVE => {
            remember_size(hwnd);
            LRESULT(0)
        }
        WM_NCLBUTTONDBLCLK => {
            // Double-click the caption = forget the dragged size and fit this file again.
            // DefWindowProc would send SC_MAXIMIZE, which this WS_POPUP window can't honour
            // anyway, so nothing is being taken away.
            if wparam.0 as u32 == HTCAPTION {
                forget_size(hwnd);
                return Some(LRESULT(0));
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DPICHANGED => {
            crate::win::wm_dpichanged(hwnd, lparam);
            LRESULT(0)
        }
        _ => return None,
    })
}

/// App-defined async/custom messages (`WM_APP_*`, plus the video player's own registered
/// message) and the tick timer: decode/render completions posted from worker threads, and
/// periodic UI upkeep. `None` when `msg` isn't one of these.
pub(super) unsafe fn on_app_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    Some(match msg {
        WM_TIMER => on_timer(hwnd, wparam),
        WM_APP_RENDER => {
            on_render(hwnd, wparam, lparam);
            LRESULT(0)
        }
        WM_APP_ANIM => {
            on_anim(hwnd, lparam);
            LRESULT(0)
        }
        WM_APP_MDIMG => on_app_mdimg(hwnd, lparam),
        WM_APP_PDFDOC => on_app_pdfdoc(hwnd, lparam),
        WM_APP_PDFTILE => on_app_pdftile(hwnd, lparam),
        WM_APP_PDFSTRIP => on_app_pdfstrip(hwnd, lparam),
        WM_APP_PDFTEXT => on_app_pdftext(hwnd, lparam),
        WM_APP_PDFINFO => on_app_pdfinfo(hwnd, lparam),
        m if m == super::super::video::WM_APP_VIDEO => {
            on_video_event(hwnd, wparam.0 as u32);
            LRESULT(0)
        }
        WM_APP_SWITCH => on_app_switch(hwnd, lparam),
        WM_APP_LOAD_RESOLVED => {
            on_app_load_resolved(hwnd, lparam);
            LRESULT(0)
        }
        _ => return None,
    })
}

/// Mouse input over the content pane: movement, buttons, wheel, cursor. `None` when `msg`
/// isn't one of these.
pub(super) unsafe fn on_mouse_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    Some(match msg {
        WM_MOUSEMOVE => on_mousemove(hwnd, lparam),
        WM_MOUSELEAVE => on_mouseleave(hwnd),
        WM_LBUTTONDOWN => on_lbuttondown(hwnd, lparam),
        WM_LBUTTONUP => on_lbuttonup(hwnd, lparam),
        WM_CAPTURECHANGED => on_capturechanged(hwnd),
        WM_SETCURSOR => on_setcursor(hwnd, wparam, lparam),
        WM_LBUTTONDBLCLK => on_lbuttondblclk(hwnd, lparam),
        WM_MOUSEWHEEL => on_mousewheel(hwnd, wparam, lparam),
        _ => return None,
    })
}

/// Keyboard input, activation, and the remaining lifecycle/IPC messages. `None` when `msg`
/// isn't one of these.
pub(super) unsafe fn on_key_and_lifecycle_msg(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> Option<LRESULT> {
    Some(match msg {
        WM_ACTIVATE => on_activate(hwnd, wparam),
        WM_KEYDOWN => on_keydown(hwnd, wparam, lparam),
        WM_CHAR => {
            // Only the find bar consumes typed characters; everything else falls through so
            // nothing else in the viewer changes behaviour.
            if super::super::find::on_char(hwnd, wparam.0 as u32) {
                return Some(LRESULT(0));
            }
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_COPYDATA => {
            on_command(hwnd, lparam);
            LRESULT(1)
        }
        WM_DESTROY => on_destroy(hwnd),
        _ => return None,
    })
}

/// `WM_NCHITTEST`: native thick frame handles resize; make the caption strip draggable.
pub(super) unsafe fn on_nchittest(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let hit = DefWindowProcW(hwnd, WM_NCHITTEST, wparam, lparam);
    if hit.0 == HTCLIENT as isize {
        let (sx, sy) = lparam_xy(lparam);
        let mut pt = POINT { x: sx, y: sy };
        let _ = ScreenToClient(hwnd, &mut pt);
        let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
        if pt.y < cap && hit_button(hwnd, pt.x, pt.y).is_none() {
            return LRESULT(HTCAPTION as isize);
        }
    }
    hit
}

/// `WM_TIMER`: the show-fallback, scrub-strip, animation-frame and outline-slide ticks.
pub(super) unsafe fn on_timer(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    if wparam.0 == SHOW_TIMER_ID {
        let _ = KillTimer(Some(hwnd), SHOW_TIMER_ID);
        let st = state(hwnd);
        if !st.is_null() && !(*st).shown.get() {
            ensure_shown(hwnd);
        }
    } else if wparam.0 == SCRUB_TIMER_ID {
        let st = &*state(hwnd);
        if st.kind.get() == ContentKind::Video {
            // repaint ONLY the strip (never the video child) so the tick can't flicker
            let sr = scrub_rect(hwnd);
            let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
        }
    } else if wparam.0 == ANIM_TIMER_ID {
        advance_frame(hwnd);
    } else if wparam.0 == TOC_TIMER_ID {
        tick_toc_anim(hwnd);
    }
    LRESULT(0)
}

/// Log one debug line when a UI-thread pipeline stage (`apply_resolved`, the render post-back
/// handler `on_render`) took longer than [`sagethumbs2k_core::safety::PREVIEW_UI_STAGE_BUDGET`]:
/// see that constant's doc comment for the whole responsiveness contract this is part of
/// (audit E02, 2026-09-07). Diagnostic only: the stage has already run to completion by the
/// time this is called, nothing is aborted or retried. A stage that PUMPS the message loop
/// (WebView2 creation) is not a stall by definition: callers must exclude that route from
/// timing rather than rely on this to filter it out.
///
/// Takes the path as an owned `&str`, never a `&ViewerState`: a stage that can pump/destroy the
/// window may have freed the state by the time this runs, so the caller must capture whatever it
/// needs from `st` before making that call, not after.
pub(super) fn log_ui_stage_stall(stage: &str, elapsed: std::time::Duration, gen: u64, path: &str) {
    if let Some(line) = sagethumbs2k_core::safety::stage_stall_report(
        stage,
        elapsed,
        sagethumbs2k_core::safety::PREVIEW_UI_STAGE_BUDGET,
        gen,
        path,
    ) {
        sagethumbs2k_core::safety::log_debug(&line);
    }
}

/// Rate-limited debug line for an abandoned decode/prepare worker, shared by
/// `loader::abandon_pending_prepare` and `content::abandoned_logged` (audit E02, 2026-09-07:
/// abandoned work must stay OBSERVABLE, not just bounded). A held arrow key can abandon a worker
/// on every repeat (the mash bench's exact case, see `content::bench_abandoned_count`, which
/// stays an EXACT, un-rate-limited counter: only this log line is throttled), and logging every
/// single one would flood the diagnostics log for no extra signal, so this logs at most once per
/// `ABANDON_LOG_WINDOW`. `context` names which worker gave up, for the log line only.
pub(in super::super) fn log_abandoned_worker(context: &str) {
    const ABANDON_LOG_WINDOW: std::time::Duration = std::time::Duration::from_millis(250);
    static LAST: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    let now = std::time::Instant::now();
    // A short, explicit lock scope, never held across the `log_debug` call below (see
    // docs/DEVELOPMENT_GOTCHAS.md, "`if let Some(x) = *MUTEX.lock()` holds the lock for the
    // whole body": this copies the decision out and drops the guard before doing anything else).
    let should_log = {
        let mut last = LAST
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let due = match *last {
            Some(prev) => now.duration_since(prev) >= ABANDON_LOG_WINDOW,
            None => true,
        };
        if due {
            *last = Some(now);
        }
        due
    };
    if !should_log {
        return;
    }
    sagethumbs2k_core::safety::log_debugf!(
        "preview {context}: abandoned a worker ({} of {} abandoned-worker budget slots live)",
        sagethumbs2k_core::safety::abandoned_workers(),
        sagethumbs2k_core::safety::MAX_ABANDONED_WORKERS,
    );
}

/// `WM_SIZE`: free the stale-sized back-buffer, re-place child windows, clamp scroll.
pub(super) unsafe fn on_size(hwnd: HWND) -> LRESULT {
    let st = &*state(hwnd);
    // The cached back-buffer bitmap was sized to the OLD client rect; keeping it
    // would blit stale-size content (or a mismatched BitBlt) on the very next paint.
    // Free it now so `paint::ensure_back_buffer` allocates fresh at the new size.
    free_back_buffer(st);
    if let Some(p) = st.video.borrow().as_ref() {
        p.place(&video_rect(hwnd)); // child fills content minus the scrub strip
    }
    #[cfg(feature = "html-preview")]
    if let Some(w) = st.webview.borrow().as_ref() {
        w.place(&content_rect(hwnd)); // webview fills the content area
    }
    // The visible height changed. Clamp immediately using the last measured document
    // height; the next paint clamps once more if Markdown reflow changes that height.
    let _ = clamp_text_scroll(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
    LRESULT(0)
}

/// `WM_APP_SWITCH`: the follow-selection poll saw a new selection.
pub(super) unsafe fn on_app_switch(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    // The follow-selection poll saw a new selection: switch to it (unless it's
    // already what we're showing).
    let path = *Box::from_raw(lparam.0 as *mut String);
    let st = &*state(hwnd);
    if st.path.borrow().as_deref() != Some(path.as_str()) {
        request_load(hwnd, &path);
    }
    LRESULT(0)
}

/// `WM_ACTIVATE`: close-on-focus-loss (opt-in setting; never when pinned; not during the
/// open grace so a just-shown, never-activated window can't self-close).
pub(super) unsafe fn on_activate(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let st = &*state(hwnd);
    if (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE
        && !st.pinned.get()
        && GetTickCount64().saturating_sub(st.born.get()) >= SETTLE_CLOSE_MS
        && sagethumbs2k_core::settings::preview_close_on_focus_loss()
    {
        request_close(hwnd);
    }
    LRESULT(0)
}

/// `WM_DESTROY`: tear down GDI+, the tooltip control, the back buffer, and free `ViewerState`.
pub(super) unsafe fn on_destroy(hwnd: HWND) -> LRESULT {
    let tok = GDIP_TOKEN.with(|t| t.replace(0));
    if tok != 0 {
        crate::gdip::shutdown(tok);
    }
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ViewerState;
    if !ptr.is_null() {
        // Close is a CANCEL, not silence (audit E02, 2026-09-07): a decode/prepare worker still
        // running for this window must be told nobody is waiting, the same way switching files
        // already tells it via `reset_viewer_state`. Bumping the generation trips
        // `content::abandoned`'s check for any in-flight decode worker; giving up the pending
        // prepare ticket counts it against the abandoned-worker budget like any other
        // abandonment. Without this a worker orphaned by closing the window was neither told
        // nor counted: it just kept running unseen until it finished or hit its own budget.
        let next_gen = (*ptr).decode_gen.get() + 1;
        (*ptr).decode_gen.set(next_gen);
        content::begin_generation(next_gen);
        abandon_pending_prepare();
        let tip = (*ptr).tip.get();
        if !tip.is_invalid() {
            let _ = DestroyWindow(tip); // owned popup; destroy before the state frees
        }
        free_back_buffer(&*ptr); // release the cached WM_PAINT double-buffer GDI handles
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(Box::from_raw(ptr)); // frees RenderData (HBITMAP) + InfoCard (HICON)
    }
    PostQuitMessage(0);
    LRESULT(0)
}
