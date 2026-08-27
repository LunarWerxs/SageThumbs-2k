//! The "you could be signed in" banner: the settings window's half of it.
//!
//! `crate::nudge` owns the decision and the memory; this owns the pixels and the clicks. The
//! division matters because the decision has to be identical across every LunarWerx app and the
//! pixels cannot be — SageThumbs draws its own chrome with GDI+, so nothing about the shared
//! renderer the web apps use applies here.
//!
//! ## Where it sits, and why not in the pane
//!
//! Between the content pane and the footer, in the strip the retired sponsor banner used to
//! occupy, and the window is created taller by exactly that much when it is showing. That position
//! is not an aesthetic preference — it is the only one available. Page content is laid out ONCE at
//! build time from a fixed row list, guarded by a `debug_assert` that every page still clears the
//! footer; inserting a ~90px card at the top of a pane whose fullest page already sits within a
//! few rows of the bottom would trip that assert at the design size, and there is no relayout path
//! to recover with. Growing the window instead leaves every page's rhythm exactly as it was.
//!
//! ## Why it is decided once, at open
//!
//! Same reason: the layout is not re-runnable, so the banner cannot appear mid-session. The ask is
//! made when the Settings window opens — a moment that IS about changing settings, which is what
//! the account keeps — and holds for that window's lifetime.
//!
//! ## What the three buttons mean
//!
//! Identical to the web banner every other LunarWerx app shows (`nudge-banner.ts`) and to
//! QuickDictate's: "Not now" is engagement and does NOT count toward the two-strike stop; "Don't
//! ask again" is a real opt-out offered on the FIRST ask, never withheld until the third; and
//! closing the window without answering counts as a decline, which the engine settles at the next
//! launch rather than here.

use std::cell::RefCell;

use super::*;
use crate::gdip;
use crate::nudge_engine::{Ask, Cadence, Outcome};
use windows::Win32::Graphics::Gdi::DT_WORDBREAK;

/// Design-pixel height of the whole strip, including the gaps above and below the card.
///
/// The window is created this much taller when a banner is live, so the content pane keeps
/// precisely the room it has without one.
pub(super) const STRIP_H: i32 = CARD_H + 16;

/// The card itself inside that strip.
///
/// Sized for the real copy rather than guessed: headline, TWO wrapped lines of body at the card's
/// full width, then the button row. The first cut was 76 and stacked the same content, which
/// clipped the body mid-sentence - a failure that is invisible in the code and obvious only in a
/// capture, which is the whole reason `scripts/nudge-shot.ps1` exists.
const CARD_H: i32 = 116;

const BTN_H: i32 = 26;
const BTN_W_ACTION: i32 = 92;
const BTN_W_LATER: i32 = 80;
const BTN_W_NEVER: i32 = 116;
const PAD: i32 = 14;

pub(super) const LATER_LABEL: &str = "Not now";
pub(super) const NEVER_LABEL: &str = "Don't ask again";

thread_local! {
    /// The ask on screen, or `None` when the engine declined to ask (the usual case).
    ///
    /// Thread-local rather than a global: the settings window owns one UI thread and everything
    /// that reads this runs on it, so there is no lock to forget and no cross-thread state to
    /// reason about.
    static ASK: RefCell<Option<Ask>> = const { RefCell::new(None) };
}

/// Ask the engine, once, before the window is created. Returns whether a banner will be shown,
/// which is what the caller uses to decide how tall to make the window.
pub(crate) fn decide() -> bool {
    // `settings-changed` selects the copy and rides the attribution link. It is the honest trigger
    // here: the user opened the settings window, and settings are exactly what an account keeps.
    let ask = crate::nudge::consider("settings-changed");
    let showing = ask.is_some();
    ASK.with(|a| *a.borrow_mut() = ask);
    showing
}

/// Whether a banner is live for this window.
pub(super) fn showing() -> bool {
    ASK.with(|a| a.borrow().is_some())
}

/// How much taller the settings window is because of the banner (design px; 0 when there is none).
pub(crate) fn extra_height() -> i32 {
    if showing() {
        STRIP_H
    } else {
        0
    }
}

/// The primary button's label, which the engine chooses per campaign.
pub(super) fn action_label() -> String {
    ASK.with(|a| {
        a.borrow()
            .as_ref()
            .map(|ask| ask.action_label.clone())
            .unwrap_or_else(|| "Sign in".into())
    })
}

/// Position the card and its three buttons. `strip_top` is the top of the reserved strip in design
/// px; `pane_x` / `pane_w` match the content pane so the card lines up with everything above it.
///
/// Takes a placer rather than calling `SetWindowPos` itself so that DPI scaling stays in the one
/// place that already does it correctly (`apply_v3_layout`'s `place`).
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
    put(ID_NUDGE_CARD, pane_x, strip_top, pane_w, CARD_H);

    // Right-aligned inside the card, bottom row, laid out right-to-left from the card's inner edge
    // so the primary action is the one nearest the corner the eye lands on.
    let by = strip_top + CARD_H - BTN_H - 12;
    let mut right = pane_x + pane_w - PAD;
    for (id, w) in [
        (ID_NUDGE_ACTION, BTN_W_ACTION),
        (ID_NUDGE_LATER, BTN_W_LATER),
        (ID_NUDGE_NEVER, BTN_W_NEVER),
    ] {
        right -= w;
        put(id, right, by, w, BTN_H);
        right -= 8;

        // Raise each button above the card EXPLICITLY. The buttons overlap an owner-draw STATIC
        // and the layout pass positions everything with SWP_NOZORDER, so whichever way the shell
        // happened to order the siblings at creation is what decides whether they are visible at
        // all - and it ordered them UNDER the card, which rendered as a blank panel with no
        // buttons in it and no error anywhere.
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
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
}

/// Hide every control the banner owns. Called when it is answered: the window cannot re-lay-out
/// itself, so the card simply disappears and leaves the gap it was occupying rather than
/// reflowing the page under the user's cursor.
unsafe fn hide(hwnd: HWND) {
    for id in [
        ID_NUDGE_CARD,
        ID_NUDGE_ACTION,
        ID_NUDGE_LATER,
        ID_NUDGE_NEVER,
    ] {
        if let Ok(c) = GetDlgItem(Some(hwnd), id) {
            let _ = ShowWindow(c, SW_HIDE);
        }
    }
}

/// The card's tint.
///
/// Blended toward the page background rather than picked as two hand-tuned constants: the accent
/// is the same colour in both themes and the background is not, so one expression gives a panel
/// that reads as "faintly accented" on a dark page and on a light one. A saturated fill would read
/// as an error state, which this is the opposite of.
fn tint() -> COLORREF {
    let a = ACCENT().0;
    let bg = DARK_BG().0;
    let weight = if is_dark() { 22u32 } else { 12u32 };
    let mix = |shift: u32| {
        let ac = (a >> shift) & 0xFF;
        let bc = (bg >> shift) & 0xFF;
        ((ac * weight + bc * (100 - weight)) / 100) & 0xFF
    };
    COLORREF(mix(0) | (mix(8) << 8) | (mix(16) << 16))
}

/// Draw the card: a tinted rounded panel, a bold headline, and the body wrapped beside the buttons.
pub(super) unsafe fn draw_card(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    // Paint the page background first: the panel rounds its corners away, and without this the
    // pixels outside the curve keep whatever was in the DC.
    fill(hdc, &rc, DARK_BG());

    let Some((headline, body)) = ASK.with(|a| {
        a.borrow()
            .as_ref()
            .map(|ask| (ask.headline.clone(), ask.body.clone()))
    }) else {
        return;
    };

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

    let mut head = wide(&headline);
    let hn = head.len().saturating_sub(1);
    SelectObject(hdc, HGDIOBJ(crate::win::gui_font_header(hwnd).0));
    SetTextColor(hdc, DARK_TEXT());
    let mut hr = RECT {
        left: rc.left + pad,
        top: rc.top + s(hwnd, 11),
        right: rc.right - pad,
        bottom: rc.top + s(hwnd, 33),
    };
    DrawTextW(
        hdc,
        &mut head[..hn],
        &mut hr,
        DT_LEFT | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );

    // Body: wrapped across the card's FULL width, bottomed out just above the button row so a long
    // sentence can never run underneath the buttons. Squeezing it into the column to their LEFT
    // instead - the first attempt - leaves ~180px for a 90-character sentence and clips it
    // mid-word. Giving the buttons their own row is what buys the text its width.
    let mut text = wide(&body);
    let tn = text.len().saturating_sub(1);
    SelectObject(hdc, HGDIOBJ(crate::win::gui_font_for(hwnd).0));
    SetTextColor(hdc, HEADER_TEXT());
    let mut tr = RECT {
        left: rc.left + pad,
        top: rc.top + s(hwnd, 36),
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

/// Handle a click on one of the three buttons. Returns whether the id belonged to the banner.
pub(super) unsafe fn on_command(hwnd: HWND, id: i32) -> bool {
    let outcome = match id {
        ID_NUDGE_ACTION => Outcome::Accepted,
        ID_NUDGE_LATER => Outcome::Snoozed,
        ID_NUDGE_NEVER => Outcome::SetCadence(Cadence::Never),
        _ => return false,
    };
    crate::nudge::record(outcome);
    ASK.with(|a| *a.borrow_mut() = None);
    hide(hwnd);

    if id == ID_NUDGE_ACTION {
        // Start the app's OWN sign-in rather than opening a web page and hoping they come back and
        // find the Data & Backup page. The offer is already built; the banner's only job was to
        // say so. `begin_connect` is the exact path the sync button on that page runs once its
        // confirmation is answered — the banner has already explained, so it does not ask twice.
        super::sync::begin_connect(hwnd);
        // Land them on the page that owns sync, so the status they are about to see ("connecting",
        // then the account) is on screen rather than nine pages away.
        if let Some(ci) = super::navrail::category_index("nav_databackup") {
            super::navrail::switch_category(hwnd, ci);
        }
    }
    true
}
