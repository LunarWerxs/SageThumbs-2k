//! Caption toolbar: button rects, tooltips, and button hit-testing.

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::UI::Controls::{
    TTF_SUBCLASS, TTM_ADDTOOLW, TTM_NEWTOOLRECTW, TTM_SETMAXTIPWIDTH, TTS_ALWAYSTIP, TTS_NOPREFIX,
    TTTOOLINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::transport::TBTNS;
use super::window::{btn_visible, state, Btn, BTNS, BTN_W, CAPTION_H, MIN_BTN_W, PAD};

/// Toolbar button rects (device px, in client coords), right-aligned in the caption. Hidden
/// buttons (see [`btn_visible`]) are omitted, so the visible set stays right-packed.
///
/// **Cells NARROW rather than overflowing when the visible set is wider than the caption.** The
/// most crowded case is a Markdown document that has headings, references a web image, and has a
/// source view, which shows twelve buttons at once; dragged to the 400 px minimum width that is
/// wider than the window, and the old fixed-width layout simply ran the leftmost buttons off the
/// left edge, where they were invisible and unclickable. Shrinking the CELL keeps every button
/// reachable and costs only padding, since the glyph is drawn centred and is ~14 px inside a
/// 38 px cell. When there is room the arithmetic picks [`BTN_W`] unchanged, so the normal window
/// lays out exactly as before.
pub(super) unsafe fn button_rects(hwnd: HWND) -> Vec<(Btn, RECT)> {
    let st = state(hwnd);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let cap = sc(CAPTION_H);
    let visible: Vec<Btn> = BTNS
        .iter()
        .rev()
        .copied()
        .filter(|&b| st.is_null() || btn_visible(&*st, b))
        .collect();
    let bw = cell_width(sc(BTN_W), sc(MIN_BTN_W), rc.right - sc(PAD), visible.len());
    let mut right = rc.right - sc(PAD);
    let mut out = Vec::with_capacity(visible.len());
    // Laid out right-to-left so Close sits at the far right.
    for b in visible {
        out.push((
            b,
            RECT {
                left: right - bw,
                top: 0,
                right,
                bottom: cap,
            },
        ));
        right -= bw;
    }
    out
}

/// How wide each toolbar cell should be: `full` when the `visible` buttons fit inside `avail`,
/// otherwise the widest that does fit, never below `min`.
///
/// Its own function so the rule is testable without a window, and so the "when there is room,
/// nothing changes" property is asserted rather than assumed — that is what keeps this from
/// quietly re-laying-out every normal preview.
pub(super) fn cell_width(full: i32, min: i32, avail: i32, visible: usize) -> i32 {
    match i32::try_from(visible) {
        Ok(n) if n > 0 => full.min(avail.max(0) / n).max(min),
        _ => full,
    }
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
        // One key, two meanings — the button flips, so the tip has to say which way it goes
        // or it describes the state you are already in.
        Btn::Theme if crate::dark::is_dark() => "preview_tip_theme_light",
        Btn::Theme => "preview_tip_theme_dark",
        Btn::Settings => "preview_tip_settings",
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
    // trigger; [`update_tooltips`] re-points every rect when the layout changes.
    let rects = tool_rects(hwnd);
    for (idx, &b) in BTNS.iter().enumerate() {
        add_tool(tip, hwnd, idx, rects[idx], btn_tip(b));
    }
    for i in 0..TBTNS.len() {
        add_tool(
            tip,
            hwnd,
            BTNS.len() + i,
            rects[BTNS.len() + i],
            super::transport::tbtn_tip(TBTNS[i]),
        );
    }
    let st = state(hwnd);
    if !st.is_null() {
        *(*st).tip_rects.borrow_mut() = rects;
    }
    tip
}

/// Every registered tool's rect, in tool-id order: one per [`BTNS`] entry (hidden buttons get an
/// empty rect, which can never be hit), then one per transport control. This is THE layout both
/// the tooltip control and the painter answer to — [`button_rects`] is the single source, so a
/// tip cannot describe a button that is no longer under it.
pub(super) unsafe fn tool_rects(hwnd: HWND) -> Vec<RECT> {
    let rects = button_rects(hwnd);
    let mut out = Vec::with_capacity(BTNS.len() + TBTNS.len());
    for &b in BTNS.iter() {
        out.push(
            rects
                .iter()
                .find(|(bb, _)| *bb == b)
                .map(|(_, r)| *r)
                .unwrap_or_default(),
        );
    }
    for (_, r) in super::transport::transport_rects(hwnd) {
        out.push(r);
    }
    out
}

/// Whether the tooltip control's registered rects still describe `now`.
///
/// Split out as a plain function so the cache it guards is testable without a window. `RECT` is a
/// plain POD here; comparing the four edges avoids depending on whether the `windows` crate
/// derives `PartialEq` for it in a given version.
pub(super) fn tooltip_layout_changed(cached: &[RECT], now: &[RECT]) -> bool {
    cached.len() != now.len()
        || cached.iter().zip(now).any(|(a, b)| {
            a.left != b.left || a.top != b.top || a.right != b.right || a.bottom != b.bottom
        })
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

/// Re-point every tooltip tool at its control's current rect, if the layout moved since the last
/// call. No-op if the tip control wasn't created (the headless shot never makes one).
///
/// **Called from the PAINT path, and that is the fix, not an optimisation.** A caption button
/// appears or disappears from several places — a decode landing (`Btn::Ocr` needs
/// `ContentKind::Image`, which only becomes true when the worker's bitmap arrives), a PDF page
/// count, a Markdown document turning out to have headings, a resize — and the previous design
/// asked each of those to remember to call this. `on_render` did not, so on every image the two
/// leftmost tips sat one button to the right of where they belonged: hovering Copy said "Keep on
/// top" and the pin had no tooltip at all. Any state change that alters the buttons MUST repaint
/// the caption or the drawn toolbar itself would be wrong, so the paint is the one place that
/// cannot be forgotten. The comparison against the last-synced rects keeps the common repaint
/// (a scroll notch, a hover) down to a `Vec` compare with no window messages at all.
pub(super) unsafe fn update_tooltips(hwnd: HWND, tip: HWND) {
    if tip.is_invalid() {
        return;
    }
    let st = state(hwnd);
    if st.is_null() {
        return;
    }
    let rects = tool_rects(hwnd);
    if !tooltip_layout_changed(&(*st).tip_rects.borrow(), &rects) {
        return;
    }
    for (idx, r) in rects.iter().enumerate() {
        move_tool(tip, hwnd, idx, *r);
    }
    *(*st).tip_rects.borrow_mut() = rects;
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
        // Run the whole bar under BOTH skins. The theme button's tooltip depends on which one
        // is active (it names the mode it switches TO), so a single pass leaves one of its two
        // strings unchecked — and WHICH one gets checked would depend on how the machine
        // running the test happens to be themed. That is a test whose result turns on
        // something other than the code.
        for dark in [false, true] {
            crate::dark::set_theme_override(Some(dark));
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
        crate::dark::set_theme_override(None);
    }

    /// The crowded caption must fit, and the ordinary one must not change.
    ///
    /// Twelve buttons is the real worst case (a Markdown document with headings, a web image and
    /// a source view), and 400 px is the viewer's minimum width. At 38 px each that is 456 px of
    /// buttons in a 400 px caption, and the old fixed-width layout ran the leftmost ones off the
    /// left edge — invisible and unclickable. The second half of this test is the more important
    /// half: with room to spare the answer has to be exactly `BTN_W`, or this "fix" would have
    /// silently re-laid-out every preview window in the product.
    #[test]
    fn cells_narrow_only_when_the_caption_cannot_fit_them() {
        const FULL: i32 = 38;
        const MIN: i32 = 22;
        // A roomy caption: untouched, whatever the button count.
        assert_eq!(cell_width(FULL, MIN, 634, 10), FULL);
        assert_eq!(cell_width(FULL, MIN, 994, 12), FULL);
        // Exactly enough room is still "enough" — no premature shrinking.
        assert_eq!(cell_width(FULL, MIN, FULL * 12, 12), FULL);
        // The crowded case: shrink, and the whole set must then fit.
        let bw = cell_width(FULL, MIN, 394, 12);
        assert!(bw < FULL, "should have narrowed, got {bw}");
        assert!(
            bw * 12 <= 394,
            "narrowed to {bw} but 12 of them still overflow"
        );
        // Absurdly small: clamped at the floor rather than collapsing to slivers.
        assert_eq!(cell_width(FULL, MIN, 60, 12), MIN);
        // Degenerate inputs must not divide by zero or go negative.
        assert_eq!(cell_width(FULL, MIN, 634, 0), FULL);
        assert_eq!(cell_width(FULL, MIN, -20, 8), MIN);
    }

    fn rc(left: i32, right: i32) -> RECT {
        RECT {
            left,
            top: 0,
            right,
            bottom: 36,
        }
    }

    /// The guard in front of the tooltip re-point must fire on exactly the layout shift that
    /// shipped broken, and stay quiet on a plain repaint.
    ///
    /// The shipped bug: the tips were registered while the viewer was still `Loading`, where
    /// `Btn::Ocr` is hidden, so Pin and Copy sat one 38 px slot further right than they would
    /// once the decode landed and the OCR button appeared between Copy and Info. Nothing
    /// re-pointed them afterwards, so hovering Copy showed the PIN's tooltip ("Keep on top")
    /// and the pin itself had none. Buttons are right-packed, so only the entries LEFT of the
    /// one that appeared move — which is why this has to compare the whole list rather than,
    /// say, the count.
    #[test]
    fn tooltip_layout_change_is_detected_when_a_button_appears_mid_load() {
        // …Copy, [Ocr hidden], Info… — Pin and Copy sit one slot right of their final home.
        let loading = [rc(330, 368), rc(368, 406), rc(0, 0), rc(406, 444)];
        // The decode landed: Ocr took a slot and pushed Copy and Pin left.
        let decoded = [rc(292, 330), rc(330, 368), rc(368, 406), rc(406, 444)];
        assert!(
            tooltip_layout_changed(&loading, &decoded),
            "a button appearing must re-point the tips, or they describe the wrong buttons"
        );
        // A repaint that changed nothing (scroll notch, hover) must NOT send window messages.
        assert!(!tooltip_layout_changed(&decoded, &decoded));
        // A resize moves every right-anchored button, and must also be caught.
        let wider: Vec<RECT> = decoded
            .iter()
            .map(|r| rc(r.left + 80, r.right + 80))
            .collect();
        assert!(tooltip_layout_changed(&decoded, &wider));
        // First paint: nothing registered yet.
        assert!(tooltip_layout_changed(&[], &decoded));
    }
}
