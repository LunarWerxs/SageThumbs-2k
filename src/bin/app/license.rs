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

/// The current mode. Installed builds only ever read the HKLM value the elevated
/// installer wrote (see the module docs). Portable builds have no installer and
/// therefore no wizard declaration, so they default to Personal too - UNLESS this
/// copy has itself redeemed a business key, in which case [`redeem`] wrote the same
/// marker string ("business") into the portable settings store, and that is the ONE
/// thing a portable copy consults instead. Both branches funnel through the same
/// [`parse_mode`], so "business" means the same thing whichever store it came from.
pub(crate) fn read_mode() -> Mode {
    if sagethumbs2k_core::settings::portable() {
        return parse_mode(sagethumbs2k_core::settings::get_string_opt(MODE_VALUE).as_deref());
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
    /// Unix seconds of the last time [`refresh_entitlement`] made (or attempted) a
    /// network check, success or failure alike. Drives the 6-hour throttle -
    /// recorded even on failure so a machine with a dead network doesn't retry the
    /// relay every launch, which is exactly the "leave the breadcrumb alone except
    /// last_check_unix" fail-open the field exists for.
    pub last_check_unix: u64,
    /// How many times the startup deauthorised/business-nag notice has been shown.
    /// Drives the nag escalation in [`nag_due`]: less-than-30 waits a day between
    /// nags, 30-and-over nags on every launch.
    pub nag_count: u64,
    /// Unix seconds the nag was last shown. Paired with `nag_count` for the 24-hour
    /// spacing; see [`nag_due`].
    pub nag_last_unix: u64,
}

// Serialization is hand-rolled over `serde_json::Value` rather than serde-derive:
// this workspace deliberately carries serde_json WITHOUT the serde derive macros
// (see `nudge_engine`'s header note - the same trade was made there), and five
// fields do not justify adding a proc-macro dependency to every build.
impl History {
    fn to_json(&self) -> Value {
        json!({
            "was_business": self.was_business,
            "last_status": self.last_status,
            "last_positive_unix": self.last_positive_unix,
            "key_prefix": self.key_prefix,
            "downgrade_acknowledged": self.downgrade_acknowledged,
            "last_check_unix": self.last_check_unix,
            "nag_count": self.nag_count,
            "nag_last_unix": self.nag_last_unix,
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
            last_check_unix: field(obj, "last_check_unix", |v| v.as_u64(), 0)?,
            nag_count: field(obj, "nag_count", |v| v.as_u64(), 0)?,
            nag_last_unix: field(obj, "nag_last_unix", |v| v.as_u64(), 0)?,
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
///
/// Bounded through the SAME open handle the read uses (issue #227/P63), rather than a
/// `metadata()` size check followed by a separate `fs::read()`: the file lives in
/// `%ProgramData%\SageThumbs2K`, which the installer creates with user-modify permissions so
/// any account on the machine can rewrite it between those two calls, and `fs::read` reads to
/// EOF regardless of the size `metadata` reported.
pub(crate) fn read_history(path: &std::path::Path) -> Option<History> {
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

/// Best-effort write; returns whether it stuck. A failed write degrades the
/// downgrade-detection feature, not the app, so callers log and move on.
pub(crate) fn write_history(path: &std::path::Path, h: &History) -> bool {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    // Write beside the file and rename over it: a reader never sees a half-written
    // breadcrumb, and a crash mid-write leaves the old one intact.
    let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let Ok(bytes) = serde_json::to_vec_pretty(&h.to_json()) else {
        return false;
    };
    if std::fs::write(&tmp, bytes).is_err() {
        return false;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Serialises the breadcrumb's read-modify-write across the processes that share it (the
/// resident helper's periodic check, a Settings window redeeming a key, a notice being
/// acknowledged). `Local\` scopes it to this logon session, like the other app locks.
struct HistoryLock(windows::Win32::Foundation::HANDLE);

impl HistoryLock {
    /// Best-effort: a lock that cannot be created, or a wait that times out, returns `None`
    /// and the caller proceeds unlocked rather than wedging a UI thread on a leaked mutex.
    fn acquire() -> Option<Self> {
        use windows::core::w;
        use windows::Win32::Foundation::{CloseHandle, WAIT_ABANDONED, WAIT_OBJECT_0};
        use windows::Win32::System::Threading::{CreateMutexW, WaitForSingleObject};
        let h =
            unsafe { CreateMutexW(None, false, w!("Local\\SageThumbs2K.LicenceHistory")) }.ok()?;
        match unsafe { WaitForSingleObject(h, 2_000) } {
            // WAIT_ABANDONED: a previous holder died mid-edit; ownership is ours and the
            // write below replaces the whole file in one rename anyway.
            WAIT_OBJECT_0 | WAIT_ABANDONED => Some(HistoryLock(h)),
            _ => {
                let _ = unsafe { CloseHandle(h) };
                None
            }
        }
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Threading::ReleaseMutex(self.0);
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
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
    let now = now_unix();
    let ent = entitlement_from_cache(now, history.as_ref().map_or(0, |h| h.last_positive_unix));
    let p = posture(mode, ent, history.as_ref());
    sagethumbs2k_core::safety::log_debug(&format!(
        "license: mode={mode:?} entitlement={ent:?} -> posture={p:?}"
    ));
    p
}

/// Now, in Unix seconds. `SystemTime::now()` failing (a clock before 1970) reads as
/// 0, which every caller in this module already treats as "no time has passed / no
/// answer on record" - the safe direction, never the panicking one.
fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Read-modify-write the breadcrumb through one closure, so every network/decision
/// function below shares one place that knows how to load-or-default and save. Silent
/// on a missing `%ProgramData%` (portable / hand-deleted, same as [`write_history`]'s
/// own fail-open) - a machine that can't remember this reminder still isn't broken.
fn update_history(mutate: impl FnOnce(&mut History)) {
    let Some(path) = history_path() else {
        return;
    };
    let _lock = HistoryLock::acquire();
    let mut h = read_history(&path).unwrap_or_default();
    mutate(&mut h);
    let _ = write_history(&path, &h);
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

// ---------------------------------------------------------------------------------
// The relay: redeeming a key and refreshing an entitlement. Both are BLOCKING network
// calls (WinINet, via `crate::http`) - every caller of `redeem`/`refresh_entitlement`
// runs them off the UI thread. Both fail toward "don't change anything and act as if
// nothing happened" on any transport or shape surprise, same standing rule as the
// rest of the module: a flaky network must never look like a rejected key or a
// revoked seat.
// ---------------------------------------------------------------------------------

pub(crate) const RELAY_BASE: &str = "https://st2k.lunarwerx.com";

/// Per-request timeout. The relay is a small Cloudflare Worker; 15 seconds is
/// generous for it and short enough that a dead network doesn't hang the Settings
/// window for a user who is just trying to close it.
const RELAY_TIMEOUT_SECS: u64 = 15;

/// Wall-clock cap on one whole relay call. The per-request timeout above resets on every
/// partial read, so a peer trickling bytes could otherwise hold a worker thread open for
/// as long as it liked; past this the call is abandoned and reads as Offline.
const RELAY_OVERALL_SECS: u64 = 30;

/// Response size cap. Every relay reply is a few bytes of JSON; 64 KiB is headroom,
/// not an expectation, the same defensive-cap idea as [`HISTORY_MAX_BYTES`].
const RELAY_MAX_RESP_BYTES: usize = 64 * 1024;

/// The salt joined onto the machine's `MachineGuid` before hashing, so the relay
/// never sees (or could reverse-engineer) the raw Windows machine identifier, only a
/// value specific to this product. Not a secret - it is compiled into every copy of
/// the app - it exists to make the fingerprint a distinct namespace, not to be hidden.
const FINGERPRINT_SALT: &str = "SageThumbs2K-seat-v1";

/// Where Windows keeps the per-machine install identifier. Readable by any user
/// (unlike most of HKLM\SOFTWARE\Microsoft\Cryptography's siblings), which is why the
/// design doc calls it out by name as the fingerprint source.
const CRYPTOGRAPHY_KEY: &str = r"SOFTWARE\Microsoft\Cryptography";

/// SHA-256 via CNG's single-shot helper (same helper `oauth.rs::sha256` and
/// `update.rs::sha256_hex` use; copied rather than imported across `bin/app`
/// modules, per that helper's own doc comment).
fn sha256(data: &[u8]) -> Option<[u8; 32]> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    status.is_ok().then_some(out)
}

/// A stable-but-anonymous identifier for this machine: lowercase hex SHA-256 of the
/// registry's `MachineGuid` joined with [`FINGERPRINT_SALT`]. `None` only when the
/// value genuinely can't be read (locked-down machine, corrupt hive) - callers treat
/// that the same as any other network failure (see `redeem`/`refresh_entitlement`),
/// never as a reason to reject a key or deny an entitlement.
pub(crate) fn machine_fingerprint() -> Option<String> {
    let guid = windows_registry::LOCAL_MACHINE
        .open(CRYPTOGRAPHY_KEY)
        .and_then(|k| k.get_string("MachineGuid"))
        .ok()?;
    fingerprint_from_guid(&guid)
}

/// The pure half of [`machine_fingerprint`] - hashing, with the registry read
/// already done - so the hex-encoding and salting can be pinned in a test without
/// touching HKLM.
fn fingerprint_from_guid(guid: &str) -> Option<String> {
    let digest = sha256(format!("{guid}{FINGERPRINT_SALT}").as_bytes())?;
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Turn whatever a human typed or pasted into the canonical `esk_XXXXX-XXXXX-XXXXX-
/// XXXXX` shape (uppercase groups), or `None` if it isn't a licence key. Trims
/// surrounding whitespace, ignores case and dashes anywhere in the body (so a key
/// copied without its group separators, or typed in lowercase, still normalizes),
/// and otherwise requires exactly the `esk_` prefix plus 20 alphanumeric characters -
/// no more, no fewer. Deliberately strict on the SHAPE: this only decides "does this
/// look like one of our keys", never whether it is valid, which is the relay's job.
pub(crate) fn normalize_key(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    let lower = trimmed.to_ascii_lowercase();
    let rest = lower.strip_prefix("esk_")?;
    let body: String = rest.chars().filter(|&c| c != '-').collect();
    if body.len() != 20 || !body.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return None;
    }
    // The all-ASCII-alphanumeric check above means indexing by byte offset here can
    // never land inside a multi-byte character.
    let upper = body.to_ascii_uppercase();
    let groups = [&upper[0..5], &upper[5..10], &upper[10..15], &upper[15..20]];
    Some(format!("esk_{}", groups.join("-")))
}

/// The display prefix for a canonical key: `esk_` plus the first four body
/// characters, e.g. `esk_A1B2`. This is the ONLY form of a key this module ever
/// prints, logs, or shows - never the full key (see the module rules).
pub(crate) fn key_prefix(canonical: &str) -> String {
    let body = canonical.strip_prefix("esk_").unwrap_or(canonical);
    format!("esk_{}", &body[..body.len().min(4)])
}

/// What redeeming a key resulted in.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RedeemOutcome {
    /// The relay accepted the key. `key_prefix` is the relay's own echo of it (never
    /// derived locally), so what the UI shows is exactly what the relay validated.
    Redeemed { key_prefix: String },
    /// The relay explicitly said no, with a human-readable reason to show.
    Rejected { message: String },
    /// No definite answer: bad local key shape aside, everything else here (no
    /// network, a 5xx, a response we don't understand) means "try again later," not
    /// "your key is wrong" - telling someone their real key is bad because a Worker
    /// hiccupped would be a worse failure than saying nothing.
    Offline,
}

/// Redeem a licence key against the relay. BLOCKING - run on a worker thread.
pub(crate) fn redeem(raw_key: &str) -> RedeemOutcome {
    let Some(canonical) = normalize_key(raw_key) else {
        // Never even ask the relay about something that isn't shaped like one of our
        // keys - this is a local formatting problem, not a validity question.
        return RedeemOutcome::Rejected {
            message: "That doesn't look like a SageThumbs 2K licence key.".to_string(),
        };
    };
    let Some(fingerprint) = machine_fingerprint() else {
        // Can't identify this machine, so there is no request to make. Failing
        // toward Offline (not Rejected) keeps a local read hiccup from ever reading
        // to the user as "your key is bad."
        return RedeemOutcome::Offline;
    };
    let body = match serde_json::to_vec(&json!({ "key": canonical, "subject": fingerprint })) {
        Ok(b) => b,
        Err(_) => return RedeemOutcome::Offline,
    };
    let url = format!("{RELAY_BASE}/license/redeem");
    let resp = crate::http::request_with_deadline(
        "POST",
        &url,
        "Content-Type: application/json",
        &body,
        RELAY_TIMEOUT_SECS,
        RELAY_OVERALL_SECS,
        RELAY_MAX_RESP_BYTES,
    );
    let outcome = match resp {
        Some(r) => redeem_outcome_from_response(r.status, &r.body, &canonical),
        None => RedeemOutcome::Offline,
    };
    if let RedeemOutcome::Redeemed { key_prefix } = &outcome {
        let now = now_unix();
        update_history(|h| {
            h.was_business = true;
            h.last_status = "active".to_string();
            h.last_positive_unix = now;
            h.key_prefix = key_prefix.clone();
        });
        // A portable copy has no HKLM the installer could have written, so this is
        // the ONE store `read_mode` consults for it - see that function's docs.
        if sagethumbs2k_core::settings::portable() {
            let _ = sagethumbs2k_core::settings::set_string(MODE_VALUE, "business");
        }
    }
    outcome
}

/// Map an HTTP status + raw body from `POST /license/redeem` to a [`RedeemOutcome`].
/// Pulled out of [`redeem`] so it can be driven from hand-written JSON in tests
/// exactly the way the network path drives it - no fingerprint, no WinINet, no clock.
fn redeem_outcome_from_response(status: u16, body: &[u8], canonical: &str) -> RedeemOutcome {
    let json: Option<Value> = serde_json::from_slice(body).ok();
    if (200..300).contains(&status) {
        return match json
            .as_ref()
            .and_then(|v| v.get("ok"))
            .and_then(Value::as_bool)
        {
            Some(true) => match json
                .as_ref()
                .and_then(|v| v.get("keyPrefix"))
                .and_then(Value::as_str)
            {
                Some(prefix) if !prefix.is_empty() => RedeemOutcome::Redeemed {
                    key_prefix: prefix.to_string(),
                },
                // The relay accepted the key but did not echo a prefix: the local
                // prefix of the key that was sent is the same value.
                _ => RedeemOutcome::Redeemed {
                    key_prefix: key_prefix(canonical),
                },
            },
            _ => RedeemOutcome::Offline,
        };
    }
    if status == 429 {
        // The relay's rate limit, not a verdict on the key: say so, rather than let a
        // paying customer read a busy office as a bad key.
        let message = json
            .as_ref()
            .and_then(|v| v.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("Too many attempts. Try again in a few minutes.")
            .to_string();
        return RedeemOutcome::Rejected { message };
    }
    if (400..500).contains(&status) {
        return match json {
            Some(v) if v.get("ok").and_then(Value::as_bool) == Some(false) => {
                let message = v
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("That key wasn't accepted.")
                    .to_string();
                RedeemOutcome::Rejected { message }
            }
            // An unparsable 4xx body is a relay/format surprise, not a confirmed
            // rejection - don't put words in the relay's mouth it never said.
            _ => RedeemOutcome::Offline,
        };
    }
    // 5xx, redirects we don't follow, anything else: Offline.
    RedeemOutcome::Offline
}

/// How long a successful (or attempted) entitlement check holds off the next one.
/// The check is a network call on every launch's critical-ish path; 6 hours keeps a
/// business machine's status fresh without hammering the relay every open.
const REFRESH_THROTTLE_SECS: u64 = 6 * 60 * 60;

/// Pure throttle decision: due when nothing has been recorded yet, or the last
/// attempt (success OR failure - see [`refresh_entitlement`]) is more than
/// [`REFRESH_THROTTLE_SECS`] old. Saturating, so a clock that jumped backwards just
/// means "not due yet," never a panic.
fn refresh_due(now_unix: u64, last_check_unix: u64) -> bool {
    last_check_unix == 0 || now_unix.saturating_sub(last_check_unix) >= REFRESH_THROTTLE_SECS
}

/// The two fields `GET /license/check` actually answers with, decoupled from the
/// breadcrumb so [`parse_check_response`] stays a pure function of the response.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckResult {
    entitled: bool,
    status: String,
}

/// Map an HTTP status + raw body from `GET /license/check` to a [`CheckResult`].
/// `None` covers every failure shape (non-200, unparsable, missing `entitled`) - the
/// relay contract only documents 200 as a real answer; a 5xx is failure, not "no".
fn parse_check_response(status: u16, body: &[u8]) -> Option<CheckResult> {
    if status != 200 {
        return None;
    }
    let v: Value = serde_json::from_slice(body).ok()?;
    if v.get("ok").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let entitled = v.get("entitled").and_then(Value::as_bool)?;
    let status = v
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("none")
        .to_string();
    Some(CheckResult { entitled, status })
}

/// The periodic entitlement re-check. BLOCKING - run on a worker thread. Makes NO
/// network call at all (returns `None` immediately) unless this machine has some
/// reason to care about a business seat - either it is currently in Business mode,
/// or the breadcrumb remembers it once was - and even then, at most once every
/// [`REFRESH_THROTTLE_SECS`]. A Personal install that never touched a business key
/// never talks to the relay, which is the point: free is free, silently.
pub(crate) fn refresh_entitlement() -> Option<Entitlement> {
    refresh_entitlement_inner(false)
}

/// The explicit "Check now" click: the same call with the throttle skipped. A person
/// asking is not a timer, and the answer they get must be fresh.
pub(crate) fn refresh_entitlement_now() -> Option<Entitlement> {
    refresh_entitlement_inner(true)
}

fn refresh_entitlement_inner(force: bool) -> Option<Entitlement> {
    let mode = read_mode();
    let path = history_path()?;
    let history = read_history(&path);
    let was_business = history.as_ref().is_some_and(|h| h.was_business);
    if mode != Mode::Business && !was_business {
        return None;
    }
    let now = now_unix();
    let last_check = history.as_ref().map_or(0, |h| h.last_check_unix);
    if !force && !refresh_due(now, last_check) {
        return None;
    }
    // Record the attempt now, before the network call, so a dead network (which
    // never reaches the code below) still holds the throttle for the next launch
    // rather than retrying every time - "leave the breadcrumb alone except
    // last_check_unix" from the contract, applied to every failure path at once.
    update_history(|h| h.last_check_unix = now);

    let fingerprint = machine_fingerprint()?;
    let url = format!(
        "{RELAY_BASE}/license/check?subject={}",
        crate::http::form_enc(&fingerprint)
    );
    let resp = crate::http::request_with_deadline(
        "GET",
        &url,
        "",
        &[],
        RELAY_TIMEOUT_SECS,
        RELAY_OVERALL_SECS,
        RELAY_MAX_RESP_BYTES,
    )?;
    let result = parse_check_response(resp.status, &resp.body)?;

    if result.entitled {
        update_history(|h| {
            h.last_positive_unix = now;
            h.last_status = "active".to_string();
            h.was_business = true;
        });
    } else if result.status == "revoked" {
        update_history(|h| h.last_status = "revoked".to_string());
    }
    // else: a definite, understood "not entitled, not revoked either" (e.g. a
    // machine that never redeemed anything) - the throttle bump already happened
    // and there's nothing else to change.

    let refreshed = read_history(&path)?;
    Some(entitlement_from_cache(now, refreshed.last_positive_unix))
}

/// Permanently retire the one-time downgrade notice for this machine.
pub(crate) fn acknowledge_downgrade() {
    update_history(|h| h.downgrade_acknowledged = true);
}

/// How often the startup nag repeats before it starts showing on every launch.
const NAG_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Past this many shown nags, the notice stops waiting a day and shows every launch -
/// the design's deliberate final escalation for a business install that has ignored
/// a month of daily reminders.
const NAG_ESCALATION_COUNT: u64 = 30;

/// Pure escalation decision, so the 24-hour boundary and the 30-count switchover can
/// be pinned without a breadcrumb file on disk.
fn nag_due_decision(now_unix: u64, nag_count: u64, nag_last_unix: u64) -> bool {
    if nag_count >= NAG_ESCALATION_COUNT {
        return true;
    }
    now_unix.saturating_sub(nag_last_unix) >= NAG_INTERVAL_SECS
}

/// Whether the startup licensing notice should show right now. Reads the breadcrumb;
/// pair with [`note_nag_shown`] once the caller has actually shown it.
pub(crate) fn nag_due(now_unix: u64) -> bool {
    let history = history_path().and_then(|p| read_history(&p));
    let (count, last) = history.map_or((0, 0), |h| (h.nag_count, h.nag_last_unix));
    nag_due_decision(now_unix, count, last)
}

/// Record that the startup notice was just shown, advancing both the count (toward
/// the 30-nag escalation) and the 24-hour clock.
pub(crate) fn note_nag_shown(now_unix: u64) {
    update_history(|h| {
        h.nag_count = h.nag_count.saturating_add(1);
        h.nag_last_unix = now_unix;
    });
}

/// Everything the Settings page shows about licensing, read once. Bundles the same
/// mode and posture computation [`current_posture`] does with the breadcrumb
/// fields the UI displays directly (key prefix, last-known status), so a caller
/// doesn't read the breadcrumb file twice.
pub(crate) struct LicenceSnapshot {
    pub mode: Mode,
    pub posture: Posture,
    pub key_prefix: String,
    pub last_positive_unix: u64,
    pub last_status: String,
}

/// Build a [`LicenceSnapshot`]. Never touches the network - purely local reads, same
/// as [`current_posture`].
pub(crate) fn snapshot() -> LicenceSnapshot {
    let mode = read_mode();
    let history = history_path().and_then(|p| read_history(&p));
    let now = now_unix();
    let entitlement =
        entitlement_from_cache(now, history.as_ref().map_or(0, |h| h.last_positive_unix));
    let posture = posture(mode, entitlement, history.as_ref());
    LicenceSnapshot {
        mode,
        posture,
        key_prefix: history
            .as_ref()
            .map_or_else(String::new, |h| h.key_prefix.clone()),
        last_positive_unix: history.as_ref().map_or(0, |h| h.last_positive_unix),
        last_status: history.map_or_else(String::new, |h| h.last_status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
            last_check_unix: 1_760_003_600,
            nag_count: 7,
            nag_last_unix: 1_759_900_000,
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

        // Oversized: the bounded `File::take` read never grows past HISTORY_MAX_BYTES + 1
        // regardless of the file's real size, so an oversized prank costs one small read.
        std::fs::write(&p, vec![b' '; (HISTORY_MAX_BYTES + 1) as usize]).unwrap();
        assert_eq!(read_history(&p), None, "an oversized prank is refused");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Issue #227/P63: the old `metadata()`-then-`fs::read()` shape checked the size before
    /// the read, which is TOCTOU on a users-writable file. Pin the boundary the `File::take`
    /// fix must land on exactly: a file of exactly `HISTORY_MAX_BYTES` is still read (trailing
    /// whitespace after a JSON value parses fine), one byte over is refused (already pinned by
    /// `read_write_round_trip_and_every_bad_shape`'s oversized case above).
    #[test]
    fn read_history_accepts_a_file_exactly_at_the_byte_cap() {
        let dir = temp_dir("bounded_read");
        let p = dir.join("license-history.json");

        let mut at_cap = serde_json::to_vec(&History::default().to_json()).unwrap();
        assert!(
            (at_cap.len() as u64) <= HISTORY_MAX_BYTES,
            "fixture must fit under the cap before padding"
        );
        at_cap.resize(HISTORY_MAX_BYTES as usize, b' '); // trailing whitespace, still valid JSON
        assert_eq!(at_cap.len() as u64, HISTORY_MAX_BYTES);
        std::fs::write(&p, &at_cap).unwrap();
        assert_eq!(
            read_history(&p),
            Some(History::default()),
            "a file exactly at HISTORY_MAX_BYTES must still be read"
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

    // ---- normalize_key / key_prefix -----------------------------------------

    #[test]
    fn normalize_key_accepts_the_canonical_shape_case_and_dash_insensitively() {
        let canonical = "esk_A1B2C-3D4E5-F6G7H-8I9J0";
        assert_eq!(normalize_key(canonical).as_deref(), Some(canonical));
        // Lowercase, no dashes at all.
        assert_eq!(
            normalize_key("esk_a1b2c3d4e5f6g7h8i9j0").as_deref(),
            Some(canonical)
        );
        // Uppercase prefix, mixed case body, dashes in different places.
        assert_eq!(
            normalize_key("ESK_a1B2c3D4e5-f6G7h8I9j0").as_deref(),
            Some(canonical)
        );
        // Leading/trailing whitespace, the way a copy-paste often arrives.
        assert_eq!(
            normalize_key("  esk_A1B2C-3D4E5-F6G7H-8I9J0  ").as_deref(),
            Some(canonical)
        );
    }

    #[test]
    fn normalize_key_rejects_the_wrong_length_and_the_wrong_prefix() {
        assert_eq!(normalize_key(""), None, "empty");
        assert_eq!(
            normalize_key("esk_A1B2C-3D4E5-F6G7H-8I9J"),
            None,
            "one character short of 20"
        );
        assert_eq!(
            normalize_key("esk_A1B2C-3D4E5-F6G7H-8I9J00"),
            None,
            "one character over 20"
        );
        assert_eq!(
            normalize_key("xyz_A1B2C3D4E5F6G7H8I9J0"),
            None,
            "wrong prefix word"
        );
        assert_eq!(
            normalize_key("A1B2C3D4E5F6G7H8I9J0"),
            None,
            "no prefix at all"
        );
        assert_eq!(
            normalize_key("esk_A1B2C-3D4E5-F6G7H-8I9J!"),
            None,
            "a non-alphanumeric character in the body"
        );
    }

    #[test]
    fn key_prefix_is_the_marker_plus_the_first_four_body_characters() {
        assert_eq!(key_prefix("esk_A1B2C-3D4E5-F6G7H-8I9J0"), "esk_A1B2");
    }

    // ---- redeem / check JSON-to-outcome mapping -----------------------------

    #[test]
    fn redeem_response_2xx_ok_true_with_a_key_prefix_is_redeemed() {
        assert_eq!(
            map_redeem(
                200,
                br#"{"ok":true,"status":"active","keyPrefix":"esk_A1B2"}"#
            ),
            RedeemOutcome::Redeemed {
                key_prefix: "esk_A1B2".to_string()
            }
        );
    }

    #[test]
    fn redeem_response_4xx_ok_false_is_rejected_with_the_relays_message() {
        assert_eq!(
            map_redeem(
                400,
                br#"{"ok":false,"error":"invalid_key","message":"That key isn't recognized."}"#
            ),
            RedeemOutcome::Rejected {
                message: "That key isn't recognized.".to_string()
            }
        );
        assert_eq!(
            map_redeem(409, br#"{"ok":false,"error":"used"}"#),
            RedeemOutcome::Rejected {
                message: "That key wasn't accepted.".to_string()
            },
            "missing message falls back to a generic one, still Rejected"
        );
    }

    #[test]
    fn redeem_response_everything_else_is_offline() {
        assert_eq!(
            map_redeem(500, b"internal error"),
            RedeemOutcome::Offline,
            "5xx"
        );
        assert_eq!(
            map_redeem(200, b"not json"),
            RedeemOutcome::Offline,
            "unparsable 2xx"
        );
        assert_eq!(
            map_redeem(400, b"not json"),
            RedeemOutcome::Offline,
            "unparsable 4xx - never invented as a rejection"
        );
        assert_eq!(
            map_redeem(200, br#"{"ok":true}"#),
            RedeemOutcome::Redeemed {
                key_prefix: "esk_A1B2".to_string()
            },
            "2xx with no keyPrefix falls back to the local prefix of the key sent"
        );
        assert_eq!(
            map_redeem(302, b""),
            RedeemOutcome::Offline,
            "a redirect status this module doesn't chase"
        );
    }

    #[test]
    fn check_response_200_entitled_true_parses() {
        assert_eq!(
            parse_check_response(200, br#"{"ok":true,"entitled":true,"status":"active"}"#),
            Some(CheckResult {
                entitled: true,
                status: "active".to_string()
            })
        );
    }

    #[test]
    fn check_response_200_entitled_false_revoked_parses() {
        assert_eq!(
            parse_check_response(200, br#"{"ok":true,"entitled":false,"status":"revoked"}"#),
            Some(CheckResult {
                entitled: false,
                status: "revoked".to_string()
            })
        );
    }

    #[test]
    fn check_response_failures_are_none() {
        assert_eq!(parse_check_response(500, b""), None, "5xx");
        assert_eq!(parse_check_response(200, b"not json"), None, "unparsable");
        assert_eq!(
            parse_check_response(200, br#"{"ok":false}"#),
            None,
            "ok:false"
        );
        assert_eq!(
            parse_check_response(200, br#"{"ok":true,"status":"active"}"#),
            None,
            "missing entitled"
        );
    }

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
            posture(Mode::Personal, Entitlement::Licensed, Some(&was_biz)),
            Posture::Silent,
            "a redeemed key within grace outranks the installer's Personal answer"
        );
        assert_eq!(
            posture(Mode::Personal, Entitlement::Lapsed, Some(&was_biz)),
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

    #[test]
    fn fingerprint_from_guid_is_deterministic_lowercase_hex() {
        let a = fingerprint_from_guid("11111111-2222-3333-4444-555555555555").unwrap();
        let b = fingerprint_from_guid("11111111-2222-3333-4444-555555555555").unwrap();
        let c = fingerprint_from_guid("00000000-0000-0000-0000-000000000000").unwrap();
        assert_eq!(a, b, "same GUID hashes the same every time");
        assert_ne!(a, c, "different GUIDs must not collide");
        assert_eq!(a.len(), 64, "SHA-256 as lowercase hex is 64 characters");
        assert!(a
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
    }

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
}
