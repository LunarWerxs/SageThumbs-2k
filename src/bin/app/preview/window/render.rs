//! A decode or a frame arriving: render results, animation frames and video events.

use super::*;

/// React to a Media Foundation engine event. The player itself only ever REPORTS (it is borrowed
/// while it runs), so the two events that need the viewer to act are handled here.
pub(super) unsafe fn on_video_event(hwnd: HWND, event: u32) {
    let st = &*state(hwnd);
    let Some((what, dims)) = player_event(st, event) else {
        return;
    };
    match what {
        super::super::video::VideoEvent::Metadata => {
            // The clip's REAL size (rotation applied) is finally known. Until now the window was a
            // placeholder 16:9 shell, so a portrait phone clip sat letterboxed inside it. Re-size to
            // the true aspect, but never when the user has already dragged their own size or gone
            // full-screen: `client_size` honours both, so simply re-running it is the whole check.
            if dims.is_none() || st.fullscreen.get().is_some() {
                return;
            }
            st.video_dims.set(dims);
            let (cw, ch) = client_size(hwnd);
            place(hwnd, cw, ch, None);
        }
        super::super::video::VideoEvent::Error => {
            // The source opened but cannot actually be decoded, so nothing will ever appear on the
            // render child. Drop the engine (its Drop destroys the child window) and fall back to
            // the still-frame path, which is the same fallback `loader::load` uses when the engine
            // refuses the file outright.
            let _ = KillTimer(Some(hwnd), SCRUB_TIMER_ID);
            *st.video.borrow_mut() = None;
            st.video_dims.set(None);
            if let Some(p) = st.path.borrow().as_ref().cloned() {
                st.kind.set(ContentKind::Loading);
                content::spawn_decode(hwnd, p, st.decode_gen.get());
            }
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
        super::super::video::VideoEvent::None => {}
    }
}

/// Ask the borrowed player what `event` was, reading the clip's real size when metadata just arrived.
unsafe fn player_event(
    st: &ViewerState,
    event: u32,
) -> Option<(super::super::video::VideoEvent, Option<(i32, i32)>)> {
    let vb = st.video.borrow();
    match vb.as_ref() {
        Some(p) => {
            let what = p.on_event(event); // CANPLAY -> autoplay, etc.
            let dims = if what == super::super::video::VideoEvent::Metadata {
                p.native_size()
            } else {
                None
            };
            Some((what, dims))
        }
        None => None,
    }
}

/// Handle a decode result: install the image (or fall back to an InfoCard on failure), then
/// size + show / resize.
pub(super) unsafe fn on_render(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) {
    // MUST stay `content::SharedRgba` — this is a hand-written cast back from a raw pointer,
    // so a type that disagrees with what `content::post_render` boxed is UB the compiler
    // cannot see. Naming the shared alias (rather than spelling the type out) is what keeps
    // the two ends in step.
    let boxed = Box::from_raw(lparam.0 as *mut (u64, Option<content::SharedRgba>));
    let (gen, decoded) = *boxed;
    let st = &*state(hwnd);
    if gen != st.decode_gen.get() {
        return; // stale — the user already switched files
    }
    let _ = wparam;
    // Cloned up front, same discipline as `on_app_load_resolved`: nothing here currently pumps
    // the message loop, but the stage timer must never read `st` after a call that might.
    let path = st.path.borrow().clone().unwrap_or_default();
    let stage_start = std::time::Instant::now();
    // A decode landing while the kind is STILL Video is the audio cover art (loader only asks for
    // one in that case, and the video fallback path sets Loading before it asks). It is a backdrop,
    // not the content: install it into `art`, leave the kind alone, and never fall back to the card
    // for it — a track with no embedded picture just keeps the plain dark surface.
    if st.kind.get() == ContentKind::Video {
        if let Some(d) = decoded {
            if let Some(hbmp) = content::make_dib(d.w, d.h, &d.rgba, letterbox_bg(st)) {
                *st.art.borrow_mut() = Some(RenderData::opaque(hbmp, d.w, d.h));
            }
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    } else {
        install_still_image(hwnd, st, decoded);
    }
    log_ui_stage_stall("render", stage_start.elapsed(), gen, &path);
}

/// Install a decoded still frame (or fall back to the InfoCard) and then show + repaint it.
unsafe fn install_still_image(hwnd: HWND, st: &ViewerState, decoded: Option<content::SharedRgba>) {
    match decoded {
        Some(d) => match content::make_render_for(&d, letterbox_bg(st)) {
            Some(rd) => {
                // A full-resolution decode landing clears any pending request for one,
                // whether this IS that decode or the user simply navigated to a small image.
                if d.is_full() {
                    st.full_pending.set(false);
                }
                *st.render.borrow_mut() = Some(rd);
                st.kind.set(ContentKind::Image);
            }
            // A successful DECODE that then fails to become a DIB (e.g. CreateDIBSection
            // under memory pressure) must not orphan a valid image already on screen,
            // mirrors the None-decode guard just below rather than falling to InfoCard.
            None if st.render.borrow().is_some() => st.full_pending.set(false),
            None => fallback_card(st),
        },
        // A failed decode must never REPLACE a picture that is already on screen. That only
        // became reachable once the fit view started being served by a scaled decode: a
        // subsequent full-resolution fetch can fail (a file deleted mid-zoom, a format the
        // scaled path opened and the buffered one refuses) and swapping the visible image
        // for an error card would be a plain downgrade. With nothing installed yet, the card
        // is still the right answer.
        None if st.render.borrow().is_some() => st.full_pending.set(false),
        None => fallback_card(st), // decode failure / timeout → the calm card
    }
    ensure_shown(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Fetch the real pixels if the zoom has outgrown the codec-scaled ones the fit view is served
/// from. A no-op — one comparison — for a full-resolution render and for any un-zoomed image.
///
/// Called from the paint path rather than from the zoom handlers, so a window resize, a
/// full-screen toggle and a wheel notch are all covered by the same check instead of three that
/// have to be kept in step. `full_pending` is what stops the repaint that follows from asking
/// again before the first answer arrives.
pub(in super::super) unsafe fn ensure_full_for_zoom(hwnd: HWND, rc: &RECT) {
    let st = &*state(hwnd);
    if st.full_pending.get() || st.kind.get() != ContentKind::Image {
        return;
    }
    let wanted = st
        .render
        .borrow()
        .as_ref()
        .is_some_and(|rd| content::wants_full_resolution(rd, rc, st.zoom.get()));
    if !wanted {
        return;
    }
    let Some(path) = st.path.borrow().as_ref().cloned() else {
        return;
    };
    st.full_pending.set(true);
    content::spawn_decode_full(hwnd, path, st.decode_gen.get());
}

/// Fall back to the InfoCard for the current path (decode failed or timed out).
pub(super) unsafe fn fallback_card(st: &ViewerState) {
    if let Some(p) = st.path.borrow().as_ref() {
        *st.card.borrow_mut() = Some(infocard::gather(p));
    }
    st.kind.set(ContentKind::InfoCard);
}

/// Install the decoded animation frames (build one DIB per frame) and start the frame timer.
pub(super) unsafe fn on_anim(hwnd: HWND, lparam: LPARAM) {
    let boxed = Box::from_raw(lparam.0 as *mut (u64, Vec<(content::DecodedRgba, u32)>));
    let (gen, frames_in) = *boxed;
    let st = &*state(hwnd);
    if gen != st.decode_gen.get() {
        return; // stale — the user already switched files
    }
    let bg = letterbox_bg(st);
    let mut rds: Vec<RenderData> = Vec::with_capacity(frames_in.len());
    let mut delays: Vec<u32> = Vec::with_capacity(frames_in.len());
    for (d, ms) in frames_in {
        if let Some(rd) = content::make_render(d.w, d.h, &d.rgba, bg) {
            rds.push(rd);
            delays.push(ms);
        }
    }
    if rds.len() < 2 {
        // couldn't build enough frames → fall through to a normal single-frame decode
        if let Some(p) = st.path.borrow().as_ref().cloned() {
            // `spawn_decode_full`, NOT `spawn_decode`. `spawn_decode` re-detects the animated
            // extension and re-runs the frame decode, which yields the same frame list that has
            // just failed to become bitmaps - so it posts `WM_APP_ANIM` again, lands back here,
            // and retries forever (a fresh thread and a full re-read every cycle) with the
            // window stuck on "Loading". `spawn_decode_full` skips the animation branch
            // entirely, which is exactly the single-frame fallback this arm promises.
            content::spawn_decode_full(hwnd, p, gen);
        }
        return;
    }
    let first = delays[0];
    *st.frames.borrow_mut() = rds;
    *st.frame_delays.borrow_mut() = delays;
    st.cur_frame.set(0);
    st.kind.set(ContentKind::Image);
    ensure_shown(hwnd);
    SetTimer(Some(hwnd), ANIM_TIMER_ID, first, None);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Advance to the next animation frame, re-arm the timer to that frame's delay, repaint content.
pub(super) unsafe fn advance_frame(hwnd: HWND) {
    let st = &*state(hwnd);
    let n = st.frames.borrow().len();
    if n < 2 {
        let _ = KillTimer(Some(hwnd), ANIM_TIMER_ID);
        return;
    }
    let next = (st.cur_frame.get() + 1) % n;
    st.cur_frame.set(next);
    let delay = st.frame_delays.borrow().get(next).copied().unwrap_or(80);
    SetTimer(Some(hwnd), ANIM_TIMER_ID, delay, None);
    let cr = content_rect(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&cr), false);
}
