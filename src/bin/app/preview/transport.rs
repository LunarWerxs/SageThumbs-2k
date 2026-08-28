//! Video/audio transport strip: seek track + volume slider + time.

use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, FillRect, InvalidateRect,
    LineTo, MoveToEx, SelectObject, SetBkMode, SetTextColor, DT_CENTER, DT_LEFT, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ, PS_SOLID, TRANSPARENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetCapture;

use super::window::{content_rect, state, SCRUB_H};

pub(super) unsafe fn video_rect(hwnd: HWND) -> RECT {
    let mut r = content_rect(hwnd);
    r.bottom -= crate::win::dpi_scale(hwnd, SCRUB_H);
    r
}

/// Video-only: the transport strip's rect (bottom band of the content area).
pub(super) unsafe fn scrub_rect(hwnd: HWND) -> RECT {
    let c = content_rect(hwnd);
    let h = crate::win::dpi_scale(hwnd, SCRUB_H);
    RECT {
        left: c.left,
        top: c.bottom - h,
        right: c.right,
        bottom: c.bottom,
    }
}

/// The clickable sub-rects inside the transport strip (device px), left to right:
/// play/pause square, time label, seek track, speaker glyph, volume slider, loop toggle, speed.
///
/// A named struct rather than a tuple because the strip grew past the point where positional
/// destructuring at four call sites was readable.
pub(super) struct Parts {
    /// Previous / next PREVIEWABLE file in the folder. Always present, so switching clips never
    /// depends on which meaning the arrow keys currently have.
    pub prev: RECT,
    pub next: RECT,
    pub play: RECT,
    pub track: RECT,
    /// Speaker glyph (click = mute toggle), immediately left of the volume slider.
    pub mute: RECT,
    pub vol: RECT,
    /// Repeat toggle (a small circular-arrow glyph).
    pub loopb: RECT,
    /// What ←/→ do while playing: seek, or move between files. Lives HERE rather than in Settings
    /// because it is only ever relevant while you are looking at this window, and a niche
    /// preference buried in a settings dialog is a preference nobody finds.
    pub arrows: RECT,
    /// Playback speed, drawn as its own multiplier text ("1x", "1.5x"); a click cycles the steps.
    pub speed: RECT,
}

/// The strip's clickable controls, in the order [`transport_rects`] reports them. Used for
/// tooltips (the strip is owner-drawn, so every control needs a registered tip rect or it is a
/// mystery glyph) and nothing else; the click dispatch is a straight rect test in
/// [`scrub_mouse_down`].
#[derive(Clone, Copy, PartialEq)]
pub(super) enum TBtn {
    Prev,
    Play,
    Next,
    Mute,
    Loop,
    Arrows,
    Speed,
}

pub(super) const TBTNS: [TBtn; 7] = [
    TBtn::Prev,
    TBtn::Play,
    TBtn::Next,
    TBtn::Mute,
    TBtn::Loop,
    TBtn::Arrows,
    TBtn::Speed,
];

/// Localized tooltip for a transport control, for the state it is CURRENTLY in.
///
/// Same convention as the caption toolbar's [`super::toolbar::btn_tip`]: a toggle's tip names
/// what the click WILL DO. The speaker and the repeat mark both change appearance with their
/// state (crossed-out glyph, accent tint), so a fixed "Mute" / "Repeat" told half the users the
/// opposite of what the button was about to do.
///
/// `Play` needs no pair: its one string already covers both directions.
pub(super) fn tbtn_tip(b: TBtn, muted: bool, looping: bool) -> &'static str {
    crate::win::t(match b {
        TBtn::Prev => "preview_tip_prevfile",
        TBtn::Play => "preview_tip_playpause",
        TBtn::Next => "preview_tip_nextfile",
        TBtn::Mute if muted => "preview_tip_unmute",
        TBtn::Mute => "preview_tip_mute",
        TBtn::Loop if looping => "preview_tip_loop_off",
        TBtn::Loop => "preview_tip_loop",
        // Its text spells out what BOTH settings do, so it does not go stale either way.
        TBtn::Arrows => "tip_preview_arrow_nav",
        TBtn::Speed => "preview_tip_speed",
    })
}

/// Rects for the strip's controls, or EMPTY when the strip is not showing (anything that is not a
/// playing video/track). An empty rect can never trigger a tooltip, which is exactly what we want
/// when the strip is not on screen.
pub(super) unsafe fn transport_rects(hwnd: HWND) -> Vec<(TBtn, RECT)> {
    let st = state(hwnd);
    let showing = !st.is_null()
        && (*st).kind.get() == super::window::ContentKind::Video
        && (*st).video.borrow().is_some();
    if !showing {
        return TBTNS.iter().map(|&b| (b, RECT::default())).collect();
    }
    let sr = scrub_rect(hwnd);
    let p = scrub_parts(hwnd, &sr);
    // The sliders are thin bands; give their tips the full strip height so hovering anywhere over
    // the control shows the tip, not just the 4px groove.
    let tall = |r: RECT| RECT {
        top: sr.top,
        bottom: sr.bottom,
        ..r
    };
    TBTNS
        .iter()
        .map(|&b| {
            let r = match b {
                TBtn::Prev => p.prev,
                TBtn::Play => p.play,
                TBtn::Next => p.next,
                TBtn::Mute => tall(p.mute),
                TBtn::Loop => p.loopb,
                TBtn::Arrows => p.arrows,
                TBtn::Speed => p.speed,
            };
            (b, r)
        })
        .collect()
}

/// The speed steps the button cycles through, in order. Deliberately short: this is a preview
/// transport, not a media player, and every extra step is another click to get back to normal.
pub(super) const SPEEDS: [f64; 5] = [0.5, 1.0, 1.25, 1.5, 2.0];

/// The next speed after `cur`, wrapping. Matching is by nearest step rather than equality so a
/// value restored from the registry (or clamped by the engine) still advances predictably.
pub(super) fn next_speed(cur: f64) -> f64 {
    let mut best = 0usize;
    for (i, s) in SPEEDS.iter().enumerate() {
        if (s - cur).abs() < (SPEEDS[best] - cur).abs() {
            best = i;
        }
    }
    SPEEDS[(best + 1) % SPEEDS.len()]
}

/// Format a speed multiplier the way the button shows it ("1x", "1.5x", "0.5x").
pub(super) fn fmt_speed(mult: f64) -> String {
    if (mult - mult.round()).abs() < 0.01 {
        format!("{}x", mult.round() as i32)
    } else {
        format!("{mult}x")
    }
}

pub(super) unsafe fn scrub_parts(hwnd: HWND, sr: &RECT) -> Parts {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let pad = sc(8);
    let btn = sc(28); // play/pause square
    let skip = sc(24); // prev / next file
    let time_w = sc(96); // "0:07 / 1:23"
    let vol_w = sc(70); // volume slider
    let spk = sc(22); // speaker glyph
    let loop_w = sc(26); // repeat toggle
    let arrows_w = sc(28); // what ←/→ do
    let speed_w = sc(38); // "1.25x" at its widest
    let midy = (sr.top + sr.bottom) / 2;
    let th = sc(4); // track thickness
                    // Right cluster, packed from the right edge inwards.
    let speed = RECT {
        left: sr.right - pad - speed_w,
        top: sr.top,
        right: sr.right - pad,
        bottom: sr.bottom,
    };
    let arrows = RECT {
        left: speed.left - arrows_w,
        top: sr.top,
        right: speed.left,
        bottom: sr.bottom,
    };
    let loopb = RECT {
        left: arrows.left - loop_w,
        top: sr.top,
        right: arrows.left,
        bottom: sr.bottom,
    };
    // The slider's round thumb overhangs `vol.right` by its ~5px radius, so the gap to the repeat
    // glyph must be wide enough for the thumb AT FULL VOLUME plus visible air (owner note,
    // 2026-08-07: at sc(4) the thumb sat nearly touching the repeat icon).
    let vol = RECT {
        left: loopb.left - sc(14) - vol_w,
        top: midy - th / 2,
        right: loopb.left - sc(14),
        bottom: midy + th / 2,
    };
    let mute = RECT {
        left: vol.left - spk,
        top: midy - th / 2,
        right: vol.left,
        bottom: midy + th / 2,
    };
    // Left cluster: ⏮ play/pause ⏭ then the time readout.
    let prev = RECT {
        left: sr.left + pad,
        top: sr.top,
        right: sr.left + pad + skip,
        bottom: sr.bottom,
    };
    let play = RECT {
        left: prev.right,
        top: sr.top,
        right: prev.right + btn,
        bottom: sr.bottom,
    };
    let next = RECT {
        left: play.right,
        top: sr.top,
        right: play.right + skip,
        bottom: sr.bottom,
    };
    let track = RECT {
        left: next.right + time_w,
        top: midy - th / 2,
        right: mute.left - sc(4),
        bottom: midy + th / 2,
    };
    Parts {
        prev,
        next,
        play,
        track,
        mute,
        vol,
        loopb,
        arrows,
        speed,
    }
}

/// Map a mouse x on the seek track to a seek (guarded on a known, finite duration).
pub(super) unsafe fn apply_seek(v: &super::video::VideoPlayer, x: i32, track: &RECT) {
    let dur = v.duration();
    if !dur.is_finite() || dur <= 0.0 {
        return;
    }
    let w = (track.right - track.left).max(1);
    let frac = ((x - track.left) as f64 / w as f64).clamp(0.0, 1.0);
    v.seek(frac * dur);
}

/// Map a mouse x on the volume slider to a volume (0..1), un-muting when raised off zero.
pub(super) unsafe fn apply_vol(v: &super::video::VideoPlayer, x: i32, vol: &RECT) {
    let w = (vol.right - vol.left).max(1);
    let frac = ((x - vol.left) as f64 / w as f64).clamp(0.0, 1.0);
    v.set_volume(frac);
    if frac > 0.0 {
        v.set_muted(false);
    }
}

/// Write the transport's current volume + mute back to Settings, so the next file — and the next
/// preview — starts where you left this one. Call it when a slider drag ENDS or the speaker is
/// clicked, never per mouse-move: each call is a registry write.
pub(super) unsafe fn persist_volume(v: &super::video::VideoPlayer) {
    let pct = (v.volume() * 100.0).round().clamp(0.0, 100.0) as u32;
    let _ = sagethumbs2k_core::settings::set_preview_volume(pct);
    let _ = sagethumbs2k_core::settings::set_preview_muted(v.muted());
}

/// Dispatch a mouse-down on the video transport strip (play/pause · mute · seek · volume).
pub(super) unsafe fn scrub_mouse_down(hwnd: HWND, x: i32, y: i32) {
    let st = &*state(hwnd);
    let sr = scrub_rect(hwnd);
    if y < sr.top || y >= sr.bottom {
        return;
    }
    let vb = st.video.borrow();
    let Some(v) = vb.as_ref() else { return };
    let p = scrub_parts(hwnd, &sr);
    let spk = crate::win::dpi_scale(hwnd, 22);
    // File switching must not run while `st.video` is borrowed: `nav_sibling` reloads, which tears
    // the player down. Note which way was clicked, drop the borrow, then act.
    if let Some(delta) = nav_arrow_hit(x, &p) {
        drop(vb);
        super::window::nav_sibling(hwnd, delta);
        return;
    }
    let _ = spk;
    dispatch_scrub_click(hwnd, st, v, x, &p);
    let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
}

/// Which sibling-nav arrow (if either) was clicked: -1 previous / 1 next / None neither.
fn nav_arrow_hit(x: i32, p: &Parts) -> Option<i32> {
    if x >= p.prev.left && x < p.prev.right {
        Some(-1)
    } else if x >= p.next.left && x < p.next.right {
        Some(1)
    } else {
        None
    }
}

/// Dispatch a click on the transport strip proper (play · speed · arrows · loop · volume ·
/// mute · seek track), once the sibling-nav zones have already been ruled out.
unsafe fn dispatch_scrub_click(
    hwnd: HWND,
    st: &super::window::ViewerState,
    v: &super::video::VideoPlayer,
    x: i32,
    p: &Parts,
) {
    if x >= p.play.left && x < p.play.right {
        v.toggle_play();
    } else if x >= p.speed.left && x < p.speed.right {
        let s = next_speed(v.speed());
        v.set_speed(s);
        let _ = sagethumbs2k_core::settings::set_preview_speed((s * 100.0).round() as u32);
    } else if x >= p.arrows.left && x < p.arrows.right {
        // Flip what ←/→ mean, and remember it. Lives on the strip, not in Settings: it only ever
        // matters while this window is open, which is also the only place anyone would look.
        let on = !st.arrow_nav.get();
        st.arrow_nav.set(on);
        let _ = sagethumbs2k_core::settings::set_preview_arrow_nav(on);
    } else if x >= p.loopb.left && x < p.loopb.right {
        let on = !v.looping();
        v.set_looping(on);
        let _ = sagethumbs2k_core::settings::set_preview_loop(on);
    } else if x >= p.vol.left && x <= p.vol.right {
        st.vol_drag.set(true);
        apply_vol(v, x, &p.vol);
        let _ = SetCapture(hwnd);
    } else if x >= p.mute.left && x < p.mute.right {
        v.set_muted(!v.muted()); // speaker glyph toggles mute
        persist_volume(v); // a click is the whole gesture — remember it now
    } else if x >= p.track.left && x <= p.track.right {
        st.scrub_drag.set(true);
        apply_seek(v, x, &p.track);
        let _ = SetCapture(hwnd);
    }
}

/// Format seconds as `m:ss` (or `0:00` when unknown / NaN).
pub(super) fn fmt_time(secs: f64) -> String {
    if !secs.is_finite() || secs < 0.0 {
        return "0:00".to_string();
    }
    let s = secs as u32;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Draw a horizontal slider: groove + accent progress fill + a round thumb at `frac`.
pub(super) unsafe fn draw_slider(hdc: HDC, rc: &RECT, frac: f64, accent: u32, groove: u32) {
    let gb = CreateSolidBrush(COLORREF(groove));
    FillRect(hdc, rc, gb);
    let _ = DeleteObject(gb.into());
    let w = (rc.right - rc.left).max(1);
    let px = rc.left + (frac * w as f64) as i32;
    let prog = RECT {
        left: rc.left,
        top: rc.top,
        right: px,
        bottom: rc.bottom,
    };
    let ab = CreateSolidBrush(COLORREF(accent));
    FillRect(hdc, &prog, ab);
    let midy = (rc.top + rc.bottom) / 2;
    let r = 5;
    let obr = SelectObject(hdc, ab.into());
    let _ = Ellipse(hdc, px - r, midy - r, px + r, midy + r);
    SelectObject(hdc, obr);
    let _ = DeleteObject(ab.into());
}

/// Paint the video transport strip: bg band + hairline, then the controls. Every ICON is a
/// **Segoe Fluent Icons** glyph via [`super::paint::icon_font`], the same font the caption toolbar
/// uses, so the two rows of buttons read as one family (the strip used to hand-draw its glyphs
/// with GDI polygons, which looked home-made next to the caption; owner call, 2026-08-07).
/// The two sliders stay drawn geometry, because they are sliders, not icons.
pub(super) unsafe fn draw_scrub_strip(
    hwnd: HWND,
    hdc: HDC,
    sr: &RECT,
    v: &super::video::VideoPlayer,
    text: u32,
    subtle: u32,
) {
    let sc = |val: i32| crate::win::dpi_scale(hwnd, val);
    let bg = CreateSolidBrush(COLORREF(crate::dark::DARK_BG().0));
    FillRect(hdc, sr, bg);
    let _ = DeleteObject(bg.into());
    let pen = CreatePen(PS_SOLID, 1, COLORREF(crate::dark::BORDER().0));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, sr.left, sr.top, None);
    let _ = LineTo(hdc, sr.right, sr.top);
    SelectObject(hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    let Parts {
        prev,
        next,
        play,
        track,
        mute,
        vol,
        loopb,
        arrows,
        speed,
    } = scrub_parts(hwnd, sr);
    let accent = crate::dark::ACCENT().0;
    let border = crate::dark::BORDER().0;

    // One icon font for the whole strip (owned here, freed at the end).
    let icon = super::paint::icon_font(hwnd);

    // prev / next file, `subtle` so play/pause stays the dominant control.
    glyph(hdc, icon, &prev, GLYPH_PREVIOUS, subtle);
    glyph(hdc, icon, &next, GLYPH_NEXT, subtle);

    // play / pause, in the text colour.
    let pp = if v.is_paused() {
        GLYPH_PLAY
    } else {
        GLYPH_PAUSE
    };
    glyph(hdc, icon, &play, pp, text);

    // time label
    let dur = v.duration();
    let cur = v.current_time();
    let label = format!("{} / {}", fmt_time(cur), fmt_time(dur));
    let f = crate::win::gui_font_for(hwnd);
    let oldf = SelectObject(hdc, f.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(subtle));
    let mut w: Vec<u16> = label.encode_utf16().collect();
    let mut tr = RECT {
        // After the LAST button of the left cluster (next-file), or the label runs under it.
        left: next.right + sc(6),
        top: sr.top,
        right: track.left - sc(6),
        bottom: sr.bottom,
    };
    DrawTextW(
        hdc,
        &mut w,
        &mut tr,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc, oldf);

    // seek track + progress + thumb
    let frac = if dur.is_finite() && dur > 0.0 {
        (cur / dur).clamp(0.0, 1.0)
    } else {
        0.0
    };
    draw_slider(hdc, &track, frac, accent, border);

    // speaker (muted swaps to the crossed-out glyph, in `subtle`) + volume slider
    let full = RECT {
        top: sr.top,
        bottom: sr.bottom,
        ..mute
    };
    if v.muted() {
        glyph(hdc, icon, &full, GLYPH_MUTE, subtle);
    } else {
        glyph(hdc, icon, &full, GLYPH_VOLUME, text);
    }
    let vfrac = if v.muted() {
        0.0
    } else {
        v.volume().clamp(0.0, 1.0)
    };
    draw_slider(hdc, &vol, vfrac, accent, border);

    // repeat toggle: ACCENT when on, `subtle` when off, so the state reads at a glance.
    glyph(
        hdc,
        icon,
        &loopb,
        GLYPH_REPEAT,
        if v.looping() { accent } else { subtle },
    );

    // ←/→ meaning (two opposing arrows). Same accent-when-on convention as repeat.
    let nav_on = (*state(hwnd)).arrow_nav.get();
    glyph(
        hdc,
        icon,
        &arrows,
        GLYPH_SWITCH,
        if nav_on { accent } else { subtle },
    );

    // playback speed, drawn as its own multiplier text (clicking cycles it). Text, not an icon:
    // the VALUE is the whole point of the control. Normal speed stays `subtle` so the strip is
    // quiet until you actually change it.
    let sp = v.speed();
    let f2 = crate::win::gui_font_for(hwnd);
    let oldf2 = SelectObject(hdc, f2.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(
        hdc,
        COLORREF(if (sp - 1.0).abs() < 0.01 {
            subtle
        } else {
            accent
        }),
    );
    let mut sw: Vec<u16> = fmt_speed(sp).encode_utf16().collect();
    let mut sr2 = speed;
    DrawTextW(
        hdc,
        &mut sw,
        &mut sr2,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc, oldf2);
    let _ = DeleteObject(icon.into());
}

// Segoe Fluent Icons codepoints for the transport (same family as `paint::btn_glyph`).
const GLYPH_PLAY: u16 = 0xE768;
const GLYPH_PAUSE: u16 = 0xE769;
const GLYPH_PREVIOUS: u16 = 0xE892;
const GLYPH_NEXT: u16 = 0xE893;
const GLYPH_VOLUME: u16 = 0xE767;
const GLYPH_MUTE: u16 = 0xE74F;
const GLYPH_REPEAT: u16 = 0xE8EE; // RepeatAll
const GLYPH_SWITCH: u16 = 0xE8AB; // Switch (two opposing horizontal arrows)

/// Draw one icon-font glyph centered in `rc` — the strip's counterpart of the caption's
/// `draw_button` tail, minus the hover pill (the strip has no hover state).
unsafe fn glyph(hdc: HDC, font: HFONT, rc: &RECT, code: u16, colour: u32) {
    let old = SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(colour));
    let mut buf = [code];
    let mut rr = *rc;
    DrawTextW(
        hdc,
        &mut buf,
        &mut rr,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );
    SelectObject(hdc, old);
}

#[cfg(test)]
mod tests {
    use super::{fmt_speed, next_speed, SPEEDS};

    #[test]
    fn speed_cycles_and_formats() {
        // Every step advances to the next, and the last wraps to the first.
        for w in SPEEDS.windows(2) {
            assert_eq!(next_speed(w[0]), w[1]);
        }
        assert_eq!(next_speed(SPEEDS[SPEEDS.len() - 1]), SPEEDS[0]);
        // A value the engine clamped (or an old registry value) still lands on the NEAREST step's
        // successor rather than falling back to the start.
        assert_eq!(next_speed(1.02), 1.25);
        assert_eq!(next_speed(3.0), SPEEDS[0]);
        // Whole multiples lose the decimal point; fractional ones keep it.
        assert_eq!(fmt_speed(1.0), "1x");
        assert_eq!(fmt_speed(2.0), "2x");
        assert_eq!(fmt_speed(1.5), "1.5x");
        assert_eq!(fmt_speed(0.5), "0.5x");
    }
}
