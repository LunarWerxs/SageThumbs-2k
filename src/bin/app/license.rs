//! Licence MODE, the survives-uninstall history breadcrumb, and the pure posture
//! decisions (grace window, downgrade detection).
//!
//! The product design is Michael's, decided 2026-08-31, and three of its choices are
//! deliberate enough to restate here so nobody "fixes" them:
//!
//! * **The installer asks Personal-or-Business, and the mode changes ONLY by
//!   reinstalling.** There is no Settings toggle on purpose ("not just a setting lazy
//!   users will just go flip the switch on"). The mode lives in HKLM, written by the
//!   elevated installer, and this process only ever READS it.
//! * **The installer question is self-declaration, not enforcement.** It exists to
//!   remove the "nobody told us" excuse a business otherwise has. Anyone who wants
//!   free clicks Personal, and that is accepted.
//! * **Everything here fails toward Personal/free.** A missing value, a corrupt
//!   breadcrumb, an unreadable key: all read as the quiet mode. The one thing this
//!   module must never do is nag someone the design says should be left alone.
//!
//! The BREADCRUMB records that this machine once ran under a business licence, so a
//! later reinstall-as-Personal can be met with a single factual notice (the
//! "downgrade detection"). It lives in ProgramData rather than the registry because
//! the licence check runs UNELEVATED at runtime and must be able to update it, and it
//! must SURVIVE UNINSTALL or the whole feature is void: reinstall is the mode-change
//! path, and a breadcrumb the uninstaller deletes would let a corporate machine
//! launder itself into a fresh home install. `installer.iss` creates the directory
//! with `uninsneveruninstall` and user-modify permissions; `check-consistency.ps1`
//! pins both so neither can be tidied away silently.
//!
//! TRUST BOUNDARY, stated plainly: the breadcrumb is a users-writable file and the
//! mode is a world-readable value. Both are ADVISORY. A user who edits them defeats
//! only the reminders, exactly as a user who clicks "Personal" does. Licence
//! ENFORCEMENT is the seat rail's job (Pay's entitlement read, via our relay), never
//! this file's, so nothing here treats either store as trustworthy input: the JSON
//! parse is bounds-checked and any malformation reads as "no history".

use serde_json::{json, Value};

/// The install-time declaration. Read-only at runtime; see the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    Personal,
    Business,
}

/// Where the elevated installer records the wizard answer. The app's own settings
/// live in HKCU; this is deliberately HKLM so an unelevated process cannot flip it.
const MODE_KEY: &str = "Software\\SageThumbs2K";
const MODE_VALUE: &str = "LicenseMode";

/// The current mode. Portable builds have no installer and therefore no declaration,
/// so they read Personal; a portable user who later redeems a serial has
/// self-declared through the stronger channel and the licence state carries that.
pub(crate) fn read_mode() -> Mode {
    if sagethumbs2k_core::settings::portable() {
        return Mode::Personal;
    }
    parse_mode(
        windows_registry::LOCAL_MACHINE
            .open(MODE_KEY)
            .and_then(|k| k.get_string(MODE_VALUE))
            .ok()
            .as_deref(),
    )
}

/// `None`, garbage, casing: everything but an exact business marker is Personal.
/// Failing toward the quiet mode is the module's standing rule (see the top docs).
fn parse_mode(raw: Option<&str>) -> Mode {
    match raw.map(str::trim) {
        Some(s) if s.eq_ignore_ascii_case("business") => Mode::Business,
        _ => Mode::Personal,
    }
}

// ---------------------------------------------------------------------------------
// The breadcrumb.
// ---------------------------------------------------------------------------------

/// What this machine's licence history was, written by the licence check as it runs
/// and read back across uninstall/reinstall cycles. NOTHING PERSONAL goes in here -
/// no name, no email, no serial (only its display prefix, which cannot redeem
/// anything). That is a contract with the seat rail's own schema, which draws the
/// same line for the same reason.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct History {
    /// Ever held an active business licence on this machine.
    pub was_business: bool,
    /// The last state the entitlement check reported: "active" | "revoked".
    pub last_status: String,
    /// Unix seconds of the last POSITIVE entitlement answer. The grace window
    /// (`entitlement_from_cache`) is measured from this.
    pub last_positive_unix: u64,
    /// Display prefix of the redeemed key ("esk_A1B2..."), for the deauthorised
    /// notice to name. Never sufficient to redeem.
    pub key_prefix: String,
    /// The one-time downgrade notice was shown and acknowledged. Keeps "once" true
    /// across launches.
    pub downgrade_acknowledged: bool,
}

// Serialization is hand-rolled over `serde_json::Value` rather than serde-derive:
// this workspace deliberately carries serde_json WITHOUT the serde derive macros
// (see `nudge_engine`'s header note - the same trade was made there), and five
// fields do not justify adding a proc-macro dependency to every build.
impl History {
    // `expect`, not `allow`: the serial-entry and heartbeat wiring is the real consumer
    // of the write path, and when it lands this attribute becomes a hard error - the
    // reminder deletes itself. Until then the tests are the only writers.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "consumed when the licence-check wiring lands; test-only today"
        )
    )]
    fn to_json(&self) -> Value {
        json!({
            "was_business": self.was_business,
            "last_status": self.last_status,
            "last_positive_unix": self.last_positive_unix,
            "key_prefix": self.key_prefix,
            "downgrade_acknowledged": self.downgrade_acknowledged,
        })
    }

    /// Missing fields take their defaults (an older file keeps working after a
    /// field is added); a PRESENT field of the WRONG TYPE fails the whole parse,
    /// because a file that half-parses is more misleading than one that does not.
    fn from_json(v: &Value) -> Option<Self> {
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
        Some(History {
            was_business: field(obj, "was_business", |v| v.as_bool(), false)?,
            last_status: field(
                obj,
                "last_status",
                |v| v.as_str().map(String::from),
                String::new(),
            )?,
            last_positive_unix: field(obj, "last_positive_unix", |v| v.as_u64(), 0)?,
            key_prefix: field(
                obj,
                "key_prefix",
                |v| v.as_str().map(String::from),
                String::new(),
            )?,
            downgrade_acknowledged: field(obj, "downgrade_acknowledged", |v| v.as_bool(), false)?,
        })
    }
}

/// Largest breadcrumb we will parse. The file is users-writable (see the trust
/// boundary note), so a multi-gigabyte prank must cost a bounded read, not a hang.
const HISTORY_MAX_BYTES: u64 = 64 * 1024;

/// `%ProgramData%\SageThumbs2K\license-history.json`. The installer pre-creates the
/// directory with user-modify ACLs; if it is missing anyway (portable, hand-deleted)
/// the write path creates it and inherits default ACLs, which merely narrows who can
/// update the breadcrumb, never breaks reading.
pub(crate) fn history_path() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("ProgramData")?;
    Some(
        std::path::Path::new(&base)
            .join("SageThumbs2K")
            .join("license-history.json"),
    )
}

/// Read the breadcrumb, tolerating absence and hostility alike: no file, oversized
/// file, malformed JSON, wrong types - all `None`, never an error the caller must
/// route. "No history" is a legitimate answer and the common one.
pub(crate) fn read_history(path: &std::path::Path) -> Option<History> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > HISTORY_MAX_BYTES {
        return None;
    }
    let v: Value = serde_json::from_slice(&std::fs::read(path).ok()?).ok()?;
    History::from_json(&v)
}

/// Best-effort write; returns whether it stuck. A failed write degrades the
/// downgrade-detection feature, not the app, so callers log and move on.
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "consumed when the licence-check wiring lands; test-only today"
    )
)]
pub(crate) fn write_history(path: &std::path::Path, h: &History) -> bool {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    serde_json::to_vec_pretty(&h.to_json())
        .ok()
        .and_then(|bytes| std::fs::write(path, bytes).ok())
        .is_some()
}

// ---------------------------------------------------------------------------------
// The pure decisions. Everything below is deterministic over its arguments so the
// tests can pin every boundary without a registry, a file, or a network in sight.
// ---------------------------------------------------------------------------------

/// How long a cached POSITIVE entitlement answer keeps a business install fully
/// licensed with no successful re-check: 7 days.
///
/// The number is a deliberate product trade (delegated to this module 2026-09-01,
/// after the design review): the check is a network call and network calls fail, so
/// the licence must FAIL OPEN on a cached yes - bricking a paying customer because
/// their wifi dropped is strictly worse than a revoked seat running out the window.
/// The accepted cost, stated rather than hidden: a deauthorised machine keeps
/// working for up to a week.
pub(crate) const GRACE_SECS: u64 = 7 * 24 * 60 * 60;

/// What the cached entitlement state means RIGHT NOW.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Entitlement {
    /// A positive answer within the grace window: fully licensed, total silence.
    Licensed,
    /// The last positive answer has gone stale past [`GRACE_SECS`]: degrade to the
    /// free feature set and start asking for attention.
    Lapsed,
    /// No positive answer on record at all.
    Unlicensed,
}

/// Grace-window arithmetic. `last_positive_unix == 0` (the serde default) means "no
/// positive answer ever". A clock that has gone BACKWARDS past the recorded answer
/// reads as still-licensed rather than lapsed: saturating math, because punishing a
/// user for a BIOS battery is the fail-closed direction this module refuses.
pub(crate) fn entitlement_from_cache(now_unix: u64, last_positive_unix: u64) -> Entitlement {
    if last_positive_unix == 0 {
        return Entitlement::Unlicensed;
    }
    if now_unix.saturating_sub(last_positive_unix) <= GRACE_SECS {
        Entitlement::Licensed
    } else {
        Entitlement::Lapsed
    }
}

/// What the UI should do about licensing, decided once per launch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Posture {
    /// Say nothing. Personal mode, and licensed business mode, both live here:
    /// free is first-class, and nobody who paid ever sees another licensing word.
    Silent,
    /// Business mode with no valid licence: the persistent, escalating reminder.
    /// Never dismissible-forever - that is the whole point of the mode.
    BusinessNag,
    /// This machine used to run under a business licence and was reinstalled as
    /// Personal: one factual notice, one acknowledgement, then silence.
    DowngradeNoticeOnce,
    /// The seat was revoked out from under a business install: loud and specific
    /// (name the key prefix, say how to re-license), degrade to free features,
    /// never hard-fail, never touch the user's data.
    DeauthorizedLoud,
}

/// Compose the real inputs into today's posture, and log the decision so a support
/// thread can see which branch a machine took. This is the app's ONE entry point to
/// the module; the UI surfaces (the Business nag, the downgrade notice, the
/// deauthorised alert) hang off the returned value as they are built.
pub(crate) fn current_posture() -> Posture {
    let mode = read_mode();
    let history = history_path().and_then(|p| read_history(&p));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ent = entitlement_from_cache(now, history.as_ref().map_or(0, |h| h.last_positive_unix));
    let p = posture(mode, ent, history.as_ref());
    sagethumbs2k_core::safety::log_debug(&format!(
        "license: mode={mode:?} entitlement={ent:?} -> posture={p:?}"
    ));
    p
}

/// The whole behaviour matrix in one place. Exhaustive over [`Mode`] so a future
/// variant is a compile error here rather than a silent fall-through.
pub(crate) fn posture(mode: Mode, ent: Entitlement, history: Option<&History>) -> Posture {
    match mode {
        Mode::Business => match ent {
            Entitlement::Licensed => Posture::Silent,
            // Lapsed-because-revoked and never-licensed look identical to the cache;
            // the breadcrumb's last recorded status is what tells a deauthorised
            // machine ("your licence was revoked, here is how to fix it") apart from
            // one that simply never entered a serial ("this mode needs a licence").
            Entitlement::Lapsed | Entitlement::Unlicensed => {
                if history.is_some_and(|h| h.last_status == "revoked") {
                    Posture::DeauthorizedLoud
                } else {
                    Posture::BusinessNag
                }
            }
        },
        Mode::Personal => {
            if history.is_some_and(|h| h.was_business && !h.downgrade_acknowledged) {
                Posture::DowngradeNoticeOnce
            } else {
                Posture::Silent
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PID-suffixed temp dir, the repo-wide convention so concurrent `cargo test`
    /// runs (mutants baselines, parallel sessions) cannot race on one path.
    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("st2k_license_{tag}_{}", std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn everything_that_is_not_exactly_business_reads_personal() {
        assert_eq!(parse_mode(None), Mode::Personal);
        assert_eq!(parse_mode(Some("")), Mode::Personal);
        assert_eq!(parse_mode(Some("personal")), Mode::Personal);
        assert_eq!(
            parse_mode(Some("corporate")),
            Mode::Personal,
            "unknown words fail quiet"
        );
        assert_eq!(parse_mode(Some("business")), Mode::Business);
        assert_eq!(
            parse_mode(Some("  Business  ")),
            Mode::Business,
            "trim + case"
        );
    }

    /// The 7-day boundary, pinned on both sides, plus the two degenerate clocks.
    #[test]
    fn the_grace_window_is_seven_days_exactly() {
        let t = 1_760_000_000u64;
        assert_eq!(
            entitlement_from_cache(t, 0),
            Entitlement::Unlicensed,
            "no answer ever"
        );
        assert_eq!(
            entitlement_from_cache(t, t),
            Entitlement::Licensed,
            "just checked"
        );
        assert_eq!(
            entitlement_from_cache(t + GRACE_SECS, t),
            Entitlement::Licensed,
            "day 7"
        );
        assert_eq!(
            entitlement_from_cache(t + GRACE_SECS + 1, t),
            Entitlement::Lapsed,
            "one second past the window is lapsed - the fail-open has an edge and this is it"
        );
        // Clock went backwards past the recorded answer: still licensed, never
        // punished for a BIOS battery. Saturating, so also never a panic.
        assert_eq!(entitlement_from_cache(t - 500, t), Entitlement::Licensed);
    }

    /// The whole matrix. Every (mode, entitlement, history) cell the design names,
    /// so a regression in any one of them fails by name.
    #[test]
    fn the_posture_matrix_matches_the_design() {
        let revoked = History {
            last_status: "revoked".into(),
            was_business: true,
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

        // Business, licensed: total silence. Never nag somebody who paid.
        assert_eq!(
            posture(Mode::Business, Entitlement::Licensed, None),
            Posture::Silent
        );
        assert_eq!(
            posture(Mode::Business, Entitlement::Licensed, Some(&revoked)),
            Posture::Silent,
            "a live licence outranks stale history"
        );
        // Business, no licence: the nag - unless the history says the seat was
        // revoked, which upgrades it to the loud, specific version.
        assert_eq!(
            posture(Mode::Business, Entitlement::Unlicensed, None),
            Posture::BusinessNag
        );
        assert_eq!(
            posture(Mode::Business, Entitlement::Lapsed, None),
            Posture::BusinessNag
        );
        assert_eq!(
            posture(Mode::Business, Entitlement::Lapsed, Some(&revoked)),
            Posture::DeauthorizedLoud
        );
        assert_eq!(
            posture(Mode::Business, Entitlement::Unlicensed, Some(&revoked)),
            Posture::DeauthorizedLoud
        );
        // Personal: silent - except the one-time downgrade notice, which the ack
        // permanently retires.
        assert_eq!(
            posture(Mode::Personal, Entitlement::Unlicensed, None),
            Posture::Silent
        );
        assert_eq!(
            posture(Mode::Personal, Entitlement::Unlicensed, Some(&was_biz)),
            Posture::DowngradeNoticeOnce
        );
        assert_eq!(
            posture(Mode::Personal, Entitlement::Unlicensed, Some(&acked)),
            Posture::Silent,
            "acknowledged means never again"
        );
        // Personal ignores entitlement entirely: free needs no licence.
        assert_eq!(
            posture(Mode::Personal, Entitlement::Licensed, None),
            Posture::Silent
        );
    }

    #[test]
    fn the_breadcrumb_round_trips_and_tolerates_hostility() {
        let dir = temp_dir("roundtrip");
        let p = dir.join("license-history.json");
        let h = History {
            was_business: true,
            last_status: "active".into(),
            last_positive_unix: 1_760_000_000,
            key_prefix: "esk_A1B2".into(),
            downgrade_acknowledged: false,
        };
        assert!(write_history(&p, &h), "write must stick");
        assert_eq!(
            read_history(&p).as_ref(),
            Some(&h),
            "read back what was written"
        );

        // The file is users-writable, so every malformation is a quiet None.
        std::fs::write(&p, b"not json at all").unwrap();
        assert_eq!(read_history(&p), None, "garbage reads as no history");
        std::fs::write(&p, b"{}").unwrap();
        assert_eq!(
            read_history(&p),
            Some(History::default()),
            "empty object = all defaults"
        );
        std::fs::write(&p, br#"{"was_business": "yes"}"#).unwrap();
        assert_eq!(read_history(&p), None, "wrong types read as no history");
        assert_eq!(
            read_history(&dir.join("absent.json")),
            None,
            "absent reads as no history"
        );

        // Oversized: refused before it is read, per HISTORY_MAX_BYTES.
        std::fs::write(&p, vec![b' '; (HISTORY_MAX_BYTES + 1) as usize]).unwrap();
        assert_eq!(
            read_history(&p),
            None,
            "an oversized prank costs a metadata call"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Writing into a directory that does not exist yet must create it - the
    /// portable / hand-deleted-ProgramData case.
    #[test]
    fn write_creates_the_directory_when_missing() {
        let dir = temp_dir("mkdir");
        let p = dir.join("deeper").join("license-history.json");
        assert!(write_history(&p, &History::default()));
        assert!(read_history(&p).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
