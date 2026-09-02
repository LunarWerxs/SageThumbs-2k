//! The Business-licence reminder strip: "this copy is installed for business use and has
//! no licence key" (`license::Posture::BusinessNag`), or the revoked wording
//! (`DeauthorizedLoud`). Sibling of `nudge.rs` — same strip mechanism, same "decided once
//! before the window is built, because page layout runs exactly once" reasoning (see that
//! module's doc comment) — but simpler: one line of body text, one button ("Open Licence"
//! → the Licence page), and **no dismiss**. Persistent-while-unlicensed is the entire
//! point of Business mode (see `license.rs`'s `Posture::BusinessNag` doc comment), so
//! unlike `nudge`'s banner this one has no "Not now" and nothing here ever hides it mid-
//! session — the strip goes away only because the NEXT Settings open decides differently.

use super::*;
use crate::gdip;
use windows::Win32::Graphics::Gdi::DT_WORDBREAK;

/// Top offset of the body text within the card. Smaller than `nudge`'s `BODY_TOP` (36):
/// this card has no headline above the body, just the body then the button row.
const BODY_TOP: i32 = 14;
const BTN_H: i32 = 26;
/// Floor for the one button's width — what the English label needs; see `nudge`'s
/// `BTN_W_ACTION` doc comment for why a floor and not a fixed width (a translated label is
/// routinely wider).
const BTN_W_FLOOR: i32 = 150;
const PAD: i32 = 14;

thread_local! {
    static SHOWING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static REVOKED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static KEY_PREFIX: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };
    /// Memo for `card_h`, cleared by `decide` for the same reason `nudge`'s `CARD_H_MEMO`
    /// is: a reopened window may be showing a different language's (longer or shorter)
    /// sentence.
    static CARD_H_MEMO: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Ask the licence engine, once, before the window is created. Returns whether the strip
/// will be shown, which is what the caller uses to decide how tall to make the window.
pub(crate) fn decide() -> bool {
    let snap = crate::license::snapshot();
    let revoked = matches!(snap.posture, crate::license::Posture::DeauthorizedLoud);
    let showing = revoked || matches!(snap.posture, crate::license::Posture::BusinessNag);
    SHOWING.with(|s| s.set(showing));
    REVOKED.with(|r| r.set(revoked));
    KEY_PREFIX.with(|k| *k.borrow_mut() = snap.key_prefix.clone());
    CARD_H_MEMO.with(|c| c.set(0));
    showing
}

/// Whether the strip is live for this window.
pub(super) fn showing() -> bool {
    SHOWING.with(|s| s.get())
}

/// How much taller the settings window is because of this strip (design px; 0 when there
/// is none). Pairs with `nudge::extra_height` — both are summed into the window height.
pub(crate) fn extra_height() -> i32 {
    if showing() {
        strip_h()
    } else {
        0
    }
}

/// Design-pixel height of the whole strip, including the gaps above and below the card.
pub(super) fn strip_h() -> i32 {
    card_h() + 16
}

fn body_text() -> String {
    if REVOKED.with(|r| r.get()) {
        let key = KEY_PREFIX.with(|k| k.borrow().clone());
        t("biznag_body_revoked").replace("{key}", &key)
    } else {
        t("biznag_body").to_string()
    }
}

/// MEASURED, not a constant — same reasoning as `nudge::card_h`'s doc comment: the body is
/// translated into 35 other languages and a height that fits English can clip a longer one.
fn card_h() -> i32 {
    CARD_H_MEMO.with(|c| {
        let memo = c.get();
        if memo > 0 {
            return memo;
        }
        let h = BODY_TOP + unsafe { nudge::measure_body_h(&body_text()) } + BTN_H + 18;
        c.set(h);
        h
    })
}

/// The card's tint. A touch stronger than `nudge`'s (22/12% vs its weight) — this is a
/// persistent, non-dismissible reminder about an actual licence problem and should read as
/// a little more insistent against the page background than the soft sign-in nudge does,
/// without inventing a whole second "danger" palette this app has no other use for.
fn tint() -> COLORREF {
    let weight = if is_dark() { 28 } else { 16 };
    navrail::blend(ACCENT(), DARK_BG(), weight)
}

/// Position the card and its one button. Same shape as `nudge::place`; see that function's
/// doc comment for why a placer closure rather than calling `SetWindowPos` directly.
pub(super) unsafe fn place(
    hwnd: HWND,
    strip_top: i32,
    pane_x: i32,
    pane_w: i32,
    mut put: impl FnMut(i32, i32, i32, i32, i32),
) {
    if !showing() {
        return;
    }
    let h = card_h();
    put(ID_BIZNAG_CARD, pane_x, strip_top, pane_w, h);

    let label = t("biznag_btn");
    let w = nudge::btn_w(hwnd, label, BTN_W_FLOOR);
    let by = strip_top + h - BTN_H - 12;
    let bx = pane_x + pane_w - PAD - w;
    put(ID_BIZNAG_ACTION, bx, by, w, BTN_H);

    // Raise the button above the card explicitly — same z-order fix `nudge::place`
    // documents (the layout pass positions everything with SWP_NOZORDER, so creation
    // order is what decides whether the button paints over the owner-draw card under it).
    if let Ok(c) = GetDlgItem(Some(hwnd), ID_BIZNAG_ACTION) {
        let _ = SetWindowPos(
            c,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// Draw the card: a tinted rounded panel, the wrapped body, no headline.
pub(super) unsafe fn draw_card(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill(hdc, &rc, DARK_BG());

    let bw = s(hwnd, 1).max(1);
    let r = s(hwnd, 8);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let fill_c = tint();
    let border_c = BORDER();
    gdip::with_aa(hdc, |g| {
        let b = gdip::brush(fill_c);
        gdip::fill_round(g, b, rc.left, rc.top, w, h, r);
        gdip::drop_brush(b);
        let p = gdip::pen(border_c, bw);
        gdip::stroke_round(g, p, rc.left, rc.top, w, h, r);
        gdip::drop_pen(p);
    });

    SetBkMode(hdc, TRANSPARENT);
    let pad = s(hwnd, PAD);
    let mut text = wide(&body_text());
    let tn = text.len().saturating_sub(1);
    SelectObject(hdc, HGDIOBJ(crate::win::gui_font_for(hwnd).0));
    SetTextColor(hdc, DARK_TEXT());
    let mut tr = RECT {
        left: rc.left + pad,
        top: rc.top + s(hwnd, BODY_TOP),
        right: rc.right - pad,
        bottom: rc.bottom - s(hwnd, BTN_H + 18),
    };
    DrawTextW(
        hdc,
        &mut text[..tn],
        &mut tr,
        DT_LEFT | DT_WORDBREAK | DT_NOPREFIX,
    );
}

/// Handle a click on the strip's one button. Returns whether the id belonged to it.
pub(super) unsafe fn on_command(hwnd: HWND, id: i32) -> bool {
    if id != ID_BIZNAG_ACTION {
        return false;
    }
    if let Some(ci) = navrail::category_index("nav_licence") {
        navrail::switch_category(hwnd, ci);
    }
    true
}
