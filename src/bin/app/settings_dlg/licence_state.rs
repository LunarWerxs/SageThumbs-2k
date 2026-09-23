//! The licence/state-line formatters: the Licence page's status line, the updates-window
//! line + Renew-button visibility, the shared reminder sentence (startup toast / tray
//! balloon / daily one-shot), and the plain `format_unix_date` helper they all lean on.
//! Split out of `mod.rs` — pure functions over a [`crate::license::LicenceSnapshot`], no
//! window/control access, so they're safe to unit-test without any HWND.

use super::*;

/// The licence-state line — "Licensed, last verified …" / "No licence key entered" /
/// "Licence revoked (key esk_XXXX)" / "Personal use, no licence needed" — the ONE formatter
/// for it, shared between this window's status line and the About box's licence line (see
/// `about.rs`) so the two surfaces cannot silently drift into disagreeing over what the exact
/// same [`crate::license::snapshot`] means.
pub(crate) fn licence_state_line(snap: &crate::license::LicenceSnapshot) -> String {
    // A REVOKED key outranks the installer's answer for the same reason a live one does: a
    // Personal copy whose business key was taken back must say so. Checked before the Personal
    // line below, which a revoked copy (no longer entitled) would otherwise fall into and read
    // "Personal use, no licence needed" - the revocation silently vanishing (owner test, 2026-09-11).
    if !snap.key_prefix.is_empty() && snap.last_status == "revoked" {
        return revoked_state_line(snap);
    }
    // The evaluation and its lock, before the plain "no key" line: a Business copy with no
    // key is on a clock, and the line says where on it this machine stands.
    match snap.posture {
        crate::license::Posture::Trial { ends_unix } => {
            return t("licence_state_trial")
                .replace(
                    "{n}",
                    &crate::license::days_until(snap.now_unix, ends_unix).to_string(),
                )
                .replace(
                    "{date}",
                    &format_unix_date(ends_unix.saturating_add(crate::license::LOCK_GRACE_SECS)),
                );
        }
        crate::license::Posture::TrialExpired { locks_unix } => {
            return t("licence_state_expired").replace("{date}", &format_unix_date(locks_unix));
        }
        crate::license::Posture::Locked { revoked: false } => {
            return t("licence_state_locked").to_string();
        }
        _ => {}
    }
    // Personal means "no licence needed" ONLY while this machine is not actually licensed. A key
    // redeemed on this copy outranks the installer's answer - `license::posture` already treats it
    // as a live business licence - so the status line must say so too, or the page contradicts
    // itself ("licence is active" beside "Personal use, no licence needed"). A stale breadcrumb from
    // a former Business install is NOT entitled, so it still reads Personal here.
    if snap.mode == crate::license::Mode::Personal && !snap.entitled {
        return t("licence_state_personal").to_string();
    }
    if snap.key_prefix.is_empty() {
        return t("licence_state_none").to_string();
    }
    // E05 audit: a certificate nearing its own `exp` gets its own line rather than the
    // ordinary "Licensed" one, but ONLY when the certificate is actually what licenses this
    // machine (`cert_expires_unix` is `None` whenever a relay verification already grants
    // it - see `entitlement_and_cert_expiry`). Compares against `snap.now_unix`, the SAME
    // instant the snapshot was built with, never a fresh clock read.
    if let Some(expires) = snap.cert_expires_unix {
        let remaining = expires.saturating_sub(snap.now_unix as i64);
        if remaining >= 0 && (remaining as u64) <= crate::license::CERT_EXPIRY_WARNING_SECS {
            let expires_unix = u64::try_from(expires).unwrap_or(0);
            return t("licence_state_cert_expiring")
                .replace("{date}", &format_unix_date(expires_unix));
        }
    }
    t("licence_state_licensed").replace("{date}", &format_unix_date(snap.last_positive_unix))
}

/// Builds the revoked-key state line for [`licence_state_line`]: the plain revocation
/// sentence, the relay's reason when recorded, then the lock from the current posture.
fn revoked_state_line(snap: &crate::license::LicenceSnapshot) -> String {
    let mut line = t("licence_state_revoked").replace("{key}", &snap.key_prefix);
    if let Some(why) = licence_reason_line(&snap.last_reason) {
        line.push(' ');
        line.push_str(why);
    }
    // The lock, from the same phase the shell reads: the date it lands, or that it has.
    match snap.posture {
        crate::license::Posture::DeauthorizedLoud {
            locks_unix: Some(locks),
        } => {
            line.push(' ');
            line.push_str(
                &t("licence_deauthorized_locks").replace("{date}", &format_unix_date(locks)),
            );
        }
        crate::license::Posture::Locked { revoked: true } => {
            line.push(' ');
            line.push_str(t("licence_state_locked"));
        }
        _ => {}
    }
    line
}

/// How long before the updates window ends the Licence page starts offering the renewal:
/// 60 days. Long enough that a business can put it through a purchase process before it
/// lapses, short enough that the button is not simply part of the furniture - which is what
/// it would become if it were always visible, and a button nobody needs yet is a nag.
pub(crate) const RENEW_NOTICE_SECS: u64 = 60 * 24 * 60 * 60;

/// Does this snapshot describe a machine whose UPDATES WINDOW is a real, current fact worth
/// showing? A window end on record, a key that redeemed it, and no revocation - a revoked
/// seat needs a licence, not another twelve months of updates on one it no longer holds.
fn has_updates_window(snap: &crate::license::LicenceSnapshot) -> Option<u64> {
    if snap.key_prefix.is_empty() || snap.last_status == "revoked" {
        return None;
    }
    snap.maint_unix
}

/// The updates-window line under the licence state: "Updates until <date>" while the window
/// is open, "Updates ended <date>" once it has closed, `None` when there is no window on
/// record (a personal copy, or a licence whose updates never lapse) and the line stays blank.
///
/// ⛔ Deliberately NOT folded into [`licence_state_line`]. The licence is perpetual and the
/// updates window is not; one sentence carrying both is how a customer reads "ended" as "my
/// licence expired", which is the single wrong idea this whole feature exists to prevent.
pub(crate) fn licence_updates_line(snap: &crate::license::LicenceSnapshot) -> Option<String> {
    let ends = has_updates_window(snap)?;
    let key = if ends >= snap.now_unix {
        "licence_updates_until"
    } else {
        "licence_updates_ended"
    };
    Some(t(key).replace("{date}", &format_unix_date(ends)))
}

/// Should the "Renew updates (US$29)" button be visible? Only for a machine that actually
/// holds a window, and only once that window is within [`RENEW_NOTICE_SECS`] of closing or
/// has already closed. Pure over the snapshot so both boundaries are pinned by tests.
pub(crate) fn renew_button_visible(snap: &crate::license::LicenceSnapshot) -> bool {
    let Some(ends) = has_updates_window(snap) else {
        return false;
    };
    // Past the end: `saturating_sub` is 0, which is inside the window by definition. Before
    // it: how long is left.
    ends.saturating_sub(snap.now_unix) <= RENEW_NOTICE_SECS
}

/// The one sentence every reminder surface speaks for a posture that wants one - the
/// startup toast or message box, the resident helper's tray balloon, and the daily
/// one-shot's toast - shared so three surfaces cannot drift into three descriptions of one
/// clock. Empty for the two postures that want no reminder. Pure over the snapshot.
pub(crate) fn licence_reminder_body(snap: &crate::license::LicenceSnapshot) -> String {
    use crate::license::{days_until, Posture};
    let now = snap.now_unix;
    match snap.posture {
        Posture::Silent | Posture::DowngradeNoticeOnce => String::new(),
        Posture::BusinessNag => t("licence_nag_toast_body").to_string(),
        Posture::Trial { ends_unix } => {
            t("licence_nag_toast_trial").replace("{n}", &days_until(now, ends_unix).to_string())
        }
        Posture::TrialExpired { locks_unix } => {
            t("licence_expired_notice").replace("{n}", &days_until(now, locks_unix).to_string())
        }
        Posture::Locked { revoked: false } => t("licence_locked_notice").to_string(),
        Posture::Locked { revoked: true } => {
            t("biznag_body_locked_revoked").replace("{key}", &snap.key_prefix)
        }
        Posture::DeauthorizedLoud { locks_unix } => {
            // Leads with WHY when the relay said (a seat the holder ejected reads very
            // differently from a licence that ended), then the date the shell stops.
            let why = licence_reason_line(&snap.last_reason)
                .map(|w| format!("{w} "))
                .unwrap_or_default();
            let notice = t("licence_deauthorized_notice").replace("{key}", &snap.key_prefix);
            let locks = locks_unix
                .map(|d| {
                    format!(
                        " {}",
                        t("licence_deauthorized_locks").replace("{date}", &format_unix_date(d))
                    )
                })
                .unwrap_or_default();
            format!("{why}{notice}{locks}")
        }
    }
}

/// The Licence page's index for `--tab`, resolved BY NAME like [`quick_preview_page`]:
/// a literal here has silently re-pointed at the wrong page before.
pub(crate) fn licence_page() -> usize {
    navrail::category_index("nav_licence").unwrap_or(NAV_CATEGORY_COUNT - 1)
}

/// The human sentence for the relay's `reason` behind a revocation (`seat_revoked`: the
/// licence holder ejected this machine; `contract_ended`: the licence itself was cancelled),
/// or `None` for anything else, including the older breadcrumbs that never recorded one.
/// Shared by the Licence page's state line and the startup deauthorised notice, so a user
/// reads the same explanation in both places. The 2026-09-04 audit found the relay had been
/// sending this since 2026-09-02 and the app dropped it on the floor.
pub(crate) fn licence_reason_line(reason: &str) -> Option<&'static str> {
    match reason {
        "seat_revoked" => Some(t("licence_reason_seat_revoked")),
        "contract_ended" => Some(t("licence_reason_contract_ended")),
        _ => None,
    }
}

/// The "how did this copy get here" line — "Installed for business use. Reinstall to
/// change." / "Installed for personal use." / "Portable copy." — shown above
/// [`licence_state_line`] on the Licence page. Portable wins over the recorded [`Mode`]:
/// a portable copy never saw the installer's Personal/Business question (see
/// `license::read_mode`'s doc comment on how it can still end up `Business` after a
/// redeemed key), and "Reinstall to change" would be nonsense advice for a copy that was
/// never installed.
///
/// A LIVE key outranks all three: once a business key is active on this copy, the installer's
/// answer no longer describes it, so the line names the licence and its key instead. A Personal
/// install that redeems a business key is a business install from then on (owner, 2026-09-11).
///
/// [`Mode`]: crate::license::Mode
pub(super) fn licence_mode_line(snap: &crate::license::LicenceSnapshot) -> String {
    if snap.entitled && !snap.key_prefix.is_empty() {
        return t("licence_mode_licensed").replace("{key}", &snap.key_prefix);
    }
    if sagethumbs2k_core::settings::portable() {
        return t("licence_mode_portable").to_string();
    }
    match snap.mode {
        crate::license::Mode::Business => t("licence_mode_business").to_string(),
        crate::license::Mode::Personal => t("licence_mode_personal").to_string(),
    }
}

/// The big title over the Licence page: the licence this copy actually holds, not the page's
/// name. "Business licence" once a business key is live (whatever the installer was told),
/// "Personal licence" on a Personal copy without one - a revoked key included - and the plain
/// page name on a Business install with no key yet (owner, 2026-09-11). Only the header uses
/// this; the nav rail and search keep the page name so the page is still found by it.
pub(crate) fn licence_page_title(snap: &crate::license::LicenceSnapshot) -> &'static str {
    if snap.entitled && !snap.key_prefix.is_empty() {
        return t("licence_title_business");
    }
    if snap.mode == crate::license::Mode::Personal {
        return t("licence_title_personal");
    }
    t("nav_licence")
}

/// `unix_secs` (0 = unknown) as "YYYY-MM-DD" in local time — the same FILETIME plumbing
/// `preview::infocard::modified_string` uses for a file's mtime, just date-only (the licence
/// line has no use for a time-of-day), through the shared `sagethumbs2k_core::unixtime`; no
/// chrono/time dependency for one line.
pub(crate) fn format_unix_date(unix_secs: u64) -> String {
    if unix_secs == 0 {
        return String::new();
    }
    sagethumbs2k_core::unixtime::local_date(unix_secs).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-build a [`crate::license::LicenceSnapshot`] for `licence_state_line` tests.
    /// Module-scope (not nested in one test fn) so both the four-states test below and the
    /// E05 certificate-expiry tests can share it.
    fn snap(
        mode: crate::license::Mode,
        key_prefix: &str,
        last_status: &str,
        last_positive_unix: u64,
    ) -> crate::license::LicenceSnapshot {
        snap_at(mode, key_prefix, last_status, last_positive_unix, None, 0)
    }

    /// [`snap`] plus the two E05 fields, for the certificate-expiry tests below.
    fn snap_at(
        mode: crate::license::Mode,
        key_prefix: &str,
        last_status: &str,
        last_positive_unix: u64,
        cert_expires_unix: Option<i64>,
        now_unix: u64,
    ) -> crate::license::LicenceSnapshot {
        crate::license::LicenceSnapshot {
            mode,
            // `Silent` here: the four original states derive from `mode`/`key_prefix`/
            // `last_status` alone; the evaluation-and-lock states are the posture arms
            // `the_state_line_speaks_the_evaluation_and_the_lock` sets explicitly.
            posture: crate::license::Posture::Silent,
            key_prefix: key_prefix.to_string(),
            last_positive_unix,
            last_status: last_status.to_string(),
            last_reason: String::new(),
            cert_expires_unix,
            maint_unix: None,
            now_unix,
            // Not licensed unless a test says otherwise - the pre-fix meaning of every snapshot.
            entitled: false,
        }
    }

    /// The evaluation-and-lock states on the Licence page's state line and in the shared
    /// reminder sentence: each posture gets its own words, the countdown is the rounded-up
    /// day count, and a revoked copy's line carries the lock date once the app knows it.
    #[test]
    fn the_state_line_and_the_reminder_speak_the_evaluation_and_the_lock() {
        use crate::license::{Mode, Posture};
        const DAY: u64 = 24 * 60 * 60;
        let now = 1_760_000_000u64;
        let mut s = snap_at(Mode::Business, "", "", 0, None, now);

        s.posture = Posture::Trial {
            ends_unix: now + 2 * DAY + 1,
        };
        let trial = licence_state_line(&s);
        assert!(trial.contains('3'), "{trial}");
        assert!(licence_reminder_body(&s).contains('3'));

        s.posture = Posture::TrialExpired {
            locks_unix: now + DAY,
        };
        let expired = licence_state_line(&s);
        assert_ne!(expired, trial);
        assert!(licence_reminder_body(&s).contains('1'));

        s.posture = Posture::Locked { revoked: false };
        let locked = licence_state_line(&s);
        assert_ne!(locked, expired);
        assert!(!licence_reminder_body(&s).is_empty());

        // Revoked: the plain line, then the same line with the lock date, then stopped.
        let mut r = snap_at(
            Mode::Business,
            "esk_A1B2",
            "revoked",
            now - 30 * DAY,
            None,
            now,
        );
        r.posture = Posture::DeauthorizedLoud { locks_unix: None };
        let plain = licence_state_line(&r);
        r.posture = Posture::DeauthorizedLoud {
            locks_unix: Some(now + DAY),
        };
        let dated = licence_state_line(&r);
        assert!(
            dated.starts_with(&plain) && dated.len() > plain.len(),
            "{dated}"
        );
        assert!(licence_reminder_body(&r).len() > plain.len());
        r.posture = Posture::Locked { revoked: true };
        let stopped = licence_state_line(&r);
        assert!(stopped.starts_with(&plain) && stopped != dated, "{stopped}");
        assert!(licence_reminder_body(&r).contains("esk_A1B2"));

        // The two silent postures have no reminder sentence at all.
        s.posture = Posture::Silent;
        assert!(licence_reminder_body(&s).is_empty());
        s.posture = Posture::DowngradeNoticeOnce;
        assert!(licence_reminder_body(&s).is_empty());
    }

    /// `licence_state_line` given a hand-built snapshot for each of the four states it must
    /// tell apart — the same four the Settings status line and the About box's line both
    /// show. Pure over its argument (no registry, no file, no network), so every boundary
    /// pins without touching the real breadcrumb.
    /// A licensed snapshot with an updates window `ends` and the clock at `now`, for the
    /// updates-line and renew-button tests below.
    fn snap_window(ends: Option<u64>, now: u64) -> crate::license::LicenceSnapshot {
        let mut s = snap_at(
            crate::license::Mode::Business,
            "esk_A1B2",
            "active",
            now.saturating_sub(3600),
            None,
            now,
        );
        s.maint_unix = ends;
        s
    }

    /// The updates line: "until" while open (INCLUSIVE at the boundary - the last day is
    /// still a day you have), "ended" once past, and blank when there is no window at all.
    ///
    /// The strings are asserted to be PRESENT and DISTINCT rather than hard-coded here: a
    /// test that pins English text just re-types `en.toml` and fails on every wording change,
    /// while a missing key would make both branches render identically, which is the failure
    /// that actually matters.
    #[test]
    fn licence_updates_line_says_until_or_ended_and_nothing_without_a_window() {
        let now = 1_800_000_000u64;
        let day = 24 * 60 * 60;

        assert!(!t("licence_updates_until").is_empty(), "string is missing");
        assert!(!t("licence_updates_ended").is_empty(), "string is missing");
        assert_ne!(t("licence_updates_until"), t("licence_updates_ended"));
        assert!(t("licence_updates_until").contains("{date}"));
        assert!(t("licence_updates_ended").contains("{date}"));

        let open = licence_updates_line(&snap_window(Some(now + 30 * day), now)).expect("open");
        assert_eq!(
            open,
            t("licence_updates_until").replace("{date}", &format_unix_date(now + 30 * day))
        );
        // The boundary belongs to the customer.
        assert!(licence_updates_line(&snap_window(Some(now), now))
            .expect("boundary")
            .starts_with(t("licence_updates_until").split("{date}").next().unwrap()));

        let past = licence_updates_line(&snap_window(Some(now - day), now)).expect("closed");
        assert_eq!(
            past,
            t("licence_updates_ended").replace("{date}", &format_unix_date(now - day))
        );

        assert_eq!(licence_updates_line(&snap_window(None, now)), None);
        // A revoked seat's window is not a fact worth showing: it needs a licence, not
        // another twelve months of updates on one it no longer holds.
        let mut revoked = snap_window(Some(now + 30 * day), now);
        revoked.last_status = "revoked".into();
        assert_eq!(licence_updates_line(&revoked), None);
        // Neither is a copy that never redeemed anything.
        let mut nokey = snap_window(Some(now + 30 * day), now);
        nokey.key_prefix = String::new();
        assert_eq!(licence_updates_line(&nokey), None);
    }

    /// The Renew button appears inside the last 60 days and stays visible after the window
    /// closes; before that it is hidden, because a button offering to buy something nobody
    /// needs yet is just a nag.
    #[test]
    fn renew_button_appears_only_near_or_past_the_window_end() {
        let now = 1_800_000_000u64;
        let day = 24 * 60 * 60;

        assert!(!renew_button_visible(&snap_window(
            Some(now + 61 * day),
            now
        )));
        assert!(renew_button_visible(&snap_window(
            Some(now + RENEW_NOTICE_SECS),
            now
        )));
        assert!(renew_button_visible(&snap_window(Some(now + day), now)));
        assert!(renew_button_visible(&snap_window(Some(now), now)));
        assert!(renew_button_visible(&snap_window(
            Some(now - 365 * day),
            now
        )));

        assert!(
            !renew_button_visible(&snap_window(None, now)),
            "no window, no button"
        );
        let mut revoked = snap_window(Some(now), now);
        revoked.last_status = "revoked".into();
        assert!(!renew_button_visible(&revoked));
    }

    /// Every string the renewal path renders exists in the shipped English table, and each
    /// one still carries the placeholder its call site substitutes. A missing key renders as
    /// an empty control or a key name on screen, which no other test here would notice.
    #[test]
    fn every_renewal_string_exists_and_keeps_its_placeholders() {
        for key in [
            "btn_licence_renew",
            "tip_licence_renew",
            "upd_renew_title",
            "btn_renew",
            "btn_not_now",
            "upd_toast_title",
            "about_update_outside",
        ] {
            assert!(!t(key).is_empty(), "{key} is missing from the locale table");
        }
        for (key, placeholders) in [
            ("upd_outside_window", &["{ver}", "{date}"][..]),
            ("upd_outside_toast", &["{ver}", "{date}"][..]),
            ("upd_toast_body", &["{ver}"][..]),
        ] {
            let s = t(key);
            assert!(!s.is_empty(), "{key} is missing from the locale table");
            for p in placeholders {
                assert!(s.contains(p), "{key} lost its {p} placeholder");
            }
        }
    }

    #[test]
    fn licence_state_line_covers_all_four_states() {
        // A revocation WITH the relay's reason says why; an unknown token adds nothing.
        let mut why = snap(
            crate::license::Mode::Business,
            "esk_A1B2",
            "revoked",
            1_700_000_000,
        );
        why.last_reason = "contract_ended".into();
        assert_eq!(
            licence_state_line(&why),
            format!(
                "{} {}",
                t("licence_state_revoked").replace("{key}", "esk_A1B2"),
                t("licence_reason_contract_ended")
            )
        );
        why.last_reason = "something_new".into();
        assert_eq!(
            licence_state_line(&why),
            t("licence_state_revoked").replace("{key}", "esk_A1B2")
        );

        // Personal wins over a stale key/status from a former Business install - it is not
        // entitled any more (the downgrade notice, not this line, owns that story).
        assert_eq!(
            licence_state_line(&snap(
                crate::license::Mode::Personal,
                "esk_A1B2",
                "active",
                1
            )),
            t("licence_state_personal")
        );
        // ⛔ BUT A KEY REDEEMED ON THIS PERSONAL COPY, AND LIVE, IS A LICENCE. The page used to
        // show "Personal use, no licence needed" beside a green "licence is active" (2026-09-11).
        let mut redeemed_here = snap(
            crate::license::Mode::Personal,
            "esk_A1B2",
            "active",
            1_700_000_000,
        );
        redeemed_here.entitled = true;
        assert_eq!(
            licence_state_line(&redeemed_here),
            t("licence_state_licensed").replace("{date}", &format_unix_date(1_700_000_000))
        );
        // ⛔ AND A KEY REVOKED ON THIS PERSONAL COPY SAYS SO. It is no longer entitled, so before
        // 2026-09-11 it fell into the Personal branch and the revocation vanished from the page.
        assert_eq!(
            licence_state_line(&snap(
                crate::license::Mode::Personal,
                "esk_A1B2",
                "revoked",
                1_700_000_000
            )),
            t("licence_state_revoked").replace("{key}", "esk_A1B2")
        );
        // Business, never redeemed anything.
        assert_eq!(
            licence_state_line(&snap(crate::license::Mode::Business, "", "", 0)),
            t("licence_state_none")
        );
        // Business, the breadcrumb's last recorded status is a revocation.
        assert_eq!(
            licence_state_line(&snap(
                crate::license::Mode::Business,
                "esk_A1B2",
                "revoked",
                1_700_000_000
            )),
            t("licence_state_revoked").replace("{key}", "esk_A1B2")
        );
        // Business, a key on record and no revocation — "Licensed", with the verify date.
        let licensed = licence_state_line(&snap(
            crate::license::Mode::Business,
            "esk_A1B2",
            "active",
            1_700_000_000,
        ));
        assert_eq!(
            licensed,
            t("licence_state_licensed").replace("{date}", &format_unix_date(1_700_000_000))
        );
    }

    /// The mode line names the business licence once a key is live on this copy, whatever the
    /// installer was told, and falls back to the installer's own answer the moment it is not.
    #[test]
    fn licence_mode_line_names_the_business_licence_once_a_key_is_live() {
        let mut live = snap(
            crate::license::Mode::Personal,
            "esk_A1B2",
            "active",
            1_700_000_000,
        );
        live.entitled = true;
        assert_eq!(
            licence_mode_line(&live),
            t("licence_mode_licensed").replace("{key}", "esk_A1B2")
        );
        // A portable test runner has no installer answer to fall back to; the live-key half above
        // is the part that matters and it has already run.
        if sagethumbs2k_core::settings::portable() {
            return;
        }
        assert_eq!(
            licence_mode_line(&snap(
                crate::license::Mode::Personal,
                "esk_A1B2",
                "revoked",
                1_700_000_000
            )),
            t("licence_mode_personal")
        );
    }

    /// The Licence page's big title names the licence: Business once a key is live, Personal on a
    /// Personal copy without one (a revoked key included), the plain page name otherwise.
    #[test]
    fn licence_page_title_names_the_licence_this_copy_holds() {
        let mut live = snap(
            crate::license::Mode::Personal,
            "esk_A1B2",
            "active",
            1_700_000_000,
        );
        live.entitled = true;
        assert_eq!(licence_page_title(&live), t("licence_title_business"));
        assert_eq!(
            licence_page_title(&snap(
                crate::license::Mode::Personal,
                "esk_A1B2",
                "revoked",
                1_700_000_000
            )),
            t("licence_title_personal")
        );
        assert_eq!(
            licence_page_title(&snap(crate::license::Mode::Business, "", "", 0)),
            t("nav_licence")
        );
    }

    /// E05 audit: a certificate that licenses the machine (relay unreachable or lapsed)
    /// gets its own line once it is inside `CERT_EXPIRY_WARNING_SECS` of its own `exp`,
    /// pinned on both sides of that boundary the way the grace window is pinned elsewhere
    /// in this codebase. Against the pre-E05 `licence_state_line` (no `cert_expires_unix`
    /// field to read at all) this test fails to compile, which is the strongest possible
    /// "fails against the old code."
    #[test]
    fn licence_state_line_shows_certificate_expiry_only_inside_the_warning_window() {
        let warning = crate::license::CERT_EXPIRY_WARNING_SECS as i64;
        let now = 1_700_000_000i64;

        // Far from expiry: the ordinary "Licensed" line, using whatever last_positive_unix
        // the snapshot carries (a cert-only-licensed machine may have none from the relay).
        let far = snap_at(
            crate::license::Mode::Business,
            "esk_A1B2",
            "active",
            1_650_000_000,
            Some(now + warning + 1),
            now as u64,
        );
        assert_eq!(
            licence_state_line(&far),
            t("licence_state_licensed").replace("{date}", &format_unix_date(1_650_000_000)),
            "outside the window, the certificate must not preempt the ordinary line"
        );

        // Exactly at the boundary: inside (inclusive).
        let boundary = snap_at(
            crate::license::Mode::Business,
            "esk_A1B2",
            "active",
            1_650_000_000,
            Some(now + warning),
            now as u64,
        );
        assert_eq!(
            licence_state_line(&boundary),
            t("licence_state_cert_expiring")
                .replace("{date}", &format_unix_date((now + warning) as u64)),
            "the boundary instant itself counts as inside the window"
        );

        // Well inside the window.
        let soon = snap_at(
            crate::license::Mode::Business,
            "esk_A1B2",
            "active",
            1_650_000_000,
            Some(now + 10),
            now as u64,
        );
        assert_eq!(
            licence_state_line(&soon),
            t("licence_state_cert_expiring")
                .replace("{date}", &format_unix_date((now + 10) as u64))
        );

        // Expired already (verify() would have refused it, so a real snapshot never
        // carries this - but a renderer that used `saturating_sub` incorrectly could still
        // show a nonsense expiring line): make sure the ordinary "no key" wording is what
        // shows for a machine with no key on record and no certificate contribution at all.
        let expired_no_relay = snap_at(crate::license::Mode::Business, "", "", 0, None, now as u64);
        assert_eq!(
            licence_state_line(&expired_no_relay),
            t("licence_state_none"),
            "no key, no certificate contribution, must not claim to be verified"
        );
    }

    /// `format_unix_date` — the two edges a caller can actually hit: no timestamp on record
    /// (0, the serde default for a field that was never written) reads as empty rather than
    /// 1970-01-01, and a real timestamp comes back as a plain 4-digit year.
    #[test]
    fn format_unix_date_edges() {
        assert_eq!(format_unix_date(0), "");
        let d = format_unix_date(1_700_000_000); // 2023-11-14T22:13:20Z
        assert_eq!(d.len(), 10, "expected YYYY-MM-DD, got {d:?}");
        assert!(d.starts_with("202"), "expected a 2020s year, got {d:?}");
    }
}
