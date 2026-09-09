//! Mouse input over the content pane and caption: movement, buttons, drags, the wheel, and
//! Markdown link/outline hit-testing.
//!
//! Parent-hub split (2026-09-08): moved out of `window.rs` (pure move, `pub(super)` only on
//! what `window.rs`'s dispatch and its `tests` module still call by name).

use super::*;

/// The link URL (if any) under the client-space point, from the last Markdown paint. Only
/// Markdown content records link rects.
pub(super) unsafe fn hit_link(hwnd: HWND, x: i32, y: i32) -> Option<String> {
    let st = &*state(hwnd);
    if st.kind.get() != ContentKind::Markdown {
        return None;
    }
    st.md_links
        .borrow()
        .iter()
        .find(|h| x >= h.rect.left && x < h.rect.right && y >= h.rect.top && y < h.rect.bottom)
        .map(|h| h.url.clone())
}

/// The outline-sidebar entry index (if any) under the client-space point, from the last paint.
pub(super) unsafe fn hit_toc(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    let st = &*state(hwnd);
    if st.kind.get() != ContentKind::Markdown {
        return None;
    }
    st.toc_hits
        .borrow()
        .iter()
        .find(|(r, _)| x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        .map(|(_, idx)| *idx)
}

/// What one wheel notch means over a continuously scrolled PDF.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum WheelAction {
    /// Move the document up or down (the bare wheel).
    Scroll,
    /// Magnify and re-render (Ctrl).
    Zoom,
    /// Slide a zoomed page sideways (Shift).
    Pan,
}

/// Route the wheel over a scrolled PDF. Pure, and split out of `WM_MOUSEWHEEL` for exactly the
/// reason [`nav_key_action`] is: 2.3.1 shipped with the wheel doing NOTHING over a PDF because
/// the routing lived inside the wndproc where no test could see it, the fall-through landed on
/// `zoom_at_cursor` (which drives state the tiled paint never reads), and every test I had
/// drove the keyboard or called the scroll function directly. A pure function makes the
/// decision assertable; the tests below would have failed on the shipped build.
pub(super) fn pdf_wheel_action(ctrl: bool, shift: bool) -> WheelAction {
    if ctrl {
        WheelAction::Zoom
    } else if shift {
        WheelAction::Pan
    } else {
        WheelAction::Scroll
    }
}

/// `WM_MOUSEMOVE`: an active drag claims the move outright; otherwise it's hover tracking.
pub(super) unsafe fn on_mousemove(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let (x, y) = lparam_xy(lparam);
    let st = &*state(hwnd);
    if let Some(r) = mousemove_drag(hwnd, st, x, y) {
        return r;
    }
    mousemove_hover(hwnd, st, x, y)
}

/// Any active drag (scrollbar thumb, a held scrollbar-track click, video seek/volume, text
/// selection, image pan) claims the move entirely: `Some` means the caller must not fall
/// through to hover tracking. Split out of `on_mousemove` because these five drags dominated
/// the original arm's complexity and share nothing but `hwnd`/`x`/`y`.
unsafe fn mousemove_drag(hwnd: HWND, st: &ViewerState, x: i32, y: i32) -> Option<LRESULT> {
    // Active drag of the custom text/Markdown scrollbar thumb.
    if let Some(grab_y) = st.scroll_drag.get() {
        drag_text_scroll_thumb(hwnd, y, grab_y);
        return Some(LRESULT(0));
    }
    // A track click captures until button-up so it cannot turn into a content click
    // if the pointer moves away. Native auto-repeat is intentionally not emulated.
    if st.scroll_page_press.get() {
        let _ = set_scroll_hot(hwnd, hit_text_scrollbar(hwnd, x, y).is_some());
        return Some(LRESULT(0));
    }
    // Active seek / volume drag on the video strip.
    if st.scrub_drag.get() || st.vol_drag.get() {
        let sr = scrub_rect(hwnd);
        let p = scrub_parts(hwnd, &sr);
        if let Some(v) = st.video.borrow().as_ref() {
            if st.scrub_drag.get() {
                apply_seek(v, x, &p.track);
            } else {
                apply_vol(v, x, &p.vol);
            }
        }
        let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
        return Some(LRESULT(0));
    }
    // Active text-selection drag: extend to the cursor, auto-scrolling past the
    // pane edges so a drag can select beyond the viewport. Hit-test BEFORE
    // scrolling — the offset must match the frame the user is looking at (and the
    // Markdown rects are from that paint); the next move picks up the new scroll.
    if st.sel_drag.get() {
        if let Some(off) = selection::hit(hwnd, x, y) {
            if let Some((a, _)) = st.sel.get() {
                st.sel.set(Some((a, off)));
            }
        }
        let c = content_rect(hwnd);
        let overshoot = if y < c.top {
            y - c.top
        } else if y > c.bottom {
            y - c.bottom
        } else {
            0
        };
        if overshoot != 0 {
            let step_cap = crate::win::dpi_scale(hwnd, 40);
            selection::scroll_by(hwnd, overshoot.clamp(-step_cap, step_cap));
        }
        let _ = InvalidateRect(Some(hwnd), Some(&c), false);
        return Some(LRESULT(0));
    }
    // Active pan drag: move the image with the cursor.
    if let Some((ax, ay, apx, apy)) = st.drag.get() {
        st.pan.set((apx + (x - ax), apy + (y - ay)));
        clamp_pan(hwnd);
        let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        r.top = cap;
        let _ = InvalidateRect(Some(hwnd), Some(&r), false);
        return Some(LRESULT(0));
    }
    None
}

/// Toolbar-button hover + custom-scrollbar hover feedback, and arming `TrackMouseEvent` so
/// `WM_MOUSELEAVE` fires when the pointer leaves either. Reached only when no drag claimed
/// the move (see `mousemove_drag`).
unsafe fn mousemove_hover(hwnd: HWND, st: &ViewerState, x: i32, y: i32) -> LRESULT {
    let now = hit_button(hwnd, x, y);
    let button_changed = now != st.hot.get();
    if button_changed {
        st.hot.set(now);
        let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        r.bottom = cap;
        let _ = InvalidateRect(Some(hwnd), Some(&r), false);
    }
    let scroll_changed = set_scroll_hot(hwnd, hit_text_scrollbar(hwnd, x, y).is_some());
    if button_changed || scroll_changed {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: core::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        let _ = TrackMouseEvent(&mut tme);
    }
    LRESULT(0)
}

/// `WM_MOUSELEAVE`: clear the hot button + scrollbar hover state.
pub(super) unsafe fn on_mouseleave(hwnd: HWND) -> LRESULT {
    let st = &*state(hwnd);
    if st.hot.get().is_some() {
        st.hot.set(None);
        let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
        let mut r = RECT::default();
        let _ = GetClientRect(hwnd, &mut r);
        r.bottom = cap;
        let _ = InvalidateRect(Some(hwnd), Some(&r), false);
    }
    let _ = set_scroll_hot(hwnd, false);
    LRESULT(0)
}

/// `WM_LBUTTONDOWN`: a toolbar button, a PDF strip thumbnail, or something in the content pane.
pub(super) unsafe fn on_lbuttondown(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let (x, y) = lparam_xy(lparam);
    let st = &*state(hwnd);
    if st.focus.get().is_some() {
        // A mouse click always clears keyboard toolbar focus, whatever it lands on — the ring
        // it left behind is stale the instant the mouse takes over.
        set_focus(hwnd, st, None);
    }
    if let Some(i) = hit_button(hwnd, x, y) {
        do_action(hwnd, BTNS[i]);
    } else if crate::preview::pdfview::strip_click(hwnd, x, y) {
        // A page thumbnail was clicked; it already scrolled there.
    } else {
        lbuttondown_pane(hwnd, x, y);
    }
    LRESULT(0)
}

/// A press that landed neither on a toolbar button nor a PDF strip thumbnail: the custom
/// text scrollbar, the video transport strip, an image pan (when zoomed), or the start of a
/// text/Markdown selection drag. Split out of `on_lbuttondown`, the original `else` arm was
/// itself a five-way branch and the biggest piece of that message's complexity.
unsafe fn lbuttondown_pane(hwnd: HWND, x: i32, y: i32) {
    let st = &*state(hwnd);
    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
    if let Some(hit) = hit_text_scrollbar(hwnd, x, y) {
        let _ = set_scroll_hot(hwnd, true);
        match hit {
            TextScrollHit::Thumb(grab_y) => {
                // The thumb is owner-drawn, so explicitly capture the mouse and
                // map subsequent pointer movement back to the document range.
                st.scroll_drag.set(Some(grab_y));
            }
            TextScrollHit::Page(dy) => {
                let _ = scroll_text_by(hwnd, dy);
                st.scroll_page_press.set(true);
            }
        }
        invalidate_text_scrollbar(hwnd); // pressed feedback
        let _ = SetCapture(hwnd);
    } else if st.kind.get() == ContentKind::Video {
        scrub_mouse_down(hwnd, x, y);
    } else if y >= cap && st.kind.get() == ContentKind::Image && st.zoom.get() > 1.0 {
        // In the content area, over a zoomed image → begin a pan drag.
        let (px, py) = st.pan.get();
        st.drag.set(Some((x, y, px, py)));
        let _ = SetCapture(hwnd);
    } else if y >= cap && selection::selectable(st.kind.get()) && hit_toc(hwnd, x, y).is_none() {
        // In a text/Markdown pane (not the outline sidebar) → begin a selection
        // drag, anchored at the hit. A drag starting on a Markdown link is fine:
        // the link only opens if the button comes up with nothing selected.
        if let Some(off) = selection::hit(hwnd, x, y) {
            st.sel.set(Some((off, off)));
            st.sel_drag.set(true);
            let _ = SetCapture(hwnd);
            let cr = content_rect(hwnd);
            let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
        }
    }
}

/// `WM_LBUTTONUP`: end whichever drag was active, or treat a plain click.
pub(super) unsafe fn on_lbuttonup(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let st = &*state(hwnd);
    if st.scroll_drag.get().is_some() || st.scroll_page_press.get() {
        st.scroll_drag.set(None);
        st.scroll_page_press.set(false);
        let _ = ReleaseCapture();
        let (x, y) = lparam_xy(lparam);
        let _ = set_scroll_hot(hwnd, hit_text_scrollbar(hwnd, x, y).is_some());
        invalidate_text_scrollbar(hwnd); // pressed → hover/idle feedback
    } else if st.scrub_drag.get() || st.vol_drag.get() {
        let was_vol = st.vol_drag.get();
        st.scrub_drag.set(false);
        st.vol_drag.set(false);
        let _ = ReleaseCapture();
        // Slider let go: remember the level ONCE, not on every mouse-move of the drag.
        if was_vol {
            if let Some(v) = st.video.borrow().as_ref() {
                persist_volume(v);
            }
        }
    } else if st.drag.get().is_some() {
        st.drag.set(None);
        let _ = ReleaseCapture();
    } else if st.sel_drag.get() {
        st.sel_drag.set(false);
        let _ = ReleaseCapture();
        // Nothing was dragged out (anchor == focus): that's a plain CLICK — drop any
        // old selection and let it act like one (outline jump / link open).
        if matches!(st.sel.get(), Some((a, b)) if a == b) {
            st.sel.set(None);
            let (x, y) = lparam_xy(lparam);
            click_content(hwnd, x, y);
            let cr = content_rect(hwnd);
            let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
        }
    } else {
        let (x, y) = lparam_xy(lparam);
        click_content(hwnd, x, y);
    }
    LRESULT(0)
}

/// `WM_CAPTURECHANGED`: capture stolen mid-drag (alt-tab, another SetCapture), end every
/// drag so a buttonless mouse-move can't keep seeking/panning/selecting.
pub(super) unsafe fn on_capturechanged(hwnd: HWND) -> LRESULT {
    let st = &*state(hwnd);
    let scrollbar_was_pressed = st.scroll_drag.get().is_some() || st.scroll_page_press.get();
    st.drag.set(None);
    st.scroll_drag.set(None);
    st.scroll_page_press.set(false);
    st.scrub_drag.set(false);
    st.vol_drag.set(false);
    st.sel_drag.set(false);
    let _ = set_scroll_hot(hwnd, false);
    if scrollbar_was_pressed {
        invalidate_text_scrollbar(hwnd);
    }
    LRESULT(0)
}

/// `WM_SETCURSOR`: hand cursor over a Markdown link, I-beam over selectable text; otherwise
/// default handling so the resize border + caption keep their sizing/move cursors.
pub(super) unsafe fn on_setcursor(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if (lparam.0 & 0xFFFF) as i32 == HTCLIENT as i32 {
        let st = &*state(hwnd);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = ScreenToClient(hwnd, &mut pt);
        // Keep the standard arrow over the scrollbar instead of presenting the
        // text-selection I-beam, which made the painted thumb look non-interactive.
        if st.scroll_drag.get().is_some()
            || st.scroll_page_press.get()
            || hit_text_scrollbar(hwnd, pt.x, pt.y).is_some()
        {
            if let Ok(arrow) = LoadCursorW(None, IDC_ARROW) {
                SetCursor(Some(arrow));
            }
            return LRESULT(1);
        }
        if st.kind.get() == ContentKind::Markdown
            && (hit_link(hwnd, pt.x, pt.y).is_some() || hit_toc(hwnd, pt.x, pt.y).is_some())
        {
            if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                SetCursor(Some(hand));
            }
            return LRESULT(1);
        }
        if selection::selectable(st.kind.get())
            && pt.y >= crate::win::dpi_scale(hwnd, CAPTION_H)
            && hit_toc(hwnd, pt.x, pt.y).is_none()
        {
            if let Ok(ibeam) = LoadCursorW(None, IDC_IBEAM) {
                SetCursor(Some(ibeam));
            }
            return LRESULT(1);
        }
    }
    DefWindowProcW(hwnd, WM_SETCURSOR, wparam, lparam)
}

/// `WM_LBUTTONDBLCLK`: double-click content = toggle fit/100%; double-click text = select word.
pub(super) unsafe fn on_lbuttondblclk(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let (x, y) = lparam_xy(lparam);
    let st = &*state(hwnd);
    let cap = crate::win::dpi_scale(hwnd, CAPTION_H);
    if hit_text_scrollbar(hwnd, x, y).is_some() {
        // A double-click on the scrollbar must not select the document text beneath it.
    } else if y >= cap && st.kind.get() == ContentKind::Image && hit_button(hwnd, x, y).is_none() {
        toggle_fit_100(hwnd); // double-click content → toggle fit / 100%
    } else if y >= cap && selection::selectable(st.kind.get()) && hit_toc(hwnd, x, y).is_none() {
        // Double-click in a text/Markdown pane → select the word under the cursor.
        // Claiming the drag (capture + flag) keeps the button-up that follows from
        // being read as a click — which would open a double-clicked link.
        if let Some((a, b)) =
            selection::hit(hwnd, x, y).and_then(|o| selection::word_range(hwnd, o))
        {
            st.sel.set(Some((a, b)));
            st.sel_drag.set(true);
            let _ = SetCapture(hwnd);
            let cr = content_rect(hwnd);
            let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
        }
    }
    LRESULT(0)
}

/// `WM_MOUSEWHEEL`: scroll/zoom/pan a PDF, zoom an image, scroll text, or nudge video volume/seek.
pub(super) unsafe fn on_mousewheel(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // GET_WHEEL_DELTA_WPARAM (signed high word).
    let delta = ((wparam.0 >> 16) & 0xFFFF) as i16 as i32;
    let st = &*state(hwnd);
    match st.kind.get() {
        // A continuously scrolled PDF takes the wheel for SCROLLING, which is what
        // the wheel means in every document reader; Ctrl+wheel magnifies and
        // Shift+wheel slides a zoomed page sideways.
        //
        // 2.3.1 SHIPPED WITHOUT THIS. The continuous view landed with the keyboard
        // wired up and the wheel still falling through to `zoom_at_cursor`, which
        // drives `st.zoom`/`st.pan` on a single `RenderData` that the tiled paint
        // path never reads - so the wheel did precisely nothing over a PDF while
        // the release notes said it scrolled. Arrow keys worked, which is exactly
        // why the tests I had (key-driven navigation, and a shot that calls the
        // scroll function directly) all passed. Test the INPUT PATH, not the thing
        // it calls.
        ContentKind::Image if crate::preview::pdfview::active(hwnd) => {
            // Three lines a notch, the same step the text pane uses.
            let step = -delta * crate::win::dpi_scale(hwnd, 54) / 120;
            match pdf_wheel_action(
                GetKeyState(VK_CONTROL.0 as i32) < 0,
                GetKeyState(VK_SHIFT.0 as i32) < 0,
            ) {
                WheelAction::Zoom => {
                    crate::preview::pdfview::zoom_by(hwnd, f64::from(delta) / 120.0);
                }
                WheelAction::Pan => {
                    crate::preview::pdfview::pan_by(hwnd, step);
                }
                WheelAction::Scroll => {
                    crate::preview::pdfview::scroll_by(hwnd, step);
                }
            }
        }
        ContentKind::Image => zoom_at_cursor(hwnd, delta, lparam),
        ContentKind::Text | ContentKind::Markdown => scroll_text(hwnd, delta),
        // A244: the wheel was dead over video/audio content — every other media
        // player uses it for volume, with Ctrl+wheel for seek. Reuses the SAME
        // relative-step helpers the transport's arrow-key controls already call
        // (`video_key`'s VK_UP/DOWN nudge_volume, VK_LEFT/RIGHT seek_by), not the
        // strip's `apply_vol`/`apply_seek` — those map an absolute click POSITION
        // on the strip, which a wheel notch has none of. Shares `wheel_remainder`
        // with text scrolling (same accumulate-to-a-full-notch reasoning) so a
        // precision trackpad's tiny deltas don't yank the volume on every tick.
        ContentKind::Video => {
            if let Some(v) = st.video.borrow().as_ref() {
                let (notches, remainder) = wheel_notches(st.wheel_remainder.get(), delta);
                st.wheel_remainder.set(remainder);
                if notches != 0 {
                    if GetKeyState(VK_CONTROL.0 as i32) < 0 {
                        v.seek_by(f64::from(notches) * 5.0);
                    } else {
                        v.nudge_volume(f64::from(notches) * 0.05);
                        persist_volume(v);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
            }
        }
        _ => {}
    }
    LRESULT(0)
}
