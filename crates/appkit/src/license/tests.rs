#![cfg(test)]

use super::*;
mod history;
mod responses;
use st2k_base::licence_state::{parse_mode, GRACE_SECS, TRIAL_SECS};

/// [`redeem_outcome_from_response`] with a fixed canonical key, so the mapping tests
/// read as status + body only.
fn map_redeem(status: u16, body: &[u8]) -> RedeemOutcome {
    redeem_outcome_from_response(status, body, "esk_A1B2C-3D4E5-F6G7H-8I9J0")
}

/// PID-suffixed temp dir, the repo-wide convention so concurrent `cargo test`
/// runs (mutants baselines, parallel sessions) cannot race on one path.
fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("st2k_license_{tag}_{}", std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

/// E05 audit: `entitlement_and_cert_expiry` must short-circuit BEFORE ever consulting a
/// certificate whenever the relay already answers `Licensed` - the certificate is a
/// floor under an unreachable/lapsed relay, never a second vote once the relay has
/// spoken. Provable without touching real cert I/O because this is exactly the branch
/// that returns before `certificate_expiry_if_licensed` is ever called: against the
/// PRE-E05 code (which only ever returned a bare `Entitlement`) this test doesn't even
/// compile, since `cert_expires_unix` didn't exist to be `None`.
#[test]
fn a_relay_licensed_machine_never_reports_a_certificate_expiry() {
    let now = 1_760_000_000u64;
    // A history the relay just confirmed - `entitlement_from_cache` reads this as
    // `Licensed`, which is the branch that must skip the certificate entirely.
    let history = History {
        last_positive_unix: now,
        last_status: "active".into(),
        ..Default::default()
    };
    let (ent, cert_expires) = entitlement_and_cert_expiry(now, Some(&history));
    assert_eq!(ent, Entitlement::Licensed);
    assert_eq!(
        cert_expires, None,
        "a relay verification must never surface a certificate expiry line"
    );
}

/// A KNOWN REVOCATION must blank the certificate expiry too, for the same reason
/// `combine_entitlement` already refuses to let a certificate override a revocation
/// (see the tests below): a certificate cannot be withdrawn, so showing "expires in 20
/// days" next to a machine the relay just revoked would read as reassurance the relay
/// explicitly contradicted.
#[test]
fn a_revoked_machine_never_reports_a_certificate_expiry_either() {
    let now = 1_760_000_000u64;
    let history = History {
        last_status: "revoked".into(),
        last_positive_unix: 0,
        ..Default::default()
    };
    let (ent, cert_expires) = entitlement_and_cert_expiry(now, Some(&history));
    assert_eq!(ent, Entitlement::Unlicensed);
    assert_eq!(cert_expires, None);
}

/// E05 follow-up audit, review item 4d: pins the 30-day cert-expiry-warning window's
/// SOURCE data through a REAL certificate object - the exact fixture
/// `licence_cert`'s own tests pin the crypto against (`exp` 1791130974, i.e.
/// 2026-10-04) - rather than only ever exercising it through a hand-built
/// `LicenceSnapshot` in `licence_state_line`'s tests, which never calls
/// `licence_cert::verify` at all. Both instants sit before the certificate's `exp` (so
/// it verifies either way); only their distance to `exp` differs.
#[test]
fn a_real_certificate_reports_its_own_expiry_both_inside_and_outside_the_warning_window() {
    use crate::licence_cert::tests::{REAL_CERT, REAL_SUB};
    let exp = 1_791_130_974i64;

    // Well outside the 30-day window (40 days before `exp`).
    let far = (exp - 40 * 24 * 60 * 60) as u64;
    assert_eq!(certificate_expiry_from(REAL_CERT, REAL_SUB, far), Some(exp));
    let remaining_far = exp - i64::try_from(far).unwrap();
    assert!(
        remaining_far as u64 > CERT_EXPIRY_WARNING_SECS,
        "the far instant must genuinely be outside the warning window"
    );

    // Inside the 30-day window (5 days before `exp`).
    let near = (exp - 5 * 24 * 60 * 60) as u64;
    assert_eq!(
        certificate_expiry_from(REAL_CERT, REAL_SUB, near),
        Some(exp)
    );
    let remaining_near = exp - i64::try_from(near).unwrap();
    assert!(
        remaining_near as u64 <= CERT_EXPIRY_WARNING_SECS,
        "the near instant must genuinely be inside the warning window"
    );

    // A machine with a different fingerprint gets nothing from this certificate at all.
    assert_eq!(
        certificate_expiry_from(REAL_CERT, "some-other-machine", near),
        None
    );
}

/// A failing entitlement check (5xx, or a body the relay contract doesn't recognise)
/// must never advance `last_positive_unix`. E05 follow-up audit (review item 4b): this
/// now drives [`apply_check_response`] itself, the extracted response-handling half of
/// `refresh_entitlement_inner`, with a hand-written failure response, instead of
/// reimplementing its if-let inline. Against the pre-extraction shape there was no such
/// function to call, that logic lived only inline inside `refresh_entitlement_inner`,
/// reachable solely through a real network call, so this test could only ever prove its
/// own copy of the decision correct, never the production code's.
#[test]
fn a_failed_check_does_not_advance_last_verified() {
    let dir = temp_dir("failed_check");
    let path = dir.join("history.json");
    let earlier = 1_700_000_000u64;
    write_history(
        &path,
        &History {
            last_positive_unix: earlier,
            last_status: "active".into(),
            ..Default::default()
        },
    );

    let now = earlier + 1_000;
    // Mirrors the unconditional throttle bump `refresh_entitlement_inner` does before
    // ever making the network call.
    update_history_at(&path, |h| h.last_check_unix = now);

    // A 500 (or any non-200/unparsable body) is a failure, not a "not entitled" answer -
    // drive the REAL production function with a hand-written failure response.
    let result = apply_check_response(&path, now, 500, b"{}");
    assert!(
        result.is_none(),
        "a failing check must not report an entitlement"
    );
    assert!(apply_check_response(&path, now, 200, b"not json").is_none());

    let after = read_history(&path).expect("history file must still parse");
    assert_eq!(
        after.last_positive_unix, earlier,
        "a failed check must not advance last verified"
    );
    assert_eq!(
        after.last_status, "active",
        "a failed check must not touch the last-known status either"
    );
    assert_eq!(
        after.last_check_unix, now,
        "the throttle still records the attempt"
    );
}

#[test]
fn a_revocation_the_relay_reported_is_not_a_live_licence() {
    assert!(is_live_licence(Entitlement::Licensed, "active"));
    assert!(!is_live_licence(Entitlement::Licensed, "revoked"));
    assert!(!is_live_licence(Entitlement::Lapsed, "active"));
    assert!(!is_live_licence(Entitlement::Unlicensed, ""));
}

/// The whole matrix, by name: the four original postures plus the evaluation path and
/// the two lock stories, every one pinned at its boundary.
#[test]
fn the_posture_matrix_matches_the_design() {
    const DAY: u64 = 24 * 60 * 60;
    let t = 1_760_000_000u64;
    let revoked_no_date = History {
        last_status: "revoked".into(),
        was_business: true,
        ..Default::default()
    };
    let revoked = History {
        last_status: "revoked".into(),
        was_business: true,
        last_positive_unix: t - 10 * DAY,
        revoked_unix: t - 9 * DAY,
        key_prefix: "esk_A1B2".into(),
        ..Default::default()
    };
    let was_biz = History {
        was_business: true,
        ..Default::default()
    };
    let acked = History {
        was_business: true,
        downgrade_acknowledged: true,
        ..Default::default()
    };
    let trial = History {
        was_business: true,
        trial_started_unix: t,
        ..Default::default()
    };
    let once = History {
        was_business: true,
        last_status: "active".into(),
        last_positive_unix: t - 30 * DAY,
        key_prefix: "esk_A1B2".into(),
        trial_started_unix: t - 60 * DAY,
        ..Default::default()
    };

    // A licensed business copy is silent, whatever the file says.
    assert_eq!(
        posture(t, Mode::Business, Entitlement::Licensed, None),
        Posture::Silent
    );
    assert_eq!(
        posture(t, Mode::Business, Entitlement::Licensed, Some(&revoked)),
        Posture::Silent,
        "a live licence outranks stale history"
    );
    // Business with no key and no clock: the plain reminder, and NEVER the lock.
    assert_eq!(
        posture(t, Mode::Business, Entitlement::Unlicensed, None),
        Posture::BusinessNag
    );
    assert_eq!(
        posture(t, Mode::Business, Entitlement::Lapsed, Some(&once)),
        Posture::BusinessNag,
        "once licensed and gone quiet: reminders only"
    );
    assert_eq!(
        posture(
            t + 400 * DAY,
            Mode::Business,
            Entitlement::Lapsed,
            Some(&once)
        ),
        Posture::BusinessNag,
        "however long the silence, and whatever the old evaluation clock says"
    );
    // The evaluation: 7 days, 3 days of notice, then the lock.
    let ends = t + TRIAL_SECS;
    let locks = ends + LOCK_GRACE_SECS;
    assert_eq!(
        posture(
            t + DAY,
            Mode::Business,
            Entitlement::Unlicensed,
            Some(&trial)
        ),
        Posture::Trial { ends_unix: ends }
    );
    assert_eq!(
        posture(ends, Mode::Business, Entitlement::Unlicensed, Some(&trial)),
        Posture::TrialExpired { locks_unix: locks }
    );
    assert_eq!(
        posture(locks, Mode::Business, Entitlement::Unlicensed, Some(&trial)),
        Posture::Locked { revoked: false }
    );
    // Revoked: loud with no date until the app has stamped when it learned; loud with
    // the lock date once it has; locked past it.
    assert_eq!(
        posture(
            t,
            Mode::Business,
            Entitlement::Lapsed,
            Some(&revoked_no_date)
        ),
        Posture::DeauthorizedLoud { locks_unix: None }
    );
    assert_eq!(
        posture(
            t,
            Mode::Business,
            Entitlement::Unlicensed,
            Some(&revoked_no_date)
        ),
        Posture::DeauthorizedLoud { locks_unix: None }
    );
    let rlocks = revoked.revoked_unix + GRACE_SECS + LOCK_GRACE_SECS;
    assert_eq!(
        posture(t, Mode::Business, Entitlement::Lapsed, Some(&revoked)),
        Posture::DeauthorizedLoud {
            locks_unix: Some(rlocks)
        }
    );
    assert_eq!(
        posture(rlocks, Mode::Business, Entitlement::Lapsed, Some(&revoked)),
        Posture::Locked { revoked: true }
    );
    // Personal: silent, or the one-time downgrade notice; never the lock.
    assert_eq!(
        posture(t, Mode::Personal, Entitlement::Unlicensed, None),
        Posture::Silent
    );
    assert_eq!(
        posture(t, Mode::Personal, Entitlement::Unlicensed, Some(&was_biz)),
        Posture::DowngradeNoticeOnce
    );
    assert_eq!(
        posture(t, Mode::Personal, Entitlement::Unlicensed, Some(&acked)),
        Posture::Silent,
        "acknowledged means never again"
    );
    assert_eq!(
        posture(t, Mode::Personal, Entitlement::Licensed, None),
        Posture::Silent
    );
    let expired_personal = History {
        was_business: true,
        downgrade_acknowledged: true,
        trial_started_unix: t - 100 * DAY,
        ..Default::default()
    };
    assert_eq!(
        posture(
            t,
            Mode::Personal,
            Entitlement::Unlicensed,
            Some(&expired_personal)
        ),
        Posture::Silent,
        "a Personal copy with a long-expired evaluation clock is just a Personal copy"
    );
}

#[test]
fn reminders_and_urgency_follow_the_posture() {
    assert!(!Posture::Silent.wants_reminder());
    assert!(!Posture::DowngradeNoticeOnce.wants_reminder());
    assert!(Posture::BusinessNag.wants_reminder());
    assert!(Posture::Trial { ends_unix: 1 }.wants_reminder());
    assert!(Posture::Locked { revoked: true }.wants_reminder());

    assert!(!Posture::BusinessNag.is_urgent());
    assert!(
        !Posture::Trial { ends_unix: 1 }.is_urgent(),
        "the evaluation is spaced a day apart while everything works"
    );
    assert!(Posture::TrialExpired { locks_unix: 1 }.is_urgent());
    assert!(Posture::Locked { revoked: false }.is_urgent());
    assert!(Posture::DeauthorizedLoud {
        locks_unix: Some(1)
    }
    .is_urgent());
    assert!(
        !Posture::DeauthorizedLoud { locks_unix: None }.is_urgent(),
        "no lock date yet: the ordinary loud notice"
    );
}

// ---- redeem / check JSON-to-outcome mapping -----------------------------

// ---- nag escalation -------------------------------------------------------

#[test]
fn nag_due_at_the_twenty_four_hour_boundary() {
    let t = 1_760_000_000u64;
    let day = NAG_INTERVAL_SECS;
    assert!(
        nag_due_decision(t, 0, 0),
        "never shown before is due immediately"
    );
    assert!(!nag_due_decision(t, 5, t), "just shown is not due yet");
    assert!(nag_due_decision(t + day, 5, t), "exactly 24h later is due");
    assert!(
        !nag_due_decision(t + day - 1, 5, t),
        "one second short of 24h is not due yet"
    );
}

#[test]
fn nag_due_escalates_to_every_launch_at_thirty() {
    let t = 1_760_000_000u64;
    assert!(
        !nag_due_decision(t, 29, t),
        "29 shown, just shown: still waits a day"
    );
    assert!(
        nag_due_decision(t, 30, t),
        "30 shown: due on every launch regardless of timing"
    );
    assert!(
        nag_due_decision(t, 100, t),
        "well past 30: still every launch"
    );
}

// ---- refresh throttle -------------------------------------------------------

#[test]
fn a_live_key_on_a_personal_install_is_silent_not_a_downgrade() {
    let was_biz = History {
        was_business: true,
        downgrade_acknowledged: false,
        ..History::default()
    };
    assert_eq!(
        posture(
            1_760_000_000,
            Mode::Personal,
            Entitlement::Licensed,
            Some(&was_biz)
        ),
        Posture::Silent,
        "a redeemed key within grace outranks the installer's Personal answer"
    );
    assert_eq!(
        posture(
            1_760_000_000,
            Mode::Personal,
            Entitlement::Lapsed,
            Some(&was_biz)
        ),
        Posture::DowngradeNoticeOnce,
        "once the key lapses the one-time notice applies again"
    );
}

#[test]
fn a_rate_limited_redeem_is_named_as_such_not_as_a_bad_key() {
    assert_eq!(
        map_redeem(429, br#"{"ok":false,"error":"rate_limited"}"#),
        RedeemOutcome::Rejected {
            message: "Too many attempts. Try again in a few minutes.".to_string()
        }
    );
    assert_eq!(
        map_redeem(
            429,
            br#"{"ok":false,"error":"rate_limited","message":"Slow down."}"#
        ),
        RedeemOutcome::Rejected {
            message: "Slow down.".to_string()
        }
    );
}

#[test]
fn refresh_due_at_the_six_hour_boundary() {
    let t = 1_760_000_000u64;
    assert!(refresh_due(t, 0), "never checked before is due immediately");
    assert!(!refresh_due(t, t), "just checked is not due yet");
    assert!(
        refresh_due(t + REFRESH_THROTTLE_SECS, t),
        "exactly 6h later is due"
    );
    assert!(
        !refresh_due(t + REFRESH_THROTTLE_SECS - 1, t),
        "one second short of 6h is not due yet"
    );
}

// ---- machine fingerprint (pure half) ---------------------------------------

// ---- portable read_mode override --------------------------------------------

#[test]
fn portable_read_mode_override_uses_the_same_parser_as_hklm() {
    // `read_mode`'s portable branch stores/reads the mode through
    // `settings::set_string` / `get_string_opt` (see `redeem`), which depends on
    // where the exe sits on disk and so can't be flipped from a unit test. What
    // CAN be pinned here is the decision `read_mode` hands that stored value to:
    // the same `parse_mode` the HKLM branch uses, fed the exact string `redeem`
    // writes on success.
    assert_eq!(
        parse_mode(Some("business")),
        Mode::Business,
        "what redeem() writes into the portable store on success"
    );
    assert_eq!(
        parse_mode(None),
        Mode::Personal,
        "a portable ini with no LicenseMode value yet"
    );
}

// ---- the offline certificate, as a floor under the relay ----------------------

#[test]
fn a_certificate_lifts_a_lapsed_machine_back_to_licensed() {
    // The whole point: the relay went quiet past the grace window, and a signed
    // statement says this machine is licensed. It is.
    assert_eq!(
        combine_entitlement(Entitlement::Lapsed, false, true),
        Entitlement::Licensed
    );
    assert_eq!(
        combine_entitlement(Entitlement::Unlicensed, false, true),
        Entitlement::Licensed
    );
}

#[test]
fn without_a_certificate_nothing_changes_from_before() {
    for cached in [
        Entitlement::Licensed,
        Entitlement::Lapsed,
        Entitlement::Unlicensed,
    ] {
        assert_eq!(
            combine_entitlement(cached, false, false),
            cached,
            "the floor must be purely additive"
        );
    }
}

#[test]
fn a_known_revocation_outranks_a_valid_certificate() {
    // A certificate cannot be withdrawn, but the relay saying `revoked` is strictly
    // newer information than a statement signed before the revocation happened.
    // Letting the floor win here would keep a revoked seat running for the whole life
    // of the certificate and silently disarm the deauthorised notice.
    assert_eq!(
        combine_entitlement(Entitlement::Lapsed, true, true),
        Entitlement::Lapsed
    );
    assert_eq!(
        combine_entitlement(Entitlement::Unlicensed, true, true),
        Entitlement::Unlicensed
    );
}

#[test]
fn a_revoked_seat_still_reaches_the_deauthorised_notice_with_a_certificate_present() {
    // The end-to-end shape of the rule above, through the real posture decision.
    let revoked = History {
        was_business: true,
        last_status: "revoked".to_string(),
        ..History::default()
    };
    assert_eq!(
        posture(
            1_760_000_000,
            Mode::Business,
            combine_entitlement(Entitlement::Unlicensed, true, true),
            Some(&revoked)
        ),
        Posture::DeauthorizedLoud { locks_unix: None }
    );
}

// ---- the machine-wide history lock (2026-09-05 audit, F18) -----------------

/// [`PORTAL_CLAIM_URL`]'s own doc comment says why: unlike a portal TOKEN, a licence key
/// is the long-lived credential and the Connections claim page's contract is that it is
/// typed into a form field and POSTed, never carried in a URL - so this constant must
/// stay a bare page with no `?key=...` (or any other query string) appended, ever. This
/// pins that shape so a future edit that reaches for `renew_url()`'s "pre-fill the key"
/// pattern here (wrong for this URL) fails a test instead of silently leaking a key into
/// browser history / a proxy log.
#[test]
fn portal_claim_url_never_carries_a_query_string() {
    assert_eq!(PORTAL_CLAIM_URL, "https://st2k.lunarwerx.com/claim");
    assert!(
        !PORTAL_CLAIM_URL.contains('?'),
        "the licence key must never ride in a URL - see PORTAL_CLAIM_URL's doc comment"
    );
}
