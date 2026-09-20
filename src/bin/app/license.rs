//! Licence MODE, the survives-uninstall history breadcrumb, the evaluation clock, and the
//! pure posture decisions (grace window, evaluation, lock, downgrade detection).
//!
//! The product design is Michael's, decided 2026-08-31 and extended 2026-09-13, and four of
//! its choices are deliberate enough to restate here so nobody "fixes" them:
//!
//! * **The installer asks Personal-or-Business, and the mode changes ONLY by
//!   reinstalling.** There is no Settings toggle on purpose ("not just a setting lazy
//!   users will just go flip the switch on"). The mode lives in HKLM, written by the
//!   elevated installer, and this process only ever READS it.
//! * **The installer question is self-declaration, not enforcement of honesty.** It
//!   exists to remove the "nobody told us" excuse a business otherwise has. Anyone who
//!   wants free clicks Personal, and that is accepted ("they can lie, I don't care").
//! * **A Business copy with no key is an EVALUATION, and an evaluation ends.** Seven days
//!   with everything working, then three days of loud notice that a key is required, then
//!   the shell stops serving (thumbnails, previews, Details pane, right-click menu) until
//!   a key is redeemed. The arithmetic and the shell's read of it live in
//!   [`sagethumbs2k_core::licence_state`], because the thumbnail provider runs inside
//!   `explorer.exe` and must decide with no app process alive. This module STARTS the
//!   clock ([`start_trial_if_due`]), speaks the phases ([`Posture`]) and never locks a
//!   copy that once held a licence and merely went quiet.
//! * **Everything here fails toward Personal/free.** A missing value, a corrupt
//!   breadcrumb, an unreadable key: all read as the quiet mode. The one thing this
//!   module must never do is nag or lock someone the design says should be left alone.
//!
//! The BREADCRUMB records that this machine ran under Business mode, so a later
//! reinstall-as-Personal can be met with a single factual notice (the "downgrade
//! detection"), and it carries the evaluation clock. It lives in ProgramData rather than
//! the registry because the licence check runs UNELEVATED at runtime and must be able to
//! update it, and it must SURVIVE UNINSTALL or the whole feature is void: reinstall is the
//! mode-change path, and a breadcrumb the uninstaller deletes would let a corporate
//! machine launder itself into a fresh home install. `installer.iss` creates the
//! directory with `uninsneveruninstall` and user-modify permissions; `check-consistency.ps1`
//! pins both so neither can be tidied away silently. The struct and its (de)serialization
//! are [`sagethumbs2k_core::licence_state::History`]; this module owns every WRITE.
//!
//! TRUST BOUNDARY, stated plainly: the breadcrumb is a users-writable file and the mode is
//! a world-readable value. Both are ADVISORY. A user who edits them defeats the reminders
//! and the lock, exactly as a user who clicks "Personal" does. The lock is a path for the
//! businesses that mean to pay, never a wall against the ones that will not; the seat
//! rail (Pay's entitlement read, via our relay) is what says who is actually licensed.

use serde_json::{json, Value};
mod relay;
use relay::*;
mod check;
#[cfg(test)]
use check::*;
mod history;
use history::*;
mod nag;
pub(crate) use check::{
    acknowledge_downgrade, parse_iso_unix, refresh_entitlement, refresh_entitlement_now,
};
#[cfg(test)]
use nag::*;
pub(crate) use nag::{background_tick, nag_due, note_nag_shown, snapshot, LicenceSnapshot};
#[cfg(test)]
pub(crate) use relay::{key_prefix, normalize_key};
pub(crate) use relay::{
    machine_fingerprint, redeem, renew_url, sha256, RedeemOutcome, BUY_URL, PORTAL_CLAIM_URL,
    RELAY_BASE,
};

pub(crate) use sagethumbs2k_core::licence_state::{
    days_until, entitlement_from_cache, history_path, now_unix, phase, read_history, read_mode,
    shell_locked, write_history, Entitlement, History, Mode, Phase, LOCK_GRACE_SECS,
};

/// The portable settings marker a redeemed key writes - the same string the installer
/// writes to HKLM, so [`read_mode`] reads either store through one parser.
const MODE_VALUE: &str = sagethumbs2k_core::licence_state::MODE_VALUE;

// ---------------------------------------------------------------------------------
// The pure decisions. Everything below is deterministic over its arguments so the
// tests can pin every boundary without a registry, a file, or a network in sight.
// ---------------------------------------------------------------------------------

/// Does a stored offline certificate license THIS machine right now, and if so, when does
/// it expire? (E05 audit: folded the old boolean `certificate_licenses_this_machine` into
/// this - the only caller needed the expiry too, and re-verifying the certificate a second
/// time just to get at a field the boolean threw away would be wasted work.)
///
/// Reads the breadcrumb's neighbour rather than the network: see [`crate::licence_cert`]
/// for the whole model. Every failure - no certificate, no fingerprint, a blob from
/// another machine, an expired one - answers `None`, which only ever means "the
/// certificate has nothing to add", never "unlicensed".
fn certificate_expiry_if_licensed(now_unix: u64) -> Option<i64> {
    let cert = crate::cred_store::load_licence_cert()?;
    let fingerprint = machine_fingerprint()?;
    certificate_expiry_from(&cert, &fingerprint, now_unix)
}

/// The stored certificate's `maint` claim - this machine's updates-window end according to
/// a signed statement rather than the relay (2026-09-10).
///
/// The OFFLINE FALLBACK, consulted only when the breadcrumb has no window on record (see
/// [`at`]): the relay answers sooner and can be re-read, so its number wins whenever there
/// is one. `None` for every failure the certificate path already answers `None` to - no
/// certificate, another machine, expired - plus a certificate that simply carries no
/// `maint`, which means "updates never lapse".
fn certificate_maint_unix(now_unix: u64) -> Option<u64> {
    let cert = crate::cred_store::load_licence_cert()?;
    let fingerprint = machine_fingerprint()?;
    certificate_maint_from(&cert, &fingerprint, now_unix)
}

/// The I/O-free core of [`certificate_maint_unix`], split out for the same reason
/// [`certificate_expiry_from`] is: so a test can drive it with the real fixture certificate.
fn certificate_maint_from(cert: &str, fingerprint: &str, now_unix: u64) -> Option<u64> {
    let now = i64::try_from(now_unix).unwrap_or(i64::MAX);
    let verified = crate::licence_cert::verify(cert, fingerprint, now).ok()?;
    u64::try_from(verified.maint_unix?).ok()
}

/// The I/O-free core of [`certificate_expiry_if_licensed`]: verify `cert` against
/// `fingerprint` at `now_unix` and report its expiry when it licenses this machine. Split
/// out (E05 follow-up audit, review item 4d) so a test can drive
/// [`entitlement_and_cert_expiry`]'s certificate path with a REAL certificate object - the
/// same fixture [`crate::licence_cert`]'s own tests pin the crypto against - rather than
/// only ever exercising the 30-day cert-expiry-warning window through a hand-built
/// `LicenceSnapshot` in `licence_state_line`'s tests, which never touches `licence_cert::verify`
/// at all.
fn certificate_expiry_from(cert: &str, fingerprint: &str, now_unix: u64) -> Option<i64> {
    let now = i64::try_from(now_unix).unwrap_or(i64::MAX);
    let verified = crate::licence_cert::verify(cert, fingerprint, now).ok()?;
    verified.licensed.then_some(verified.exp_unix)
}

/// How long before a certificate's `exp` the Licence page starts warning
/// ([`Posture`]/`Entitlement` are unaffected - a valid certificate keeps licensing the
/// machine right up to the moment it actually expires; this only controls when the UI
/// starts saying so). 30 days: long enough that a business relying on the certificate as
/// its floor (no relay reachable, or none configured) has a real window to re-redeem
/// before the machine would fall back to [`Entitlement::Unlicensed`], short enough that
/// the warning isn't visible for a meaningful fraction of a typical one-year maintenance
/// term.
pub(crate) const CERT_EXPIRY_WARNING_SECS: u64 = 30 * 24 * 60 * 60;

/// The entitlement this machine actually has: the relay breadcrumb, with a valid offline
/// certificate as a FLOOR under it.
///
/// This is the composition [`crate::licence_cert`] exists for. The relay answers sooner
/// and says more, so it is consulted first and its positive answer is taken as-is; the
/// certificate only speaks when the breadcrumb would otherwise degrade a machine that a
/// signed statement says is licensed. A relay that is unreachable, misconfigured, or
/// pointed at the wrong company therefore costs a customer nothing.
///
/// ⛔ A KNOWN REVOCATION OUTRANKS A CERTIFICATE, and that ordering is the reason this is
/// a function rather than a `max()`. A certificate cannot be withdrawn - its only
/// revocation reach is its own expiry - but the relay telling us `revoked` is strictly
/// better information than a statement signed before the revocation happened. Letting the
/// floor win there would keep a revoked seat running for the certificate's whole life and
/// silently disarm [`Posture::DeauthorizedLoud`].
fn entitlement_now(now_unix: u64, history: Option<&History>) -> Entitlement {
    entitlement_and_cert_expiry(now_unix, history).0
}

/// Same decision as [`entitlement_now`], additionally reporting the certificate's expiry
/// (E05 audit) when the certificate is the REASON this machine is licensed - never when a
/// healthier relay answer already grants it, and never when the relay has recorded a
/// revocation (a certificate has no revocation reach beyond its own expiry, so it must not
/// be allowed to look reassuring next to a status that says otherwise). `None` means no
/// certificate participated in the answer at all - whether because none is stored, none
/// matches this machine, or the relay's own verdict made checking one pointless.
fn entitlement_and_cert_expiry(
    now_unix: u64,
    history: Option<&History>,
) -> (Entitlement, Option<i64>) {
    let cached = entitlement_from_cache(now_unix, history.map_or(0, |h| h.last_positive_unix));
    let revoked = history.is_some_and(|h| h.last_status == "revoked");
    // Only reach for the certificate when it could actually change the answer: reading and
    // verifying one is cheap, but doing it on a machine already known to be licensed (or
    // known to be revoked) would be work whose result is discarded.
    if cached == Entitlement::Licensed || revoked {
        return (combine_entitlement(cached, revoked, false), None);
    }
    let cert_expiry = certificate_expiry_if_licensed(now_unix);
    (
        combine_entitlement(cached, revoked, cert_expiry.is_some()),
        cert_expiry,
    )
}

/// The ordering rule itself, with the I/O lifted out so it can be pinned by tests.
/// See [`entitlement_now`] for why a known revocation beats a valid certificate.
fn combine_entitlement(cached: Entitlement, revoked: bool, cert_licenses: bool) -> Entitlement {
    if revoked {
        return cached;
    }
    if cached == Entitlement::Licensed || cert_licenses {
        return Entitlement::Licensed;
    }
    cached
}

/// What the UI should do about licensing, decided once per launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Posture {
    /// Say nothing. Personal mode, and licensed business mode, both live here:
    /// free is first-class, and nobody who paid ever sees another licensing word.
    Silent,
    /// Business mode with no live licence and no evaluation clock to speak of: a copy that
    /// once held a key and has gone quiet (offline past the grace window, certificate
    /// lapsed), or one whose evaluation the app has not started yet. The persistent
    /// reminder, and nothing more - this posture never leads to the lock.
    BusinessNag,
    /// Business mode, never licensed, inside the 7-day evaluation that ends at
    /// `ends_unix`: everything works, a small countdown and a way to enter or buy a key.
    Trial { ends_unix: u64 },
    /// The evaluation is over and the 3-day notice is running: everything still works
    /// until `locks_unix`, and the app says so on every open.
    TrialExpired { locks_unix: u64 },
    /// This machine used to run under Business mode and was reinstalled as Personal: one
    /// factual notice, one acknowledgement, then silence.
    DowngradeNoticeOnce,
    /// The seat was revoked out from under a business install: loud and specific (name
    /// the key prefix, say how to re-license). `locks_unix` is when the shell stops, once
    /// the app has recorded when it learned of the revocation; `None` for a breadcrumb
    /// written before that field existed (the next evaluation stamps it).
    DeauthorizedLoud { locks_unix: Option<u64> },
    /// Past the lock date: the shell refuses everything until a key is redeemed. `revoked`
    /// tells the two stories apart ("your evaluation ended" / "your licence was revoked").
    Locked { revoked: bool },
}

impl Posture {
    /// Is this a posture the startup notices and the tray should speak about? Everything
    /// but the two silent-or-once cases.
    pub(crate) fn wants_reminder(self) -> bool {
        !matches!(self, Posture::Silent | Posture::DowngradeNoticeOnce)
    }

    /// Is this a posture where waiting a day between reminders would be wrong? Once the
    /// evaluation is over, or the shell has stopped, every open says so.
    pub(crate) fn is_urgent(self) -> bool {
        matches!(
            self,
            Posture::TrialExpired { .. }
                | Posture::Locked { .. }
                | Posture::DeauthorizedLoud {
                    locks_unix: Some(_)
                }
        )
    }
}

/// Compose the real inputs into today's posture, and log the decision so a support
/// thread can see which branch a machine took. This is the app's ONE entry point to
/// the module; the UI surfaces (the Business strip, the downgrade notice, the
/// deauthorised alert) hang off the returned value as they are built.
///
/// Starts the evaluation clock first ([`start_trial_if_due`]): the Settings window
/// opening is one of the moments a business copy is first seen, and the posture must
/// describe the clock it just started, not the "not started" state from a second ago.
pub(crate) fn current_posture() -> Posture {
    start_trial_if_due();
    let mode = read_mode();
    let history = history_path().and_then(|p| read_history(&p));
    let now = now_unix();
    let ent = entitlement_now(now, history.as_ref());
    let p = posture(now, mode, ent, history.as_ref());
    sagethumbs2k_core::safety::log_debugf!(
        "license: mode={mode:?} entitlement={ent:?} -> posture={p:?}"
    );
    p
}

/// Start the 7-day evaluation on a Business copy that has never redeemed a key and has no
/// clock yet, and repair a revocation recorded before `revoked_unix` existed. Idempotent
/// and cheap when there is nothing to do (a read, no write), so every entry point that
/// might be the first to see a business copy calls it: the installer's post-install
/// `--sync-user-shell`, the Settings window opening, the resident helper starting, the
/// daily one-shot check, and the background entitlement refresh. The shell never calls
/// this - it only reads - so a copy nobody has launched the app on stays `Clear`.
///
/// Also marks the machine `was_business`: an evaluation that was started and then
/// reinstalled as Personal is exactly the case the one-time downgrade notice exists for.
pub(crate) fn start_trial_if_due() {
    if read_mode() != Mode::Business {
        return;
    }
    let Some(path) = history_path() else {
        return;
    };
    let h = read_history(&path).unwrap_or_default();
    let needs_clock = h.last_positive_unix == 0 && h.trial_started_unix == 0;
    let needs_revoked_stamp = h.last_status == "revoked" && h.revoked_unix == 0;
    if !needs_clock && !needs_revoked_stamp {
        return;
    }
    let now = now_unix();
    update_history_at(&path, |h| {
        if h.last_positive_unix == 0 && h.trial_started_unix == 0 {
            h.trial_started_unix = now;
            h.was_business = true;
            sagethumbs2k_core::safety::log("license: business evaluation started");
        }
        if h.last_status == "revoked" && h.revoked_unix == 0 {
            h.revoked_unix = now;
        }
    });
}

/// The whole behaviour matrix in one place. Exhaustive over [`Mode`] so a future
/// variant is a compile error here rather than a silent fall-through.
///
/// The evaluation-and-lock phases come from the SAME function the shell reads
/// ([`phase`]), so what the Settings strip says and what `GetThumbnail` does can never
/// disagree about the date. A live entitlement outranks every phase: a copy inside its
/// grace window is `Silent` even if a stale evaluation clock sits in the file beside it.
pub(crate) fn posture(
    now_unix: u64,
    mode: Mode,
    ent: Entitlement,
    history: Option<&History>,
) -> Posture {
    match mode {
        Mode::Business => business_posture(now_unix, mode, ent, history),
        Mode::Personal => {
            // A key redeemed on this copy outranks the installer's answer while it is
            // within its grace window: the machine holds a live business licence and
            // hears nothing, whatever the wizard was told.
            if ent == Entitlement::Licensed {
                Posture::Silent
            } else if history.is_some_and(|h| h.was_business && !h.downgrade_acknowledged) {
                Posture::DowngradeNoticeOnce
            } else {
                Posture::Silent
            }
        }
    }
}

/// Business-mode posture: the phase-to-posture mapping, with the revoked breadcrumb from
/// `history` splitting the lapsed cases.
fn business_posture(
    now_unix: u64,
    mode: Mode,
    ent: Entitlement,
    history: Option<&History>,
) -> Posture {
    if ent == Entitlement::Licensed {
        return Posture::Silent;
    }
    // Lapsed-because-revoked and never-licensed look identical to the cache; the
    // breadcrumb's last recorded status is what tells a deauthorised machine
    // ("your licence was revoked, here is how to fix it") apart from one that
    // simply never entered a key ("this mode needs a licence").
    let revoked = history.is_some_and(|h| h.last_status == "revoked");
    match phase(now_unix, mode, history) {
        Phase::Locked => Posture::Locked { revoked },
        Phase::Expiring { locks_unix } if revoked => Posture::DeauthorizedLoud {
            locks_unix: Some(locks_unix),
        },
        Phase::Expiring { locks_unix } => Posture::TrialExpired { locks_unix },
        Phase::Trial { ends_unix } => Posture::Trial { ends_unix },
        Phase::Clear if revoked => Posture::DeauthorizedLoud { locks_unix: None },
        Phase::Clear => Posture::BusinessNag,
    }
}

// ---------------------------------------------------------------------------------
// The relay: redeeming a key and refreshing an entitlement. Both are BLOCKING network
// calls (WinINet, via `crate::http`) - every caller of `redeem`/`refresh_entitlement`
// runs them off the UI thread. Both fail toward "don't change anything and act as if
// nothing happened" on any transport or shape surprise, same standing rule as the
// rest of the module: a flaky network must never look like a rejected key or a
// revoked seat.
// ---------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
