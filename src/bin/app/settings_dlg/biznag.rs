//! The Business-licence strip across every Settings page: the evaluation countdown
//! ("7 days left"), the notice that it has ended, the stopped state, the plain "no key"
//! reminder for a copy that once held a licence, and the revoked wording - one card, one
//! sentence chosen by `license::Posture`, and two buttons: "Open licence" (the Licence
//! page, where the key goes in) and "Buy a licence…" (the shop). Sibling of `nudge.rs` -
//! same strip mechanism, same "decided once before the window is built, because page
//! layout runs exactly once" reasoning (see that module's doc comment) - but with **no
//! dismiss**. Persistent-while-unlicensed is the entire point of Business mode (see
//! `license.rs`'s `Posture` doc comments), so unlike `nudge`'s banner this one has no
//! "Not now" and nothing here ever hides it mid-session - the strip goes away only because
//! the NEXT Settings open decides differently (a key was redeemed).

use super::*;
use crate::license::Posture;

/// Top offset of the body text within the card. Smaller than `nudge`'s `BODY_TOP` (36):
/// this card has no headline above the body, just the body then the button row.
const BODY_TOP: i32 = 14;
const BTN_H: i32 = 26;
/// Floor for each button's width — what the English labels need; see `nudge`'s
/// `BTN_W_ACTION` doc comment for why a floor and not a fixed width (a translated label is
/// routinely wider).
const BTN_W_FLOOR: i32 = 120;

thread_local! {
    static SHOWING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    /// The posture the strip is speaking for, decided once in [`decide`].
    static POSTURE: std::cell::Cell<Posture> = const { std::cell::Cell::new(Posture::Silent) };
    static KEY_PREFIX: std::cell::RefCell<String> =
        const { std::cell::RefCell::new(String::new()) };
    /// The clock the sentence was written against, so "{n} days left" is the same number
    /// the rest of the window computed from one snapshot.
    static NOW: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Memo for `card_h`, cleared by `decide` for the same reason `nudge`'s `CARD_H_MEMO`
    /// is: a reopened window may be showing a different language's (longer or shorter)
    /// sentence.
    static CARD_H_MEMO: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
}

/// Ask the licence engine, once, before the window is created. Returns whether the strip
/// will be shown, which is what the caller uses to decide how tall to make the window.
pub(crate) fn decide() -> bool {
    let snap = crate::license::snapshot();
    let showing = snap.posture.wants_reminder();
    SHOWING.with(|s| s.set(showing));
    POSTURE.with(|p| p.set(snap.posture));
    KEY_PREFIX.with(|k| *k.borrow_mut() = snap.key_prefix.clone());
    NOW.with(|n| n.set(snap.now_unix));
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

/// The one sentence for the posture the strip was decided for. Pure over the decided
/// state, so [`body_text_for`] can be pinned by a test without a window.
fn body_text() -> String {
    let posture = POSTURE.with(|p| p.get());
    let key = KEY_PREFIX.with(|k| k.borrow().clone());
    let now = NOW.with(|n| n.get());
    body_text_for(posture, &key, now)
}

/// The strip's sentence per posture. `Silent`/`DowngradeNoticeOnce` never show a strip,
/// so they read as the plain reminder here rather than panicking on an unreachable arm.
pub(super) fn body_text_for(posture: Posture, key: &str, now: u64) -> String {
    use crate::license::days_until;
    match posture {
        Posture::Trial { ends_unix } => {
            t("biznag_body_trial").replace("{n}", &days_until(now, ends_unix).to_string())
        }
        Posture::TrialExpired { locks_unix } => {
            t("biznag_body_expired").replace("{n}", &days_until(now, locks_unix).to_string())
        }
        Posture::Locked { revoked: false } => t("biznag_body_locked").to_string(),
        Posture::Locked { revoked: true } => t("biznag_body_locked_revoked").replace("{key}", key),
        Posture::DeauthorizedLoud {
            locks_unix: Some(d),
        } => {
            format!(
                "{} {}",
                t("biznag_body_revoked").replace("{key}", key),
                t("licence_deauthorized_locks").replace("{date}", &format_unix_date(d))
            )
        }
        Posture::DeauthorizedLoud { locks_unix: None } => {
            t("biznag_body_revoked").replace("{key}", key)
        }
        Posture::BusinessNag | Posture::Silent | Posture::DowngradeNoticeOnce => {
            t("biznag_body").to_string()
        }
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
/// persistent, non-dismissible reminder about an actual licence state and should read as
/// a little more insistent against the page background than the soft sign-in nudge does,
/// without inventing a whole second "danger" palette this app has no other use for. An
/// urgent posture (the evaluation over, the shell stopped) leans harder on the accent.
fn tint() -> COLORREF {
    let urgent = POSTURE.with(|p| p.get()).is_urgent();
    let weight = match (is_dark(), urgent) {
        (true, true) => 44,
        (true, false) => 28,
        (false, true) => 26,
        (false, false) => 16,
    };
    navrail::blend(ACCENT(), DARK_BG(), weight)
}

/// Position the card and its two buttons. Same shape as `nudge::place`; see that function's
/// doc comment for why a placer closure rather than calling `SetWindowPos` directly. The
/// Buy button sits at the far right, "Open licence" to its left.
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

    // Buy first so it lands at the far right; laid out right-to-left by the shared placer.
    let row = [
        (
            ID_BIZNAG_BUY,
            nudge::btn_w(hwnd, t("btn_licence_buy"), BTN_W_FLOOR),
        ),
        (
            ID_BIZNAG_ACTION,
            nudge::btn_w(hwnd, t("biznag_btn"), BTN_W_FLOOR),
        ),
    ];
    for (id, x, y, w, h) in nudge::button_rects(strip_top, pane_x, pane_w, h, &row) {
        put(id, x, y, w, h);
    }

    // Raise the buttons above the card explicitly — same z-order fix `nudge::raise_above`
    // documents (the layout pass positions everything with SWP_NOZORDER, so creation
    // order is what decides whether a button paints over the owner-draw card under it).
    nudge::raise_above(hwnd, &[ID_BIZNAG_ACTION, ID_BIZNAG_BUY]);
}

/// Draw the card: a tinted rounded panel, the wrapped body, no headline.
pub(super) unsafe fn draw_card(hwnd: HWND, d: &DRAWITEMSTRUCT) {
    let hdc = d.hDC;
    let rc = d.rcItem;
    fill(hdc, &rc, DARK_BG());
    let pad = nudge::paint_panel(hwnd, d, tint());

    let mut text = wide(&body_text());
    SelectObject(hdc, HGDIOBJ(crate::win::gui_font_for(hwnd).0));
    SetTextColor(hdc, DARK_TEXT());
    let mut tr = RECT {
        left: rc.left + pad,
        top: rc.top + s(hwnd, BODY_TOP),
        right: rc.right - pad,
        bottom: rc.bottom - s(hwnd, BTN_H + 18),
    };
    nudge::draw_body(hdc, &mut text, &mut tr);
}

/// Handle a click on one of the strip's buttons. Returns whether the id belonged to it.
pub(super) unsafe fn on_command(hwnd: HWND, id: i32) -> bool {
    match id {
        ID_BIZNAG_ACTION => {
            if let Some(ci) = navrail::category_index("nav_licence") {
                navrail::switch_category(hwnd, ci);
            }
            true
        }
        ID_BIZNAG_BUY => {
            crate::win::open_url(crate::license::BUY_URL);
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;

    /// Every posture that shows the strip has a sentence, the countdown is the rounded-up
    /// day count, and the two stories the lock can tell are told apart.
    #[test]
    fn the_strip_speaks_every_posture() {
        let now = 1_760_000_000u64;
        let trial = body_text_for(
            Posture::Trial {
                ends_unix: now + 3 * DAY - 1,
            },
            "",
            now,
        );
        assert!(trial.contains('3'), "{trial}");
        let expired = body_text_for(
            Posture::TrialExpired {
                locks_unix: now + 2 * DAY,
            },
            "",
            now,
        );
        assert!(expired.contains('2'), "{expired}");
        let locked = body_text_for(Posture::Locked { revoked: false }, "", now);
        let locked_revoked = body_text_for(Posture::Locked { revoked: true }, "esk_A1B2", now);
        assert_ne!(locked, locked_revoked);
        assert!(locked_revoked.contains("esk_A1B2"));
        let revoked = body_text_for(
            Posture::DeauthorizedLoud { locks_unix: None },
            "esk_A1B2",
            now,
        );
        let revoked_dated = body_text_for(
            Posture::DeauthorizedLoud {
                locks_unix: Some(now + DAY),
            },
            "esk_A1B2",
            now,
        );
        assert!(
            revoked_dated.starts_with(&revoked),
            "the dated form extends the plain one"
        );
        assert!(revoked_dated.len() > revoked.len());
        assert_eq!(
            body_text_for(Posture::BusinessNag, "", now),
            body_text_for(Posture::Silent, "", now),
            "the strip never shows for Silent, so it reads as the plain reminder"
        );
    }
}
