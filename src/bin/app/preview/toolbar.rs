//! Caption toolbar: button rects, tooltips, and button hit-testing.

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::Controls::{
    TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW, TTM_SETMAXTIPWIDTH, TTS_ALWAYSTIP, TTS_NOPREFIX,
    TTTOOLINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::transport::TBTNS;
use super::window::{btn_visible, state, Btn, BTNS, BTN_W, CAPTION_H, PAD};

/// Toolbar button rects (device px, in client coords), right-aligned in the caption. Hidden
/// buttons (see [`btn_visible`]) are omitted, so the visible set stays right-packed.
pub(super) unsafe fn button_rects(hwnd: HWND) -> Vec<(Btn, RECT)> {
    let st = state(hwnd);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let cap = sc(CAPTION_H);
    let bw = sc(BTN_W);
    let mut right = rc.right - sc(PAD);
    let mut out = Vec::with_capacity(BTNS.len());
    // Lay out right-to-left so Close sits at the far right.
    for &b in BTNS.iter().rev() {
        if !st.is_null() && !btn_visible(&*st, b) {
            continue;
        }
        let r = RECT {
            left: right - bw,
            top: 0,
            right,
            bottom: cap,
        };
        out.push((b, r));
        right -= bw;
    }
    out
}

/// Localized tooltip label for a toolbar button.
pub(super) fn btn_tip(b: Btn) -> &'static str {
    crate::win::t(match b {
        Btn::Toc => "preview_tip_toc",
        // Reuses the string the Settings checkbox used before this moved into the
        // window, so all 36 translations carried straight over.
        Btn::MdImages => "tip_preview_md_remote",
        Btn::Source => "preview_tip_source",
        Btn::PdfPrev => "preview_tip_prev",
        Btn::PdfNext => "preview_tip_next",
        Btn::Pin => "preview_tip_pin",
        Btn::Copy => "preview_tip_copy",
        Btn::Ocr => "preview_tip_ocr",
        Btn::Info => "preview_tip_info",
        Btn::Upload => "preview_tip_upload",
        Btn::OpenWith => "preview_tip_openwith",
        Btn::Open => "preview_tip_open",
        Btn::Close => "preview_tip_close",
    })
}

/// Create the caption toolbar's tooltip control: one RECT tool per button, `TTF_SUBCLASS` so the
/// tip auto-tracks the mouse over the parent (the buttons are custom-drawn, not child HWNDs).
/// Returns `HWND::default()` on failure. Rects are refreshed on resize via [`update_tooltips`].
pub(super) unsafe fn create_tooltips(hwnd: HWND, hinst: HINSTANCE) -> HWND {
    let Ok(tip) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("tooltips_class32"),
        PCWSTR::null(),
        WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
        0,
        0,
        0,
        0,
        Some(hwnd),
        None,
        Some(hinst),
        None,
    ) else {
        return HWND::default();
    };
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, Some(WPARAM(0)), Some(LPARAM(320)));
    // One tool per BTNS entry (uId = BTNS index), then one per transport control (uId continues
    // past BTNS.len()). Hidden buttons and a hidden strip get an EMPTY rect so their tip can never
    // trigger; update_tooltips re-points every rect when visibility changes.
    let rects = button_rects(hwnd);
    for (idx, &b) in BTNS.iter().enumerate() {
        let r = rects
            .iter()
            .find(|(bb, _)| *bb == b)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        add_tool(tip, hwnd, idx, r, btn_tip(b));
    }
    for (i, (_, r)) in super::transport::transport_rects(hwnd)
        .into_iter()
        .enumerate()
    {
        add_tool(
            tip,
            hwnd,
            BTNS.len() + i,
            r,
            super::transport::tbtn_tip(TBTNS[i]),
        );
    }
    tip
}

/// Register one rect tool. comctl32 copies the text on add, so the wide temporary is fine.
unsafe fn add_tool(tip: HWND, hwnd: HWND, id: usize, rect: RECT, text: &str) {
    let text = crate::win::wide(text);
    let mut ti = TTTOOLINFOW {
        cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_SUBCLASS,
        hwnd,
        uId: id,
        rect,
        lpszText: PWSTR(text.as_ptr() as *mut u16),
        ..Default::default()
    };
    SendMessageW(
        tip,
        TTM_ADDTOOLW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut ti as *mut _ as isize)),
    );
}

/// Re-point each tooltip tool at its button's current rect (buttons are right-anchored, so a
/// resize moves them; the PDF pager appears/disappears with the content). Hidden buttons get an
/// empty rect. No-op if the tip control wasn't created.
pub(super) unsafe fn update_tooltips(hwnd: HWND, tip: HWND) {
    if tip.is_invalid() {
        return;
    }
    let rects = button_rects(hwnd);
    for (idx, &b) in BTNS.iter().enumerate() {
        let r = rects
            .iter()
            .find(|(bb, _)| *bb == b)
            .map(|(_, r)| *r)
            .unwrap_or_default();
        move_tool(tip, hwnd, idx, r);
    }
    // The transport strip appears and disappears with the content, so its rects have to be
    // re-pointed on the same schedule as the caption's.
    for (i, (_, r)) in super::transport::transport_rects(hwnd)
        .into_iter()
        .enumerate()
    {
        move_tool(tip, hwnd, BTNS.len() + i, r);
    }
}

/// Re-point one registered tool at a new rect.
unsafe fn move_tool(tip: HWND, hwnd: HWND, id: usize, rect: RECT) {
    let mut ti = TTTOOLINFOW {
        cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
        uFlags: TTF_SUBCLASS,
        hwnd,
        uId: id,
        rect,
        ..Default::default()
    };
    SendMessageW(
        tip,
        TTM_NEWTOOLRECTW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut ti as *mut _ as isize)),
    );
}

/// Which button (if any) contains the client-space point.
pub(super) unsafe fn hit_button(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    for (b, r) in button_rects(hwnd) {
        if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
            return BTNS.iter().position(|&bb| bb == b);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every caption button must have a REAL translated tooltip. `i18n::t` returns a
    /// `⟨?⟩` sentinel when a key is missing from both the active locale and `en`, so this
    /// catches the easy half of adding a button: wiring the enum + paint + click and then
    /// forgetting the string. Distinctness catches the copy-paste variant (two buttons
    /// pointing at one key), which reads as a duplicated tooltip on hover.
    #[test]
    fn every_toolbar_button_has_its_own_real_tooltip() {
        let mut seen: Vec<&str> = Vec::new();
        for &b in BTNS.iter() {
            let tip = btn_tip(b);
            assert!(
                !tip.is_empty() && !tip.starts_with('\u{27e8}'),
                "a preview toolbar button has no translated tooltip (missing locale key)"
            );
            assert!(
                !seen.contains(&tip),
                "two preview toolbar buttons share the tooltip {tip:?}"
            );
            seen.push(tip);
        }
    }
}
