//! Keyboard input: navigation-key routing, the `WM_KEYDOWN` dispatch cluster, and the
//! video-transport key handling it defers to.
//!
//! Parent-hub split (2026-09-08): moved out of `window.rs` (pure move, `pub(super)` only on
//! what `window.rs`'s dispatch and its `tests` module still call by name).

use super::*;

/// What a bare (unmodified) navigation key means in the viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NavKey {
    /// Flip to a sibling file in the folder.
    File(i32),
    /// Turn a page inside the current multi-page document.
    Page(i32),
}

/// Route one navigation keypress. Pure, and split out of `WM_KEYDOWN` on purpose: the bug it
/// exists to prevent lived inside the wndproc, where no test could reach it.
///
/// **←/→ ALWAYS mean "next / previous FILE", on every kind of content.** They used to mean
/// "next / previous PAGE" while a multi-page PDF was showing, which made such a PDF a keyboard
/// dead end. `goto_pdf_page` clamps at both ends and returns early when the page does not
/// change, nothing fell through to `nav_sibling`, and Home/End are inert on an `Image`, so once
/// the popup landed on a multi-page PDF the only way to reach the next file was to close it and
/// re-open on something else. Every real-world PDF has more than one page and therefore hit
/// this; the corpus's `sample.pdf` has exactly one, which is why it stayed invisible.
///
/// Paging lives on ↑/↓ and PgUp/PgDn instead, matching Quick Look. PgUp/PgDn keep flipping
/// FILES everywhere else, which is what they did before and what the non-PDF viewer expects.
pub(super) fn nav_key_action(multipage_pdf: bool, vk: u16) -> Option<NavKey> {
    if multipage_pdf {
        if vk == VK_NEXT.0 || vk == VK_DOWN.0 {
            return Some(NavKey::Page(1));
        }
        if vk == VK_PRIOR.0 || vk == VK_UP.0 {
            return Some(NavKey::Page(-1));
        }
    }
    if vk == VK_RIGHT.0 || vk == VK_NEXT.0 {
        return Some(NavKey::File(1));
    }
    if vk == VK_LEFT.0 || vk == VK_PRIOR.0 {
        return Some(NavKey::File(-1));
    }
    None
}

/// Whether `vk` is one of the eight navigation keys that extend a selection under Shift
/// (plain arrows, Home/End, Page Up/Down). Split out of the Shift+nav-extend check in
/// `keydown_copy_select` purely because a single `matches!` over eight alternatives was, by
/// itself, most of that check's cyclomatic weight.
fn is_selection_extend_key(vk: u16) -> bool {
    matches!(vk, v if v == VK_LEFT.0 || v == VK_RIGHT.0 || v == VK_UP.0
        || v == VK_DOWN.0 || v == VK_HOME.0 || v == VK_END.0
        || v == VK_PRIOR.0 || v == VK_NEXT.0)
}

/// Ctrl+A / Ctrl+C / Ctrl+U / Ctrl+S: the content-editing quartet — select all, copy,
/// toggle source view, save the shown page/frame. Split out of `keydown_copy_select`
/// purely to keep that dispatcher's own weight under the complexity gate; the four
/// checks are independent early returns, same as they were inline.
unsafe fn keydown_edit_actions(
    hwnd: HWND,
    st: &ViewerState,
    vk: u16,
    ctrl: bool,
    shift: bool,
) -> Option<LRESULT> {
    // Ctrl+A / Ctrl+C: select all / copy the CONTENT (the selection, the rendered
    // text, the info-card text, or the decoded image) — the whole point of a viewer
    // you can lift text out of. Ctrl+Shift+C copies a Markdown file's raw source.
    if ctrl && vk == 'A' as u16 {
        if let Some(len) = selection::doc_len(hwnd) {
            if len > 0 {
                st.sel.set(Some((0, len)));
                let cr = content_rect(hwnd);
                let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
            }
        }
        return Some(LRESULT(0));
    }
    if ctrl && vk == 'C' as u16 {
        copy_content(hwnd, shift);
        return Some(LRESULT(0));
    }
    // Ctrl+U: view source / view rendered — the browser convention, same as the
    // toolbar's `</>` toggle. Ignored on files that only have one view.
    if ctrl && vk == 'U' as u16 {
        toggle_source(hwnd);
        return Some(LRESULT(0));
    }
    // Ctrl+S: save the shown PDF page / animation frame as a PNG — same action as the
    // `Btn::SavePage` toolbar button, self-guarding via `on_btn_save_page` when neither
    // applies (a plain image has nothing to save beyond what Ctrl+C already copies).
    if ctrl && vk == 'S' as u16 {
        do_action(hwnd, Btn::SavePage);
        return Some(LRESULT(0));
    }
    // Ctrl+P: print the shown content — same action as the `Btn::Print` toolbar button,
    // self-guarding via `print::do_print` when the pane has no bitmap to print.
    if ctrl && vk == 'P' as u16 {
        do_action(hwnd, Btn::Print);
        return Some(LRESULT(0));
    }
    None
}

/// Bare W and the Ctrl+=/Ctrl+-/Ctrl+0 keyboard-zoom trio: the image-view-only keys.
/// Split out of `keydown_copy_select` purely to keep that dispatcher's own weight under
/// the complexity gate; the checks below are unchanged from their original inline form.
unsafe fn keydown_image_zoom_keys(
    hwnd: HWND,
    st: &ViewerState,
    vk: u16,
    ctrl: bool,
    shift: bool,
) -> Option<LRESULT> {
    // Bare "W": toggle fit-width vs aspect-fit — the mode a portrait page (a
    // scanned document, a tall screenshot) needs in a landscape-shaped preview
    // window, where aspect-fit leaves empty margins on both sides instead of using
    // the width that's actually there. Sits alongside the double-click
    // aspect-fit/100% toggle above; unmodified because it only ever reaches here
    // when no child control (e.g. the find bar's edit box) has keyboard focus.
    if !ctrl && !shift && vk == 'W' as u16 && st.kind.get() == ContentKind::Image {
        toggle_fit_width(hwnd);
        return Some(LRESULT(0));
    }
    // Ctrl+=/Ctrl+- : keyboard zoom, one wheel notch per press, anchored on the content
    // pane's centre (there is no cursor position to anchor on from the keyboard). Not gated
    // on `!shift`: the `=`/`+` key is the same VK regardless of the Shift needed to type `+`
    // on most layouts, and Ctrl+Shift+= is the same browser-zoom-in convention users already
    // know. Ctrl+0 resets to fit/100%, same as double-click.
    if ctrl && st.kind.get() == ContentKind::Image {
        if vk == VK_OEM_PLUS.0 {
            zoom_step_at_center(hwnd, 1);
            return Some(LRESULT(0));
        }
        if vk == VK_OEM_MINUS.0 {
            zoom_step_at_center(hwnd, -1);
            return Some(LRESULT(0));
        }
        if vk == '0' as u16 {
            toggle_fit_100(hwnd);
            return Some(LRESULT(0));
        }
    }
    None
}

/// Shift+nav selection-extend, Ctrl+F, and an already-open find bar: the search/selection
/// tail of the cluster. Split out of `keydown_copy_select` purely to keep that dispatcher's
/// own weight under the complexity gate; unchanged from the original inline checks.
unsafe fn keydown_find_and_extend(hwnd: HWND, vk: u16, ctrl: bool, shift: bool) -> Option<LRESULT> {
    // Shift+<nav key> extends the selection (plain arrows stay file navigation).
    if shift && is_selection_extend_key(vk) && selection::extend(hwnd, vk, ctrl) {
        return Some(LRESULT(0));
    }
    // Ctrl+F opens the find bar (or steps to the next match if it is already open).
    if ctrl && vk == 'F' as u16 {
        crate::preview::find::toggle(hwnd);
        return Some(LRESULT(0));
    }
    // While the bar is up it owns Esc / Enter / F3. F3 also works with it closed, so a
    // search survives Esc and can be resumed without retyping it.
    if crate::preview::find::on_key(hwnd, vk, shift) {
        return Some(LRESULT(0));
    }
    None
}

/// Ctrl+A / Ctrl+C / Ctrl+U / Ctrl+S / bare W / Ctrl+=/Ctrl+-/Ctrl+0 (image zoom) / Shift+nav /
/// Ctrl+F / an already-open find bar: the "editing and search" cluster of `WM_KEYDOWN`.
/// `Some` means the key was consumed. A thin dispatcher over the three helpers above, tried
/// in the same order this cluster always checked them in.
unsafe fn keydown_copy_select(
    hwnd: HWND,
    st: &ViewerState,
    vk: u16,
    ctrl: bool,
    shift: bool,
) -> Option<LRESULT> {
    if let Some(r) = keydown_edit_actions(hwnd, st, vk, ctrl, shift) {
        return Some(r);
    }
    if let Some(r) = keydown_image_zoom_keys(hwnd, st, vk, ctrl, shift) {
        return Some(r);
    }
    keydown_find_and_extend(hwnd, vk, ctrl, shift)
}

/// The playing-video transport keys, and Home/End over a text/Markdown pane. Both stay
/// early, ahead of the PDF/file navigation cluster, for the same reason they did inside the
/// original arm: a clip owns its own scrub keys, and Home/End must reach the document ends
/// before the generic nav-key routing below gets a chance to misread them.
unsafe fn keydown_video_and_home(
    hwnd: HWND,
    st: &ViewerState,
    vk: u16,
    ctrl: bool,
    shift: bool,
) -> Option<LRESULT> {
    // A playing clip owns the transport keys (seek / volume / pause / mute / loop)
    // BEFORE the generic Home/End and arrow handling below, which would otherwise
    // scroll or flip files while you are trying to scrub.
    if video_key(hwnd, vk, ctrl, shift) {
        return Some(LRESULT(0));
    }
    // Home / End scroll a text or Markdown document to its ends.
    if !shift && (vk == VK_HOME.0 || vk == VK_END.0) && selection::selectable(st.kind.get()) {
        let to = if vk == VK_HOME.0 {
            -st.text_scroll.get()
        } else {
            st.text_h.get()
        };
        selection::scroll_by(hwnd, to);
        return Some(LRESULT(0));
    }
    None
}

/// PDF continuous-view vertical scrolling, then the file/page navigation `nav_key_action`
/// dispatch. Split out on its own because between them a 6-armed match (the PDF viewport
/// step) and the `nav_key_action` match were most of the original arm's remaining weight.
unsafe fn keydown_page_nav(
    hwnd: HWND,
    st: &ViewerState,
    vk: u16,
    ctrl: bool,
    shift: bool,
) -> Option<LRESULT> {
    // A continuously scrolled PDF owns the vertical keys: Up/Down nudge, PgUp/PgDn
    // move a viewport, Home/End jump to the ends. Left/Right are NOT here, and
    // must never be: they stay file navigation on every kind of content.
    if crate::preview::pdfview::active(hwnd) && !ctrl && !shift {
        let line = crate::win::dpi_scale(hwnd, 64);
        let page = crate::preview::pdfview::viewport_step(hwnd);
        let delta = match vk {
            v if v == VK_DOWN.0 => Some(line),
            v if v == VK_UP.0 => Some(-line),
            v if v == VK_NEXT.0 => Some(page),
            v if v == VK_PRIOR.0 => Some(-page),
            v if v == VK_HOME.0 => Some(i32::MIN / 2),
            v if v == VK_END.0 => Some(i32::MAX / 2),
            _ => None,
        };
        if let Some(d) = delta {
            crate::preview::pdfview::scroll_by(hwnd, d);
            return Some(LRESULT(0));
        }
    }
    let multipage_pdf = st.kind.get() == ContentKind::Image && st.pdf_pages.get() > 1;
    match nav_key_action(multipage_pdf, vk) {
        Some(NavKey::Page(delta)) => {
            goto_pdf_page(hwnd, delta);
            Some(LRESULT(0))
        }
        Some(NavKey::File(delta)) => {
            nav_sibling(hwnd, delta);
            Some(LRESULT(0))
        }
        None => None,
    }
}

/// Esc-leaves-fullscreen, then the manual-mode Esc/Space/Enter close. Kept last, matching
/// the original arm's order: everything above gets first refusal at a key before these
/// window-lifecycle defaults apply.
unsafe fn keydown_lifecycle(hwnd: HWND, st: &ViewerState, vk: u16) -> Option<LRESULT> {
    // Esc leaves full-screen first (even when the daemon hook owns lifecycle keys).
    if vk == VK_ESCAPE.0 && st.fullscreen.get().is_some() {
        toggle_fullscreen(hwnd);
        return Some(LRESULT(0));
    }
    // Only own the lifecycle keys when the daemon hook is NOT the authority.
    if st.manual && (vk == VK_ESCAPE.0 || vk == VK_SPACE.0 || vk == VK_RETURN.0) {
        request_close(hwnd);
        return Some(LRESULT(0));
    }
    None
}

/// `WM_KEYDOWN`: thin dispatcher over the four key-handling clusters above, in the same
/// priority order the original single arm checked them in.
pub(super) unsafe fn on_keydown(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let st = &*state(hwnd);
    let vk = wparam.0 as u16;
    // F11 toggles borderless full-screen (works in daemon + manual mode).
    if vk == VK_F11.0 {
        toggle_fullscreen(hwnd);
        return LRESULT(0);
    }
    let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
    let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
    // The toolbar keyboard-focus cluster goes FIRST: while a caption/transport button has
    // focus it must own Left/Right/Up/Down/Enter/Space/Escape ahead of every other cluster
    // below (video seek, PDF/file nav, the manual-mode Esc/Space/Enter close) — see
    // `toolbar::keydown_toolbar_focus`'s doc comment. It is a cheap no-op bail whenever focus
    // is not on the toolbar, so every existing key path is otherwise untouched.
    if let Some(r) = keydown_toolbar_focus(hwnd, st, vk, shift) {
        return r;
    }
    if let Some(r) = keydown_copy_select(hwnd, st, vk, ctrl, shift) {
        return r;
    }
    if let Some(r) = keydown_video_and_home(hwnd, st, vk, ctrl, shift) {
        return r;
    }
    if let Some(r) = keydown_page_nav(hwnd, st, vk, ctrl, shift) {
        return r;
    }
    if let Some(r) = keydown_lifecycle(hwnd, st, vk) {
        return r;
    }
    DefWindowProcW(hwnd, WM_KEYDOWN, wparam, lparam)
}

/// Keyboard control for a playing video or audio track, matching what every media player does.
/// Returns whether the key was consumed.
///
/// `←/→` are the seek keys here rather than folder navigation: while a clip is playing that is what
/// the key means everywhere else, and PgUp/PgDn still flip through the folder, so nothing is lost.
/// Seek keys: `←/→` (step scaled by Ctrl/Shift), Home/End.
unsafe fn video_key_seek(v: &crate::preview::video::VideoPlayer, vk: u16, step: f64) -> bool {
    match vk {
        k if k == VK_LEFT.0 => v.seek_by(-step),
        k if k == VK_RIGHT.0 => v.seek_by(step),
        k if k == VK_HOME.0 => v.seek(0.0),
        k if k == VK_END.0 => {
            let d = v.duration();
            if d.is_finite() && d > 0.0 {
                v.seek((d - 0.1).max(0.0));
            }
        }
        _ => return false,
    }
    true
}

/// Volume keys: `↑/↓` nudge and persist.
unsafe fn video_key_volume(v: &crate::preview::video::VideoPlayer, vk: u16) -> bool {
    match vk {
        k if k == VK_UP.0 => v.nudge_volume(0.05),
        k if k == VK_DOWN.0 => v.nudge_volume(-0.05),
        _ => return false,
    }
    persist_volume(v);
    true
}

/// Toggle keys: play/pause, mute, loop.
unsafe fn video_key_toggle(v: &crate::preview::video::VideoPlayer, vk: u16) -> bool {
    match vk {
        // K and P both pause, because muscle memory splits between YouTube and desktop players.
        // Space is deliberately NOT bound: it belongs to the preview's own open/close lifecycle.
        k if k == 'K' as u16 || k == 'P' as u16 => v.toggle_play(),
        k if k == 'M' as u16 => {
            v.set_muted(!v.muted());
            persist_volume(v);
        }
        k if k == 'L' as u16 => {
            let on = !v.looping();
            v.set_looping(on);
            let _ = sagethumbs2k_core::settings::set_preview_loop(on);
        }
        _ => return false,
    }
    true
}

unsafe fn video_key(hwnd: HWND, vk: u16, ctrl: bool, shift: bool) -> bool {
    let st = &*state(hwnd);
    if st.kind.get() != ContentKind::Video {
        return false;
    }
    let vb = st.video.borrow();
    let Some(v) = vb.as_ref() else { return false };
    // Coarse with Ctrl, fine with Shift, 5 s otherwise.
    let step = if ctrl {
        30.0
    } else if shift {
        1.0
    } else {
        5.0
    };
    // With "arrows switch files" on, ←/→ are NOT ours: fall through to the folder navigation
    // below. Everything else on this map still applies, and the strip's ⏮/⏭ buttons plus
    // PgUp/PgDn mean neither behaviour is ever unreachable.
    if st.arrow_nav.get() && matches!(vk, k if k == VK_LEFT.0 || k == VK_RIGHT.0) {
        return false;
    }
    let consumed =
        video_key_seek(v, vk, step) || video_key_volume(v, vk) || video_key_toggle(v, vk);
    if !consumed {
        return false;
    }
    let sr = scrub_rect(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
    true
}
