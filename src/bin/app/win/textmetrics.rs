//! Measuring text for layout, in device and design units.

use super::*;

/// Pixel width of `s` rendered in the GUI font, in 96-DPI DESIGN pixels — the same units
/// every `ctl()` caller passes as `cw`, since `ctl()` unconditionally re-scales `cw` via
/// `dpi_scale(parent, cw)`.
///
/// Measures with `gui_font_for(hwnd)` (not the process-lifetime-cached [`gui_font`]), so the
/// font actually matches `hwnd`'s real current DPI rather than whatever DPI happened to be
/// active the first time any dialog in this process asked for a font. `dpi_unscale` then
/// converts that real-DPI pixel width back down to 96-DPI design units before returning —
/// without it, a caller like the About box would hand `ctl()` an already-DPI-scaled width,
/// and `ctl()` would scale it AGAIN, sizing the pill wrong on any non-96-DPI monitor. At
/// 96 DPI both the font and the unscale are identity, so the common case is unchanged.
pub(crate) unsafe fn text_width(hwnd: HWND, s: &str) -> i32 {
    dpi_unscale(hwnd, text_w_in(gui_font_for(hwnd), s))
}

/// One line of `s` measured in `font`, in that font's OWN pixels. The raw half of
/// [`text_width`] and [`design_text_w`], so the two differ only in which font they pick and
/// whether they scale the answer back.
pub(super) unsafe fn text_w_in(font: HFONT, s: &str) -> i32 {
    let hdc = GetDC(None);
    if hdc.is_invalid() {
        return 0;
    }
    let old = SelectObject(hdc, HGDIOBJ(font.0));
    let w = wide(s);
    let n = w.len().saturating_sub(1);
    let mut sz = windows::Win32::Foundation::SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
    SelectObject(hdc, old);
    ReleaseDC(None, hdc);
    sz.cx
}

/// Height `s` needs once wrapped into a `col_w`-wide column, in 96-DPI DESIGN pixels, the
/// same units [`ctl`] takes for `ch`.
///
/// The height half of [`text_width`], and the one shared place three dialogs now measure
/// wrapped copy from (2026-09-05 audit finding F36): the settings nudge card, the first-run
/// welcome's intro and its caption rows. Each of those grew its own private copy of this GDI
/// dance, which is how the first-run captions kept a flat two-line box while the nudge card
/// was already measuring properly.
///
/// `HWND::default()` is a legitimate argument, and the reason this takes an `HWND` at all
/// rather than measuring at a flat design scale: `effective_dpi` answers with the
/// headless-shot override (or 96) for a null window, so the SAME function measures both
/// before the window exists (sizing it) and afterwards (laying its children out), and a
/// `--shot --dpi 192` capture measures at 192 in both. The column is scaled up to device px
/// for the measurement because the font is a device-DPI font, then the answer is scaled back
/// down, keeping the 96-DPI identity.
pub(crate) unsafe fn wrapped_text_h(hwnd: HWND, s: &str, col_w: i32) -> i32 {
    let col_px = dpi_scale(hwnd, col_w).max(1);
    dpi_unscale(hwnd, wrapped_h_in(gui_font_for(hwnd), s, col_px))
}

/// `s` wrapped into a `col_px`-wide column in `font`, in that font's OWN pixels. The raw
/// half of [`wrapped_text_h`] and [`design_wrapped_text_h`].
pub(super) unsafe fn wrapped_h_in(font: HFONT, s: &str, col_px: i32) -> i32 {
    let hdc = GetDC(None);
    if hdc.is_invalid() {
        return 0;
    }
    let old = SelectObject(hdc, HGDIOBJ(font.0));
    let mut w = wide(s);
    let n = w.len().saturating_sub(1);
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: col_px.max(1),
        bottom: 0,
    };
    // DT_WORDBREAK + DT_NOPREFIX: the same flags the STATIC controls this sizes are drawn
    // with, or the measurement would describe a different layout than the one on screen.
    DrawTextW(
        hdc,
        &mut w[..n],
        &mut rc,
        DT_CALCRECT | DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
    );
    SelectObject(hdc, old);
    ReleaseDC(None, hdc);
    rc.bottom - rc.top
}

/// [`text_width`] at a pinned 96-DPI design scale.
#[cfg(test)]
pub(crate) unsafe fn design_text_w(s: &str) -> i32 {
    text_w_in(gui_font(), s)
}

/// [`wrapped_text_h`] at a pinned 96-DPI design scale.
#[cfg(test)]
pub(crate) unsafe fn design_wrapped_text_h(s: &str, col_w: i32) -> i32 {
    wrapped_h_in(gui_font(), s, col_w)
}

#[cfg(test)]
pub(super) mod text_width_tests {
    use super::*;

    // The DPI-override guard is `scaling::DpiOverrideGuard`, imported through the glob
    // above. This module used to keep its OWN copy because scaling's was private, and that
    // duplication is precisely what let this test and scaling's race on the one global they
    // share. One guard, one lock, or they collide again.
    use super::scaling::DpiOverrideGuard;

    /// `ctl()` unconditionally re-scales its `cw` argument by the window's DPI
    /// (`dpi_scale(parent, cw)`), so `text_width` must hand back a 96-DPI DESIGN-pixel
    /// width — not the raw device-pixel width of whatever font it measured with, or a
    /// pill built from it gets scaled TWICE on any non-96-DPI monitor. This reproduces
    /// the bug directly: round-tripping `text_width`'s result back through `dpi_scale`
    /// (exactly what `ctl()` does) must reproduce the REAL 192-DPI measurement, not
    /// double it. `HWND(usize::MAX as _)` is never dereferenced — `set_dpi_override`
    /// makes `effective_dpi` short-circuit before `GetDpiForWindow` is ever called,
    /// the same pattern `scaling.rs`'s own DPI-override test already relies on.
    #[test]
    fn text_width_round_trips_through_dpi_scale_without_doubling() {
        let hwnd = HWND(usize::MAX as *mut c_void);
        let _guard = DpiOverrideGuard::acquire(); // BEFORE the set: see the lock's doc
        set_dpi_override(192); // 2x

        let s = "v1.2.3";
        let design_w = unsafe { text_width(hwnd, s) };

        // The raw 192-DPI measurement `text_width` is supposed to un-scale from: the same
        // `text_w_in` measurement `text_width` itself takes, without the 96-DPI unscale that
        // is exactly what this test is about.
        let raw_w = unsafe { text_w_in(gui_font_for(hwnd), s) };

        let rescaled = dpi_scale(hwnd, design_w);
        assert!(
            (rescaled - raw_w).abs() <= 1,
            "ctl()'s rescale of text_width's result ({rescaled}) must reproduce the real \
             192-DPI measurement ({raw_w}) — not double- or under-scale it"
        );
        assert!(
            design_w < raw_w,
            "a 96-DPI design width ({design_w}) must be smaller than the raw 192-DPI \
             measurement ({raw_w}); returning the raw width unchanged is the exact bug \
             this test catches"
        );
    }
}
