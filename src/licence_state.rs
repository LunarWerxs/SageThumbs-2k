//! The licence facts the SHELL honours, shared by the app (which writes them) and the
//! in-process handlers (which only read): the install-time mode, the breadcrumb, and the
//! pure phase decision - evaluation running, evaluation ended, or locked.
//!
//! Until 2026-09-13 all of this lived in `bin/app/license.rs` and the shell never looked at
//! it: a business copy with no key had every feature and heard reminders. Michael's
//! direction that day: a business install gets a **7-day evaluation**, then a **3-day
//! notice** that a key is required, and then it **stops** - thumbnails, previews, the
//! Details pane and the right-click menu refuse until a key is redeemed. The thumbnail
//! provider runs inside `explorer.exe`, so the decision has to be readable from there, off
//! the same two stores the app already keeps, with no app process alive. That is why the
//! mode read, the breadcrumb and the phase arithmetic moved here, into the core crate,
//! and why [`shell_locked`] is deliberately I/O-light.
//!
//! Three choices are deliberate enough to restate so nobody "fixes" them:
//!
//! * **The lock reaches ONLY a copy that never held a licence, or one whose licence the
//!   relay explicitly revoked.** A machine that once redeemed a key and has merely gone
//!   quiet (offline, relay down, certificate lapsed) is never locked by this module - it
//!   gets reminders, exactly as before. Network silence must never deny a paying customer
//!   (the 2026-09-04 rail audit's verdict, kept).
//! * **Personal copies never lock**, whatever the breadcrumb says. Free is first-class;
//!   the installer question is self-declaration, and someone who clicks Personal is
//!   accepted (Michael: "they can lie, I don't care").
//! * **Everything fails OPEN.** No breadcrumb, a corrupt one, an unreadable HKLM value, a
//!   clock before the trial started: all read as "not locked". The shell only ever refuses
//!   on a positive, well-formed, past-the-date reading.
//!
//! TRUST BOUNDARY, stated plainly: the breadcrumb is a users-writable file and the mode is
//! a world-readable value. A user who edits them defeats the lock, exactly as a user who
//! chooses Personal does. The lock is a path for the businesses that want to pay, not a
//! wall against the ones that will not.

use serde_json::{json, Value};
use std::sync::OnceLock;

/// The install-time declaration. Read-only at runtime; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Personal,
    Business,
}

/// Where the elevated installer records the wizard answer. The app's own settings live in
/// HKCU; this is deliberately HKLM so an unelevated process cannot flip it.
const MODE_KEY: &str = "Software\\SageThumbs2K";
pub const MODE_VALUE: &str = "LicenseMode";

/// The current mode. Installed builds only ever read the HKLM value the elevated installer
/// wrote. Portable builds have no installer and therefore no wizard declaration, so they
/// default to Personal too - UNLESS this copy has itself redeemed a business key, in which
/// case the app wrote the same marker string ("business") into the portable settings
/// store, and that is the ONE thing a portable copy consults instead. Both branches funnel
/// through the same [`parse_mode`], so "business" means the same thing whichever store it
/// came from.
///
/// `ST2K_LICENCE_MODE=business` forces Business for TEST ISOLATION (the in-process shell
/// tests cannot write HKLM). It is honoured in that direction ONLY: the override can make
/// this copy stricter, never looser, so it is not a way around the lock. Cached once, like
/// `settings::hkcu_root`, because this sits on the per-thumbnail hot path.
pub fn read_mode() -> Mode {
    static FORCED_BUSINESS: OnceLock<bool> = OnceLock::new();
    if *FORCED_BUSINESS.get_or_init(|| {
        std::env::var("ST2K_LICENCE_MODE").is_ok_and(|v| v.eq_ignore_ascii_case("business"))
    }) {
        return Mode::Business;
    }
    if crate::settings::portable() {
        return parse_mode(crate::settings::get_string_opt(MODE_VALUE).as_deref());
    }
    parse_mode(
        windows_registry::LOCAL_MACHINE
            .open(MODE_KEY)
            .and_then(|k| k.get_string(MODE_VALUE))
            .ok()
            .as_deref(),
    )
}

/// `None`, garbage, casing: everything but an exact business marker is Personal. Failing
/// toward the quiet mode is the module's standing rule (see the top docs).
pub fn parse_mode(raw: Option<&str>) -> Mode {
    match raw.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("business") => Mode::Business,
        _ => Mode::Personal,
    }
}

// ---------------------------------------------------------------------------------
// The breadcrumb.
// ---------------------------------------------------------------------------------

/// What this machine's licence history was, written by the app's licence check as it runs
/// and read back across uninstall/reinstall cycles - and, since 2026-09-13, read by the
/// shell handlers to decide whether to serve. NOTHING PERSONAL goes in here - no name, no
/// email, no serial (only its display prefix, which cannot redeem anything). That is a
/// contract with the seat rail's own schema, which draws the same line for the same reason.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    /// Ever ran under Business mode on this machine - a redeemed key, a positive relay
    /// answer, or (since the evaluation exists) simply an evaluation that started. Powers
    /// the one-time downgrade notice on a later reinstall-as-Personal.
    pub was_business: bool,
    /// The last state the entitlement check reported: "active" | "revoked".
    pub last_status: String,
    /// WHY, when `last_status` is "revoked": the relay's `reason` token (`seat_revoked`,
    /// `contract_ended`, ...), empty when it sent none or the status is not a revocation.
    /// Display only; never a decision input.
    pub last_reason: String,
    /// Unix seconds of the last POSITIVE entitlement answer. The grace window
    /// ([`entitlement_from_cache`]) is measured from this. `0` means "never licensed",
    /// which is what makes the evaluation clock below apply at all.
    pub last_positive_unix: u64,
    /// Display prefix of the redeemed key ("esk_A1B2..."), for the deauthorised notice to
    /// name. Never sufficient to redeem.
    pub key_prefix: String,
    /// The one-time downgrade notice was shown and acknowledged. Keeps "once" true across
    /// launches.
    pub downgrade_acknowledged: bool,
    /// Unix seconds of the last time the app made (or attempted) a network check, success
    /// or failure alike. Drives the app's 6-hour throttle.
    pub last_check_unix: u64,
    /// How many times the startup licensing notice has been shown; drives the app's nag
    /// escalation.
    pub nag_count: u64,
    /// Unix seconds the nag was last shown.
    pub nag_last_unix: u64,
    /// Unix seconds this machine's UPDATES WINDOW ends (the relay's `maintenanceEndsAt`).
    /// `0` means "no window on record". An UPDATES fact, never a LICENCE fact: no code path
    /// may read this to decide entitlement or the lock.
    pub maint_unix: u64,
    /// Unix seconds the business EVALUATION started on this machine - the first moment the
    /// app saw Business mode with no licence ever redeemed. `0` means "not started", which
    /// [`phase`] reads as [`Phase::Clear`]: the shell never locks a copy whose clock nobody
    /// started. Set once and never reset by the app; it survives uninstall with the rest of
    /// the file, so a reinstall does not restart the evaluation (an owner-accepted edge
    /// either way: Michael, 2026-09-13, on people reinstalling every seven days - "if a
    /// person's really gonna go through that much effort, I don't really give a shit").
    pub trial_started_unix: u64,
    /// Unix seconds the app first LEARNED this machine's licence was revoked. `0` when not
    /// revoked, or revoked before this field existed (the app stamps it on its next
    /// evaluation). The lock for a revoked copy is measured from this, so a machine always
    /// gets the full notice period from the moment it could have known.
    pub revoked_unix: u64,
}

// Serialization is hand-rolled over `serde_json::Value` rather than serde-derive: this
// workspace deliberately carries serde_json WITHOUT the serde derive macros, and a dozen
// fields do not justify adding a proc-macro dependency to every build.
impl History {
    pub fn to_json(&self) -> Value {
        json!({
            "was_business": self.was_business,
            "last_status": self.last_status,
            "last_reason": self.last_reason,
            "last_positive_unix": self.last_positive_unix,
            "key_prefix": self.key_prefix,
            "downgrade_acknowledged": self.downgrade_acknowledged,
            "last_check_unix": self.last_check_unix,
            "nag_count": self.nag_count,
            "nag_last_unix": self.nag_last_unix,
            "maint_unix": self.maint_unix,
            "trial_started_unix": self.trial_started_unix,
            "revoked_unix": self.revoked_unix,
        })
    }

    /// Missing fields take their defaults (an older file keeps working after a field is
    /// added); a PRESENT field of the WRONG TYPE fails the whole parse, because a file that
    /// half-parses is more misleading than one that does not.
    pub fn from_json(v: &Value) -> Option<Self> {
        let obj = v.as_object()?;
        fn field<T>(
            obj: &serde_json::Map<String, Value>,
            name: &str,
            take: impl Fn(&Value) -> Option<T>,
            default: T,
        ) -> Option<T> {
            match obj.get(name) {
                None => Some(default),
                Some(v) => take(v),
            }
        }
        let s = |v: &Value| v.as_str().map(String::from);
        Some(History {
            was_business: field(obj, "was_business", |v| v.as_bool(), false)?,
            last_status: field(obj, "last_status", s, String::new())?,
            last_reason: field(obj, "last_reason", s, String::new())?,
            last_positive_unix: field(obj, "last_positive_unix", |v| v.as_u64(), 0)?,
            key_prefix: field(obj, "key_prefix", s, String::new())?,
            downgrade_acknowledged: field(obj, "downgrade_acknowledged", |v| v.as_bool(), false)?,
            last_check_unix: field(obj, "last_check_unix", |v| v.as_u64(), 0)?,
            nag_count: field(obj, "nag_count", |v| v.as_u64(), 0)?,
            nag_last_unix: field(obj, "nag_last_unix", |v| v.as_u64(), 0)?,
            maint_unix: field(obj, "maint_unix", |v| v.as_u64(), 0)?,
            trial_started_unix: field(obj, "trial_started_unix", |v| v.as_u64(), 0)?,
            revoked_unix: field(obj, "revoked_unix", |v| v.as_u64(), 0)?,
        })
    }
}

/// Largest breadcrumb we will parse. The file is users-writable (see the trust boundary
/// note), so a multi-gigabyte prank must cost a bounded read, not a hang.
pub const HISTORY_MAX_BYTES: u64 = 64 * 1024;

/// `%ProgramData%\SageThumbs2K\license-history.json`. The installer pre-creates the
/// directory with user-modify ACLs; if it is missing anyway (portable, hand-deleted) the
/// write path creates it and inherits default ACLs, which merely narrows who can update the
/// breadcrumb, never breaks reading.
///
/// `ST2K_LICENCE_HOME=<dir>` redirects the whole file for TEST ISOLATION, the way
/// `ST2K_SETTINGS_ROOT` redirects the HKCU settings. Cached once per process: this is read
/// on every `GetThumbnail`, and an env var does not change under a running process.
pub fn history_path() -> Option<std::path::PathBuf> {
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let base = std::env::var_os("ST2K_LICENCE_HOME")
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("ProgramData")
                    .map(|p| std::path::Path::new(&p).join("SageThumbs2K"))
            })?;
        Some(base.join("license-history.json"))
    })
    .clone()
}

/// Read the breadcrumb, tolerating absence and hostility alike: no file, oversized file,
/// malformed JSON, wrong types - all `None`, never an error the caller must route. "No
/// history" is a legitimate answer and the common one.
///
/// Bounded through the SAME open handle the read uses, rather than a `metadata()` size
/// check followed by a separate `fs::read()`: the file lives in a users-modify directory,
/// so any account on the machine can rewrite it between those two calls, and `fs::read`
/// reads to EOF regardless of the size `metadata` reported.
pub fn read_history(path: &std::path::Path) -> Option<History> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(HISTORY_MAX_BYTES + 1)
        .read_to_end(&mut buf)
        .ok()?;
    if buf.len() as u64 > HISTORY_MAX_BYTES {
        return None;
    }
    let v: Value = serde_json::from_slice(&buf).ok()?;
    History::from_json(&v)
}

/// Best-effort write; returns whether it stuck. Atomic via [`crate::fsutil::write_atomically`]
/// (temp file beside `path`, then a retrying rename over it): a reader never sees a
/// half-written breadcrumb. Atomicity alone does NOT serialise two writers - that is the
/// app's `HistoryLock`, and this function assumes the caller holds it. The shell never
/// calls this: the handlers only read.
pub fn write_history(path: &std::path::Path, h: &History) -> bool {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(bytes) = serde_json::to_vec_pretty(&h.to_json()) else {
        return false;
    };
    crate::fsutil::write_atomically(path, &bytes).is_ok()
}

// ---------------------------------------------------------------------------------
// The pure decisions. Everything below is deterministic over its arguments so the tests
// can pin every boundary without a registry, a file, or a network in sight.
// ---------------------------------------------------------------------------------

/// How long a cached POSITIVE entitlement answer keeps a business install fully licensed
/// with no successful re-check: 7 days. The check is a network call and network calls
/// fail, so the licence must FAIL OPEN on a cached yes - bricking a paying customer because
/// their wifi dropped is strictly worse than a revoked seat running out the window.
pub const GRACE_SECS: u64 = 7 * 24 * 60 * 60;

/// How long a business copy that never redeemed a key may EVALUATE the product with
/// everything working: 7 days from the moment the app first saw it in Business mode
/// (Michael, 2026-09-13: "for seven days, it will let them use it, try it out, evaluate").
pub const TRIAL_SECS: u64 = 7 * 24 * 60 * 60;

/// The notice period between "the evaluation is over, a key is required" and the shell
/// actually refusing: 3 days. "A couple of days or whatever" was the brief; three covers
/// an evaluation that ends on a Friday evening, so the person who has to raise a purchase
/// order is back at their desk before anything stops. The same three days follow a
/// revocation, measured from when this machine learned of it.
pub const LOCK_GRACE_SECS: u64 = 3 * 24 * 60 * 60;

/// What the cached entitlement state means RIGHT NOW.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entitlement {
    /// A positive answer within the grace window: fully licensed, total silence.
    Licensed,
    /// The last positive answer has gone stale past [`GRACE_SECS`]: reminders.
    Lapsed,
    /// No positive answer on record at all.
    Unlicensed,
}

/// Grace-window arithmetic. `last_positive_unix == 0` means "no positive answer ever". A
/// clock that has gone BACKWARDS past the recorded answer reads as still-licensed rather
/// than lapsed: saturating math, because punishing a user for a BIOS battery is the
/// fail-closed direction this module refuses.
pub fn entitlement_from_cache(now_unix: u64, last_positive_unix: u64) -> Entitlement {
    if last_positive_unix == 0 {
        return Entitlement::Unlicensed;
    }
    if now_unix.saturating_sub(last_positive_unix) <= GRACE_SECS {
        Entitlement::Licensed
    } else {
        Entitlement::Lapsed
    }
}

/// Where a business copy stands on the evaluation-to-lock path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Nothing to enforce: a Personal copy, a licensed one, one whose licence merely went
    /// quiet, or an evaluation the app has not started yet.
    Clear,
    /// The evaluation is running and ends at `ends_unix`.
    Trial { ends_unix: u64 },
    /// The evaluation has ended (or the licence was revoked); everything still works until
    /// `locks_unix`, and the app says so loudly.
    Expiring { locks_unix: u64 },
    /// Past the lock date: the shell refuses until a key is redeemed.
    Locked,
}

/// The whole evaluation-and-lock decision, pure over its inputs. Exhaustive over [`Mode`]
/// so a future variant is a compile error here rather than a silent fall-through.
///
/// The order of the arms is the design: Personal never locks; a copy the relay revoked is
/// judged from `revoked_unix` (a cached positive still inside its grace window keeps it
/// clear, exactly as the app's posture does); a copy that ever held a licence and was not
/// revoked is clear whatever its cache says; and only a copy that NEVER held one runs the
/// evaluation clock.
pub fn phase(now_unix: u64, mode: Mode, history: Option<&History>) -> Phase {
    match mode {
        Mode::Personal => Phase::Clear,
        Mode::Business => business_phase(now_unix, history),
    }
}

/// Judges a business copy from its breadcrumb alone: the [`Mode::Business`] arm of
/// [`phase`], failing OPEN (to [`Phase::Clear`]) wherever the record is silent or absent.
fn business_phase(now_unix: u64, history: Option<&History>) -> Phase {
    let Some(h) = history else {
        return Phase::Clear;
    };
    if h.last_status == "revoked" {
        if entitlement_from_cache(now_unix, h.last_positive_unix) == Entitlement::Licensed {
            return Phase::Clear;
        }
        if h.revoked_unix == 0 {
            return Phase::Clear;
        }
        return after_deadline(now_unix, h.revoked_unix.saturating_add(GRACE_SECS));
    }
    if h.last_positive_unix > 0 {
        return Phase::Clear;
    }
    if h.trial_started_unix == 0 {
        return Phase::Clear;
    }
    let ends = h.trial_started_unix.saturating_add(TRIAL_SECS);
    if now_unix < ends {
        return Phase::Trial { ends_unix: ends };
    }
    after_deadline(now_unix, ends)
}

/// The shared tail of both lock paths: a deadline has passed, the notice period runs from
/// it, and the lock lands [`LOCK_GRACE_SECS`] later.
fn after_deadline(now_unix: u64, deadline_unix: u64) -> Phase {
    let locks = deadline_unix.saturating_add(LOCK_GRACE_SECS);
    if now_unix < locks {
        Phase::Expiring { locks_unix: locks }
    } else {
        Phase::Locked
    }
}

/// Whole days from `now` until `until`, rounded UP, never below 1 while `until` is still
/// ahead - "1 day left" is what a person expects to read four hours before the end, not
/// "0 days left". `0` once `until` has passed.
pub fn days_until(now_unix: u64, until_unix: u64) -> u64 {
    if until_unix <= now_unix {
        return 0;
    }
    (until_unix - now_unix).div_ceil(24 * 60 * 60).max(1)
}

/// Now, in Unix seconds. `SystemTime::now()` failing (a clock before 1970) reads as 0,
/// which every caller in this module treats as "no time has passed" - the safe direction.
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The phase THIS machine is in right now, from the real stores. The one I/O entry point
/// the shell handlers and the doctor share with the app.
pub fn current_phase() -> Phase {
    let mode = read_mode();
    if mode == Mode::Personal {
        // The 99% case, and it costs one HKLM value read: a Personal copy never opens the
        // breadcrumb from inside explorer.exe.
        return Phase::Clear;
    }
    let history = history_path().and_then(|p| read_history(&p));
    phase(now_unix(), mode, history.as_ref())
}

/// Should the shell handlers refuse right now? Called per `GetThumbnail` / `DoPreview` /
/// property-store `Initialize` / menu build, so it is exactly as cheap as
/// [`current_phase`]: an HKLM value read for a Personal copy, plus one small file read
/// for a Business one. No caching on purpose - redeeming a key must unlock the very next
/// thumbnail, not the next process.
pub fn shell_locked() -> bool {
    matches!(current_phase(), Phase::Locked)
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 60 * 60;
    const T0: u64 = 1_760_000_000;

    fn never_licensed(trial_started: u64) -> History {
        History {
            was_business: true,
            trial_started_unix: trial_started,
            ..Default::default()
        }
    }

    #[test]
    fn everything_that_is_not_exactly_business_reads_personal() {
        assert_eq!(parse_mode(None), Mode::Personal);
        assert_eq!(parse_mode(Some("")), Mode::Personal);
        assert_eq!(parse_mode(Some("personal")), Mode::Personal);
        assert_eq!(parse_mode(Some("corporate")), Mode::Personal);
        assert_eq!(parse_mode(Some("business")), Mode::Business);
        assert_eq!(parse_mode(Some("  Business  ")), Mode::Business);
    }

    /// The whole evaluation path, boundary by boundary: day 0 to day 7 is the trial, day 7
    /// to day 10 is the notice, day 10 onward is the lock.
    #[test]
    fn the_evaluation_runs_seven_days_then_three_of_notice_then_locks() {
        let h = never_licensed(T0);
        let ends = T0 + TRIAL_SECS;
        let locks = ends + LOCK_GRACE_SECS;
        assert_eq!(
            phase(T0, Mode::Business, Some(&h)),
            Phase::Trial { ends_unix: ends },
            "the moment it starts"
        );
        assert_eq!(
            phase(ends - 1, Mode::Business, Some(&h)),
            Phase::Trial { ends_unix: ends },
            "one second before the end is still the trial"
        );
        assert_eq!(
            phase(ends, Mode::Business, Some(&h)),
            Phase::Expiring { locks_unix: locks },
            "the end itself is the notice period"
        );
        assert_eq!(
            phase(locks - 1, Mode::Business, Some(&h)),
            Phase::Expiring { locks_unix: locks }
        );
        assert_eq!(phase(locks, Mode::Business, Some(&h)), Phase::Locked);
        assert_eq!(
            phase(locks + 365 * DAY, Mode::Business, Some(&h)),
            Phase::Locked,
            "and it stays locked"
        );
        assert_eq!(TRIAL_SECS, 7 * DAY);
        assert_eq!(LOCK_GRACE_SECS, 3 * DAY);
    }

    /// The fail-open cases, each of which must read Clear however far the clock has run.
    #[test]
    fn nothing_locks_without_a_started_evaluation_or_outside_business_mode() {
        let far = T0 + 400 * DAY;
        assert_eq!(
            phase(far, Mode::Business, None),
            Phase::Clear,
            "no breadcrumb at all"
        );
        assert_eq!(
            phase(far, Mode::Business, Some(&History::default())),
            Phase::Clear,
            "a breadcrumb with no evaluation started"
        );
        assert_eq!(
            phase(far, Mode::Personal, Some(&never_licensed(T0))),
            Phase::Clear,
            "a Personal copy never locks, whatever the file says"
        );
        assert_eq!(
            phase(T0 - DAY, Mode::Business, Some(&never_licensed(T0))),
            Phase::Trial {
                ends_unix: T0 + TRIAL_SECS
            },
            "a clock set back before the start is still inside the trial"
        );
    }

    /// A machine that ever held a licence and was not revoked is never locked, however stale
    /// its cache is - an offline paying customer gets reminders, never a wall.
    #[test]
    fn a_once_licensed_machine_is_never_locked_by_silence() {
        let h = History {
            was_business: true,
            last_status: "active".into(),
            last_positive_unix: T0,
            key_prefix: "esk_A1B2".into(),
            // Even with an evaluation clock long expired on the same file.
            trial_started_unix: T0 - 100 * DAY,
            ..Default::default()
        };
        for now in [T0, T0 + GRACE_SECS, T0 + GRACE_SECS + 1, T0 + 400 * DAY] {
            assert_eq!(
                phase(now, Mode::Business, Some(&h)),
                Phase::Clear,
                "at {now}"
            );
        }
    }

    /// A revoked machine: silent while a cached positive is inside its grace window, then
    /// the notice period from when the revocation was learned plus the grace, then locked.
    #[test]
    fn a_revoked_machine_gets_the_grace_then_the_notice_then_locks() {
        let learned = T0 + DAY;
        let h = History {
            was_business: true,
            last_status: "revoked".into(),
            last_positive_unix: T0,
            key_prefix: "esk_A1B2".into(),
            revoked_unix: learned,
            ..Default::default()
        };
        let locks = learned + GRACE_SECS + LOCK_GRACE_SECS;
        assert_eq!(
            phase(T0 + 2 * DAY, Mode::Business, Some(&h)),
            Phase::Clear,
            "the cached positive still inside its 7 days keeps it clear"
        );
        assert_eq!(
            phase(T0 + GRACE_SECS + 1, Mode::Business, Some(&h)),
            Phase::Expiring { locks_unix: locks },
            "past the grace it is on notice"
        );
        assert_eq!(
            phase(locks - 1, Mode::Business, Some(&h)),
            Phase::Expiring { locks_unix: locks }
        );
        assert_eq!(phase(locks, Mode::Business, Some(&h)), Phase::Locked);

        // The loud phase is never shorter than the notice period: the cache lapses at
        // last_positive + GRACE, and last_positive can never be later than revoked_unix
        // (a later positive clears the revocation), so lock - lapse >= LOCK_GRACE_SECS.
        assert!(locks - (T0 + GRACE_SECS) >= LOCK_GRACE_SECS);
    }

    /// A breadcrumb that recorded a revocation before `revoked_unix` existed must not lock
    /// on the spot; the app stamps the field on its next evaluation and the clock starts
    /// from there.
    #[test]
    fn a_revocation_with_no_learned_date_does_not_lock() {
        let h = History {
            last_status: "revoked".into(),
            last_positive_unix: T0,
            ..Default::default()
        };
        assert_eq!(
            phase(T0 + 400 * DAY, Mode::Business, Some(&h)),
            Phase::Clear
        );
    }

    #[test]
    fn days_until_rounds_up_and_never_says_zero_while_time_remains() {
        assert_eq!(days_until(T0, T0 + 7 * DAY), 7);
        assert_eq!(days_until(T0, T0 + 7 * DAY - 1), 7);
        assert_eq!(days_until(T0, T0 + 6 * DAY + 1), 7);
        assert_eq!(days_until(T0, T0 + 6 * DAY), 6);
        assert_eq!(days_until(T0, T0 + 1), 1, "an hour left is still a day");
        assert_eq!(days_until(T0, T0), 0);
        assert_eq!(days_until(T0 + 1, T0), 0);
    }

    #[test]
    fn the_breadcrumb_round_trips_and_tolerates_hostility() {
        let dir = std::env::temp_dir().join(format!("st2k_licstate_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("license-history.json");
        let h = History {
            was_business: true,
            last_status: "active".into(),
            last_reason: "contract_ended".into(),
            last_positive_unix: 1_760_000_000,
            key_prefix: "esk_A1B2".into(),
            downgrade_acknowledged: false,
            last_check_unix: 1_760_003_600,
            nag_count: 7,
            nag_last_unix: 1_759_900_000,
            maint_unix: 1_791_500_000,
            trial_started_unix: 1_759_000_000,
            revoked_unix: 1_759_500_000,
        };
        assert!(write_history(&p, &h), "write must stick");
        assert_eq!(
            read_history(&p).as_ref(),
            Some(&h),
            "read back what was written"
        );

        std::fs::write(&p, b"not json at all").unwrap();
        assert_eq!(read_history(&p), None, "garbage reads as no history");
        std::fs::write(&p, b"{}").unwrap();
        assert_eq!(
            read_history(&p),
            Some(History::default()),
            "empty object = all defaults, the two new fields included"
        );
        std::fs::write(&p, br#"{"trial_started_unix": "soon"}"#).unwrap();
        assert_eq!(read_history(&p), None, "wrong types read as no history");
        assert_eq!(read_history(&dir.join("absent.json")), None);

        std::fs::write(&p, vec![b' '; (HISTORY_MAX_BYTES + 1) as usize]).unwrap();
        assert_eq!(read_history(&p), None, "an oversized prank is refused");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_history_accepts_a_file_exactly_at_the_byte_cap_and_creates_its_directory() {
        let dir = std::env::temp_dir().join(format!("st2k_licstate_cap_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let p = dir.join("license-history.json");

        let mut at_cap = serde_json::to_vec(&History::default().to_json()).unwrap();
        assert!((at_cap.len() as u64) <= HISTORY_MAX_BYTES);
        at_cap.resize(HISTORY_MAX_BYTES as usize, b' '); // trailing whitespace, still valid JSON
        std::fs::write(&p, &at_cap).unwrap();
        assert_eq!(
            read_history(&p),
            Some(History::default()),
            "a file exactly at HISTORY_MAX_BYTES must still be read"
        );

        let deeper = dir.join("deeper").join("license-history.json");
        assert!(write_history(&deeper, &History::default()));
        assert!(read_history(&deeper).is_some());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The installer's "was this a business machine?" check has no JSON parser: it looks
    /// for the literal `"was_business": true` that serde_json's pretty printer writes. Pin
    /// that shape so a serializer change cannot silently blind the installer's confirmation.
    #[test]
    fn the_pretty_printed_breadcrumb_carries_the_literals_the_installer_greps_for() {
        let h = History {
            was_business: true,
            downgrade_acknowledged: true,
            ..Default::default()
        };
        let text = serde_json::to_string_pretty(&h.to_json()).unwrap();
        assert!(text.contains("\"was_business\": true"), "{text}");
        assert!(text.contains("\"downgrade_acknowledged\": true"), "{text}");
    }
}
