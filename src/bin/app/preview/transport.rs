//! Video/audio transport strip: seek track + volume slider + time.


use windows::Win32::Foundation::{
    COLORREF, HWND, POINT, RECT,
};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, FillRect,
    InvalidateRect, LineTo, MoveToEx, Polygon, Polyline, SelectObject, SetBkMode,
    SetTextColor, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HDC, HGDIOBJ,
    PS_SOLID, TRANSPARENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetCapture;

use super::window::{state, content_rect, SCRUB_H};

pub(super) unsafe fn video_rect(hwnd: HWND) -> RECT {
    let mut r = content_rect(hwnd);
    r.bottom -= crate::win::dpi_scale(hwnd, SCRUB_H);
    r
}

/// Video-only: the transport strip's rect (bottom band of the content area).
pub(super) unsafe fn scrub_rect(hwnd: HWND) -> RECT {
    let c = content_rect(hwnd);
    let h = crate::win::dpi_scale(hwnd, SCRUB_H);
    RECT { left: c.left, top: c.bottom - h, right: c.right, bottom: c.bottom }
}

/// The seek-track and volume-slider sub-rects inside the strip (device px). The play/pause
/// button is a square on the left; the time label sits after it; the seek track fills the
/// middle; a speaker + volume slider sit on the right.
pub(super) unsafe fn scrub_parts(hwnd: HWND, sr: &RECT) -> (RECT, RECT, RECT) {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let pad = sc(8);
    let btn = sc(28); // play/pause square
    let time_w = sc(96); // "0:07 / 1:23"
    let vol_w = sc(70); // volume slider
    let spk = sc(22); // speaker glyph
    let midy = (sr.top + sr.bottom) / 2;
    let th = sc(4); // track thickness
    let track = RECT {
        left: sr.left + pad + btn + time_w,
        top: midy - th / 2,
        right: sr.right - pad - vol_w - spk,
        bottom: midy + th / 2,
    };
    let vol = RECT {
        left: sr.right - pad - vol_w,
        top: midy - th / 2,
        right: sr.right - pad,
        bottom: midy + th / 2,
    };
    let play = RECT { left: sr.left + pad, top: sr.top, right: sr.left + pad + btn, bottom: sr.bottom };
    (play, track, vol)
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

/// Dispatch a mouse-down on the video transport strip (play/pause · mute · seek · volume).
pub(super) unsafe fn scrub_mouse_down(hwnd: HWND, x: i32, y: i32) {
    let st = &*state(hwnd);
    let sr = scrub_rect(hwnd);
    if y < sr.top || y >= sr.bottom {
        return;
    }
    let vb = st.video.borrow();
    let Some(v) = vb.as_ref() else { return };
    let (play, track, vol) = scrub_parts(hwnd, &sr);
    let spk = crate::win::dpi_scale(hwnd, 22);
    if x >= play.left && x < play.right {
        v.toggle_play();
    } else if x >= vol.left && x <= vol.right {
        st.vol_drag.set(true);
        apply_vol(v, x, &vol);
        let _ = SetCapture(hwnd);
    } else if x >= vol.left - spk && x < vol.left {
        v.set_muted(!v.muted()); // speaker glyph toggles mute
    } else if x >= track.left && x <= track.right {
        st.scrub_drag.set(true);
        apply_seek(v, x, &track);
        let _ = SetCapture(hwnd);
    }
    let _ = InvalidateRect(Some(hwnd), Some(&sr), false);
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
    let prog = RECT { left: rc.left, top: rc.top, right: px, bottom: rc.bottom };
    let ab = CreateSolidBrush(COLORREF(accent));
    FillRect(hdc, &prog, ab);
    let midy = (rc.top + rc.bottom) / 2;
    let r = 5;
    let obr = SelectObject(hdc, ab.into());
    let _ = Ellipse(hdc, px - r, midy - r, px + r, midy + r);
    SelectObject(hdc, obr);
    let _ = DeleteObject(ab.into());
}

/// Paint the video transport strip: bg band + hairline, play/pause glyph, `m:ss / m:ss`, seek
/// track + thumb, speaker glyph + volume slider. All GDI + existing `dark.rs` colours.
pub(super) unsafe fn draw_scrub_strip(hwnd: HWND, hdc: HDC, sr: &RECT, v: &super::video::VideoPlayer, text: u32, subtle: u32) {
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

    let (play, track, vol) = scrub_parts(hwnd, sr);
    let accent = crate::dark::ACCENT().0;
    let border = crate::dark::BORDER().0;
    let midy = (sr.top + sr.bottom) / 2;
    let cx = (play.left + play.right) / 2;

    // play / pause glyph (filled, in the text colour)
    let fill = CreateSolidBrush(COLORREF(text));
    let obr = SelectObject(hdc, fill.into());
    let gpen = CreatePen(PS_SOLID, 1, COLORREF(text));
    let gob = SelectObject(hdc, HGDIOBJ(gpen.0));
    if v.is_paused() {
        let s = sc(6);
        let tri = [
            POINT { x: cx - s / 2, y: midy - s },
            POINT { x: cx - s / 2, y: midy + s },
            POINT { x: cx + s, y: midy },
        ];
        let _ = Polygon(hdc, &tri);
    } else {
        let s = sc(5);
        let b = sc(3);
        let l = RECT { left: cx - s, top: midy - s - 1, right: cx - s + b, bottom: midy + s + 1 };
        let r = RECT { left: cx + s - b, top: midy - s - 1, right: cx + s, bottom: midy + s + 1 };
        FillRect(hdc, &l, fill);
        FillRect(hdc, &r, fill);
    }
    SelectObject(hdc, gob);
    let _ = DeleteObject(HGDIOBJ(gpen.0));
    SelectObject(hdc, obr);
    let _ = DeleteObject(fill.into());

    // time label
    let dur = v.duration();
    let cur = v.current_time();
    let label = format!("{} / {}", fmt_time(cur), fmt_time(dur));
    let f = crate::win::gui_font_for(hwnd);
    let oldf = SelectObject(hdc, f.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(subtle));
    let mut w: Vec<u16> = label.encode_utf16().collect();
    let mut tr = RECT { left: play.right + sc(6), top: sr.top, right: track.left - sc(6), bottom: sr.bottom };
    DrawTextW(hdc, &mut w, &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX);
    SelectObject(hdc, oldf);

    // seek track + progress + thumb
    let frac = if dur.is_finite() && dur > 0.0 { (cur / dur).clamp(0.0, 1.0) } else { 0.0 };
    draw_slider(hdc, &track, frac, accent, border);

    // speaker glyph (outline) + volume slider
    let spk = sc(22);
    let sx = vol.left - spk + sc(3);
    let spen = CreatePen(PS_SOLID, sc(2), COLORREF(if v.muted() { subtle } else { text }));
    let sob = SelectObject(hdc, HGDIOBJ(spen.0));
    let cone = [
        POINT { x: sx, y: midy - sc(2) },
        POINT { x: sx + sc(4), y: midy - sc(2) },
        POINT { x: sx + sc(8), y: midy - sc(5) },
        POINT { x: sx + sc(8), y: midy + sc(5) },
        POINT { x: sx + sc(4), y: midy + sc(2) },
        POINT { x: sx, y: midy + sc(2) },
        POINT { x: sx, y: midy - sc(2) },
    ];
    let _ = Polyline(hdc, &cone);
    SelectObject(hdc, sob);
    let _ = DeleteObject(HGDIOBJ(spen.0));
    let vfrac = if v.muted() { 0.0 } else { v.volume().clamp(0.0, 1.0) };
    draw_slider(hdc, &vol, vfrac, accent, border);
}

