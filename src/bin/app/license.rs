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
    /// WHY, when `last_status` is "revoked": the relay's `reason` token (`seat_revoked`,
    /// `contract_ended`, ...), empty when it sent none or the status is not a revocation.
    /// Display only, through `settings_dlg::licence_reason_line`; never a decision input.
    pub last_reason: String,
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
    /// Unix seconds this machine's UPDATES WINDOW ends - the relay's `maintenanceEndsAt`,
    /// recorded on every successful entitlement check (2026-09-10). `0` means "no window on
    /// record", which is NOT "the window closed": it is the state every machine was in
    /// before this field existed, and it keeps every build offered.
    ///
    /// ⛔ This is an UPDATES fact, never a LICENCE fact. A lapsed window stops new builds
    /// being OFFERED and does nothing else - the licence is perpetual, the installed version
    /// keeps working, and no code path may read this to decide `Entitlement`.
    pub maint_unix: u64,
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
            "last_reason": self.last_reason,
            "last_positive_unix": self.last_positive_unix,
            "key_prefix": self.key_prefix,
            "downgrade_acknowledged": self.downgrade_acknowledged,
            "last_check_unix": self.last_check_unix,
            "nag_count": self.nag_count,
            "nag_last_unix": self.nag_last_unix,
            "maint_unix": self.maint_unix,
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
            last_reason: field(
                obj,
                "last_reason",
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
            maint_unix: field(obj, "maint_unix", |v| v.as_u64(), 0)?,
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
///
/// Atomic via [`sagethumbs2k_core::fsutil::write_atomically`] (temp file beside `path`,
/// then a retrying rename over it): a reader never sees a half-written breadcrumb, and a
/// crash mid-write leaves the old one intact. Atomicity alone does NOT stop two writers
/// from each replacing the file with their own view of it and losing the other's update
/// (2026-09-05 audit, F18) - that is what [`HistoryLock`] is for. This function assumes
/// the lock is already held; the only caller in this module (`update_history_at`) takes
/// it first.
pub(crate) fn write_history(path: &std::path::Path, h: &History) -> bool {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let Ok(bytes) = serde_json::to_vec_pretty(&h.to_json()) else {
        return false;
    };
    sagethumbs2k_core::fsutil::write_atomically(path, &bytes).is_ok()
}

/// How many times [`HistoryLock::acquire`] retries before giving up, and how long it waits
/// between tries. `LockFile` fails immediately rather than blocking when the region is
/// already held (blocking needs `LockFileEx` plus an `OVERLAPPED`, which this one call
/// site does not otherwise need), so the ~2s budget the old session-local mutex gave
/// callers via `WaitForSingleObject` is reproduced here as bounded polling instead. Short
/// under `cfg(test)` so a test that deliberately holds the lock costs milliseconds, not
/// the production budget.
#[cfg(not(test))]
const LOCK_ATTEMPTS: u32 = 20;
#[cfg(test)]
const LOCK_ATTEMPTS: u32 = 6;
#[cfg(not(test))]
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(100);
#[cfg(test)]
const LOCK_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(25);

/// Serialises the breadcrumb's read-modify-write across every process that touches it, on
/// this MACHINE rather than this logon session (2026-09-05 audit, F18). The lock this
/// replaces (`Local\SageThumbs2K.LicenceHistory`) named a kernel object scoped to the
/// caller's session, so two Windows sessions on one box - two users, or RDP layered over
/// the console - each got their OWN mutex and could both believe they held exclusive
/// access to the SAME shared ProgramData file, the second write silently discarding the
/// first's. A file lock has no session namespace: `LockFile` contends on the file itself,
/// and a file has exactly one identity no matter which session opened it.
struct HistoryLock {
    file: std::fs::File,
}

impl HistoryLock {
    /// Open (creating if needed) the lock file beside the breadcrumb and take an exclusive
    /// whole-file lock on it, retrying up to [`LOCK_ATTEMPTS`] times. `None` once that
    /// budget is spent - the caller's contract is to write NOTHING in that case (see
    /// [`update_history_at`]), never to fall back to writing unlocked the way the old
    /// mutex path did on a timed-out wait.
    fn acquire(lock_path: &std::path::Path) -> Option<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::LockFile;

        if let Some(dir) = lock_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            // The lock file carries no content that matters - it exists only to be
            // locked - but a truncate on every acquire would still be a pointless write
            // to a file another process might be mid-open on. Never truncate it.
            .truncate(false)
            .open(lock_path)
            .ok()?;
        let handle = HANDLE(file.as_raw_handle());
        for attempt in 0..LOCK_ATTEMPTS {
            // SAFETY: `file` outlives this call and is kept alive inside the returned
            // `HistoryLock` for as long as the lock must hold; a whole-file range (offset
            // 0, length u32::MAX/u32::MAX) is the standard Windows idiom for "lock the
            // file" regardless of its actual length.
            let locked = unsafe { LockFile(handle, 0, 0, u32::MAX, u32::MAX) }.is_ok();
            if locked {
                return Some(HistoryLock { file });
            }
            if attempt + 1 < LOCK_ATTEMPTS {
                std::thread::sleep(LOCK_RETRY_DELAY);
            }
        }
        None
    }
}

impl Drop for HistoryLock {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::Foundation::HANDLE;
        use windows::Win32::Storage::FileSystem::UnlockFile;

        let handle = HANDLE(self.file.as_raw_handle());
        // SAFETY: the same handle and region locked in `acquire`; unlocking a region this
        // handle does not hold is a documented failure return, never undefined behaviour.
        unsafe {
            let _ = UnlockFile(handle, 0, 0, u32::MAX, u32::MAX);
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

/// Does a stored offline certificate license THIS machine right now?
///
/// Reads the breadcrumb's neighbour rather than the network: see [`crate::licence_cert`]
/// for the whole model. Every failure - no certificate, no fingerprint, a blob from
/// another machine, an expired one - answers `false`, which only ever means "the
/// certificate has nothing to add", never "unlicensed".
///
/// `licensed` is what this reads; the certificate's `maint_unix` is read separately by
/// `LicenceSnapshot` as the offline fallback for the updates window (the relay's
/// `maintenanceEndsAt` wins when the breadcrumb has one).
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
    let ent = entitlement_now(now, history.as_ref());
    let p = posture(mode, ent, history.as_ref());
    sagethumbs2k_core::safety::log_debugf!(
        "license: mode={mode:?} entitlement={ent:?} -> posture={p:?}"
    );
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
    update_history_at(&path, mutate);
}

/// The testable core of [`update_history`]: an explicit path, and a return value saying
/// whether the update actually happened, so a test can tell "lost the lock race" apart
/// from "wrote, and it happened to be a no-op".
///
/// 2026-09-05 audit, F18: no lock now means no write, not a write proceeding unlocked. The
/// old code called `HistoryLock::acquire`, ignored a `None`, and wrote anyway - which is
/// exactly how two sessions each racing their own separate `Local\` mutex could both
/// believe they held exclusive access and clobber each other's write. Losing one update (a
/// stale nag count, a downgrade notice shown once more than it should be) is a cosmetic
/// cost; silently replacing a newer write from another session is the bug this function
/// now refuses to reproduce.
fn update_history_at(path: &std::path::Path, mutate: impl FnOnce(&mut History)) -> bool {
    let Some(_lock) = HistoryLock::acquire(&lock_path(path)) else {
        sagethumbs2k_core::safety::log_debug(
            "license: history lock unavailable after retrying, skipping this update rather than writing over a possibly newer file",
        );
        return false;
    };
    let mut h = read_history(path).unwrap_or_default();
    mutate(&mut h);
    write_history(path, &h)
}

/// Where the machine-wide lock lives: a sibling of the breadcrumb, never the breadcrumb
/// file itself, so taking the lock can never race the JSON file's own atomic replace.
fn lock_path(path: &std::path::Path) -> std::path::PathBuf {
    path.with_extension("lock")
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

/// Where a licence is bought. A relay redirect rather than the checkout's own address on
/// purpose: the checkout is a Pay offer whose id changes with a repricing or a new product,
/// and this string is compiled into every copy ever shipped. The redirect moves; this does
/// not. Opened by the Licence page's Buy button and named in the business-nag notices.
pub(crate) const BUY_URL: &str = "https://st2k.lunarwerx.com/buy";

/// Where another 12 months of updates is bought (US$29), for a licence that is already
/// held. Unlike [`BUY_URL`] this is the checkout's own address rather than a relay
/// redirect on the relay (`/renew`, the twin of `/buy`), which forwards the query string, so a
/// repricing or a new checkout page moves the link without a release. The relay's default
/// target is the checkout page for [`crate::licence_cert::PRODUCT_ID`].
const RENEW_URL: &str = "https://st2k.lunarwerx.com/renew";

/// The renewal link for this machine: the checkout page for our product, with the stored
/// licence key pre-filled when we have one.
///
/// The key comes from [`crate::cred_store`] (per-user, DPAPI-encrypted), NEVER from the
/// breadcrumb, which by contract holds only a display prefix - see that store's
/// `V_LICENCE_KEY` note. With no stored key the bare page is opened and the checkout asks
/// for it, which is a worse experience and a perfectly correct one; a prefix is never
/// substituted, because `?key=esk_A1B2` would look like a key and redeem nothing.
pub(crate) fn renew_url() -> String {
    match crate::cred_store::load_licence_key()
        .as_deref()
        .and_then(normalize_key)
    {
        Some(key) => format!("{RENEW_URL}?key={}", crate::http::form_enc(&key)),
        None => RENEW_URL.to_string(),
    }
}

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
    // The certificate rides along on the SAME response, so it costs no extra call. Read
    // separately from the outcome mapping rather than threaded through `RedeemOutcome`,
    // which would change a shape the Settings page and a dozen tests already match on.
    let (outcome, certificate) = match resp {
        Some(r) => (
            redeem_outcome_from_response(r.status, &r.body, &canonical),
            certificate_from_response(&r.body),
        ),
        None => (RedeemOutcome::Offline, None),
    };
    if let RedeemOutcome::Redeemed { key_prefix } = &outcome {
        // Keep the certificate before anything else: from here on this machine can prove
        // its own licence with no network, which is the whole point of having one.
        // Best-effort - a machine that cannot store it is exactly as licensed as before.
        if let Some(cert) = &certificate {
            let _ = crate::cred_store::save_licence_cert(cert);
        }
        // Keep the key too, in the SAME per-user DPAPI store, so the Renew button can hand
        // it to the checkout instead of making the customer dig out their purchase email.
        // This is the one place a full key is written and `cred_store::V_LICENCE_KEY` says
        // why it is there and not in the breadcrumb.
        let _ = crate::cred_store::save_licence_key(&canonical);
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

/// Pull the offline certificate out of a redeem response, if it sent one.
///
/// Pure, and deliberately incurious: anything that is not a non-empty string is simply
/// absent. A relay that has not been taught to forward the certificate yet, an older
/// deployment, a truncated body - all of them mean "no certificate", which costs nothing
/// because the relay breadcrumb licenses the machine exactly as it did before.
fn certificate_from_response(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<Value>(body)
        .ok()?
        .get("certificate")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .map(String::from)
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

/// What `GET /license/check` actually answers with, decoupled from the breadcrumb so
/// [`parse_check_response`] stays a pure function of the response.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CheckResult {
    entitled: bool,
    status: String,
    /// The relay's optional `reason` token (a short `[a-z0-9_]` word such as
    /// `seat_revoked` / `contract_ended`), kept only when it has that shape.
    reason: Option<String>,
    /// The relay's `maintenanceActive` flag: is this seat still inside its 12 months of
    /// updates. `None` when the relay sent none (an older deployment, or a Pay that does
    /// not answer it) - which is "unknown", never "no".
    ///
    /// Carried for completeness and for the debug log; `maint_unix` is what every decision
    /// actually reads, because a DATE can be compared against a release's publication date
    /// and a boolean cannot.
    maintenance_active: Option<bool>,
    /// `maintenanceEndsAt` parsed to Unix seconds. `None` for absent, null, or anything
    /// that does not parse - all of which mean "no window on record" and leave every build
    /// offered.
    maint_unix: Option<u64>,
}

/// Parse the ISO-8601 instant the relay sends for `maintenanceEndsAt` into Unix seconds.
///
/// Deliberately narrow, and pure so the boundaries can be pinned in tests: `YYYY-MM-DD`
/// optionally followed by `T`/space, `HH:MM[:SS]`, an optional fractional part, and an
/// optional `Z`. An offset other than `Z` is REFUSED rather than silently read as UTC -
/// this value decides whether a customer is offered a build, so guessing hours is worse
/// than answering "no window on record", which is the safe direction (every build stays
/// offered). Adding a full offset parser is a change to make when Pay actually sends one.
///
/// No chrono/time dependency for one call site, same trade `settings_dlg::format_unix_date`
/// already made in the other direction; the civil-days arithmetic below is Howard Hinnant's
/// `days_from_civil`, valid for any Gregorian date this app will ever see.
pub(crate) fn parse_iso_unix(s: &str) -> Option<u64> {
    let s = s.trim();
    let (date, rest) = s.split_at(s.char_indices().nth(10).map_or(s.len(), |(i, _)| i));
    let (year, month, day) = parse_iso_date(date)?;
    let (hour, minute, second) = if rest.is_empty() {
        (0, 0, 0)
    } else {
        parse_iso_time(rest)?
    };
    let days = days_from_civil(year, month, day);
    u64::try_from(days * 86_400 + hour * 3600 + minute * 60 + second).ok()
}

/// `YYYY-MM-DD` with a plausible month and day (the civil-days arithmetic tolerates a 31st of
/// a short month; the relay never sends one, and refusing it here would only move the guess).
fn parse_iso_date(date: &str) -> Option<(i64, i64, i64)> {
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// The part after the date: a `T`/`t`/space separator, `HH:MM[:SS]`, optional fraction,
/// optional `Z`. A numeric offset is refused (see [`parse_iso_unix`]).
fn parse_iso_time(rest: &str) -> Option<(i64, i64, i64)> {
    let time = rest
        .strip_prefix('T')
        .or_else(|| rest.strip_prefix('t'))
        .or_else(|| rest.strip_prefix(' '))?;
    let time = time.trim_end_matches(['Z', 'z']);
    let time = time.split('.').next().unwrap_or(time);
    if time.contains('+') || time.contains('-') {
        return None;
    }
    let mut hms = time.split(':');
    let h: i64 = hms.next()?.parse().ok()?;
    let mi: i64 = hms.next()?.parse().ok()?;
    let se: i64 = hms.next().map_or(Ok(0), str::parse).ok()?;
    if hms.next().is_some() || h > 23 || mi > 59 || se > 60 {
        return None;
    }
    Some((h, mi, se))
}

/// Howard Hinnant's `days_from_civil`: days since 1970-01-01 for a proleptic Gregorian date.
/// The year is shifted to start in March so the leap day lands last.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Is `s` a reason token the way the relay defines one: 1-40 chars of `[a-z0-9_]`? Anything
/// else is dropped rather than stored, so a surprising relay can never put arbitrary text
/// into the breadcrumb or onto the screen.
fn is_reason_token(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 40
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
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
    let reason = v
        .get("reason")
        .and_then(Value::as_str)
        .filter(|r| is_reason_token(r))
        .map(String::from);
    let maintenance_active = v.get("maintenanceActive").and_then(Value::as_bool);
    let maint_unix = v
        .get("maintenanceEndsAt")
        .and_then(Value::as_str)
        .and_then(parse_iso_unix);
    Some(CheckResult {
        entitled,
        status,
        reason,
        maintenance_active,
        maint_unix,
    })
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
    apply_check_response(&path, now, resp.status, &resp.body)
}

/// The response-handling half of [`refresh_entitlement_inner`] (E05 follow-up audit, review
/// item 4b): given the relay's raw status/body for `GET /license/check`, decide what (if
/// anything) to write to the breadcrumb and return the resulting entitlement. Split out so a
/// test can drive the REAL decision with a hand-written failure response, rather than
/// re-implementing its if-let in the test and only ever proving the test's own copy correct.
fn apply_check_response(
    path: &std::path::Path,
    now: u64,
    status: u16,
    body: &[u8],
) -> Option<Entitlement> {
    let result = parse_check_response(status, body)?;
    sagethumbs2k_core::safety::log_debugf!(
        "license: check entitled={} status={} maintenance_active={:?} maint_unix={:?}",
        result.entitled,
        result.status,
        result.maintenance_active,
        result.maint_unix
    );

    // The updates window is recorded on EVERY understood answer, entitled or not, and
    // separately from the entitlement branches below - it is a different fact with a
    // different lifetime (a perpetual licence outlives its update window by design), so a
    // seat that has gone quiet must not also lose the date the Licence page shows. A
    // response that carries no window leaves whatever is on record alone rather than
    // clearing it: "the relay didn't say" is not "the window is gone".
    if let Some(maint) = result.maint_unix {
        update_history_at(path, |h| h.maint_unix = maint);
    }

    if result.entitled {
        update_history_at(path, |h| {
            h.last_positive_unix = now;
            h.last_status = "active".to_string();
            h.last_reason.clear();
            h.was_business = true;
        });
    } else if result.status == "revoked" {
        update_history_at(path, |h| {
            h.last_status = "revoked".to_string();
            h.last_reason = result.reason.clone().unwrap_or_default();
        });
    }
    // else: a definite, understood "not entitled, not revoked either" (e.g. a
    // machine that never redeemed anything) - the throttle bump already happened
    // and there's nothing else to change.

    let refreshed = read_history(path)?;
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
    /// See [`History::last_reason`].
    pub last_reason: String,
    /// The offline certificate's `exp`, in Unix seconds, ONLY when the certificate is the
    /// reason this machine is licensed (E05 audit; see
    /// [`entitlement_and_cert_expiry`]). `None` whenever a relay verification already
    /// grants the licence, or there is simply no matching certificate.
    pub cert_expires_unix: Option<i64>,
    /// When this machine's 12 months of updates end, in Unix seconds, or `None` for "no
    /// window on record" (2026-09-10). The relay breadcrumb's `maint_unix` first; the
    /// offline certificate's `maint` claim only when the breadcrumb has none.
    ///
    /// ⛔ Read this ONLY to decide whether to OFFER a new build and what the Licence page
    /// says about it. It is not, and must never become, an input to `licensed`: the licence
    /// is perpetual and a closed window changes nothing about the copy already installed.
    pub maint_unix: Option<u64>,
    /// The instant this snapshot was built - the same `now_unix` [`at`] was given.
    /// Carried alongside `cert_expires_unix` so a renderer compares the two against each
    /// other rather than reading the wall clock a second time (the whole point of
    /// threading a clock through instead of calling [`now_unix`] wherever one is needed).
    pub now_unix: u64,
    /// This machine holds a LIVE business licence right now - the [`Entitlement::Licensed`] answer
    /// [`posture`] acts on, minus a revocation the relay has since reported (see
    /// [`is_live_licence`]). Carried so the Settings status line can tell a key redeemed on
    /// THIS copy (licensed, whatever the installer was told) apart from a stale breadcrumb left by
    /// a former Business install (not licensed; the downgrade notice owns that story). Without it
    /// a Personal install that redeemed a key showed "licence is active" beside "Personal use, no
    /// licence needed" (2026-09-11).
    pub entitled: bool,
}

/// Is this a LIVE business licence for the Settings page to name? [`Entitlement::Licensed`], and
/// not a machine the relay has since told us was revoked. The grace window keeps a cached positive
/// for seven days so an OFFLINE machine is not punished; it was never meant to go on describing a
/// licence the relay has explicitly taken back, and the page reading "active" beside "revoked" was
/// the result (owner test, 2026-09-11). Display only: [`posture`] still acts on the entitlement.
fn is_live_licence(entitlement: Entitlement, last_status: &str) -> bool {
    entitlement == Entitlement::Licensed && last_status != "revoked"
}

/// Build a [`LicenceSnapshot`] as of `now_unix`. The one place this module's wall clock is
/// read is [`snapshot`] below; every decision in here is a pure function of `now_unix`, so
/// a test (or a fake-clock caller) can pin any instant it likes by calling this directly.
pub(crate) fn at(now_unix: u64) -> LicenceSnapshot {
    let mode = read_mode();
    let history = history_path().and_then(|p| read_history(&p));
    let (entitlement, cert_expires_unix) = entitlement_and_cert_expiry(now_unix, history.as_ref());
    let entitled = is_live_licence(
        entitlement,
        history.as_ref().map_or("", |h| h.last_status.as_str()),
    );
    let posture = posture(mode, entitlement, history.as_ref());
    // The relay's recorded window wins; the certificate is the floor beneath it, exactly the
    // ordering `entitlement_and_cert_expiry` uses for the entitlement itself. `0` in the
    // breadcrumb is "never recorded", not "ended in 1970" - the one reading that would
    // silently stop offering updates to every machine that has not checked in yet.
    let maint_unix = match history.as_ref().map_or(0, |h| h.maint_unix) {
        0 => certificate_maint_unix(now_unix),
        recorded => Some(recorded),
    };
    LicenceSnapshot {
        entitled,
        mode,
        posture,
        key_prefix: history
            .as_ref()
            .map_or_else(String::new, |h| h.key_prefix.clone()),
        last_positive_unix: history.as_ref().map_or(0, |h| h.last_positive_unix),
        last_status: history
            .as_ref()
            .map_or_else(String::new, |h| h.last_status.clone()),
        last_reason: history.map_or_else(String::new, |h| h.last_reason),
        cert_expires_unix,
        maint_unix,
        now_unix,
    }
}

/// Build a [`LicenceSnapshot`] for right now. Never touches the network - purely local
/// reads, same as [`current_posture`]. Thin wrapper over [`at`] so every other caller
/// (tests included) can pin the clock instead.
pub(crate) fn snapshot() -> LicenceSnapshot {
    at(now_unix())
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

    /// A relay-reported revocation ends what the Settings page calls a live licence at once, even
    /// while the cached positive is still inside its grace window (owner test, 2026-09-11: the page
    /// read "Business licence active" beside "Business licence revoked").
    #[test]
    fn a_revocation_the_relay_reported_is_not_a_live_licence() {
        assert!(is_live_licence(Entitlement::Licensed, "active"));
        assert!(!is_live_licence(Entitlement::Licensed, "revoked"));
        assert!(!is_live_licence(Entitlement::Lapsed, "active"));
        assert!(!is_live_licence(Entitlement::Unlicensed, ""));
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
            last_reason: "contract_ended".into(),
            last_positive_unix: 1_760_000_000,
            key_prefix: "esk_A1B2".into(),
            downgrade_acknowledged: false,
            last_check_unix: 1_760_003_600,
            nag_count: 7,
            nag_last_unix: 1_759_900_000,
            maint_unix: 1_791_500_000,
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
                status: "active".to_string(),
                reason: None,
                maintenance_active: None,
                maint_unix: None,
            })
        );
    }

    #[test]
    fn check_response_200_entitled_false_revoked_parses() {
        assert_eq!(
            parse_check_response(200, br#"{"ok":true,"entitled":false,"status":"revoked"}"#),
            Some(CheckResult {
                entitled: false,
                status: "revoked".to_string(),
                reason: None,
                maintenance_active: None,
                maint_unix: None,
            })
        );
    }

    /// The relay's `reason` rides along only in the shape the relay defines for it; anything
    /// else is dropped, never stored, never shown.
    #[test]
    fn check_response_keeps_a_well_formed_reason_and_drops_the_rest() {
        let with = |body: &[u8]| parse_check_response(200, body).and_then(|r| r.reason);
        assert_eq!(
            with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":"seat_revoked"}"#),
            Some("seat_revoked".to_string())
        );
        assert_eq!(
            with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":"contract_ended"}"#),
            Some("contract_ended".to_string())
        );
        assert_eq!(
            with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":"Seat Revoked!"}"#),
            None,
            "not a token"
        );
        assert_eq!(
            with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":""}"#),
            None,
            "empty"
        );
        assert_eq!(
            with(br#"{"ok":true,"entitled":false,"status":"revoked","reason":7}"#),
            None,
            "wrong type"
        );
    }

    /// The 2026-09-10 maintenance fields, present and absent. ABSENT IS THE LOAD-BEARING
    /// CASE: every relay deployment older than that date, and every Pay that does not answer
    /// them, sends neither - and both must read as "no window on record" (`None`), which
    /// leaves every build offered, rather than as a closed window, which would stop offering
    /// updates to the entire installed base at once.
    #[test]
    fn check_response_reads_the_maintenance_window_and_tolerates_its_absence() {
        let with = |body: &[u8]| parse_check_response(200, body).expect("a 200 answer");

        let full = with(
            br#"{"ok":true,"entitled":true,"status":"active","maintenanceActive":true,"maintenanceEndsAt":"2027-09-03T00:00:00Z"}"#,
        );
        assert_eq!(full.maintenance_active, Some(true));
        assert_eq!(full.maint_unix, Some(1_819_929_600));

        let bare = with(br#"{"ok":true,"entitled":true,"status":"active"}"#);
        assert_eq!(bare.maintenance_active, None);
        assert_eq!(bare.maint_unix, None, "absent is unknown, never expired");

        // An explicit null, and a date we cannot parse, both mean the same thing.
        let nulled = with(
            br#"{"ok":true,"entitled":true,"status":"active","maintenanceActive":null,"maintenanceEndsAt":null}"#,
        );
        assert_eq!(nulled.maint_unix, None);
        let junk = with(
            br#"{"ok":true,"entitled":true,"status":"active","maintenanceEndsAt":"whenever"}"#,
        );
        assert_eq!(junk.maint_unix, None);

        // A window that has closed is still a perfectly good answer, and says nothing about
        // whether the machine is entitled.
        let closed = with(
            br#"{"ok":true,"entitled":true,"status":"active","maintenanceActive":false,"maintenanceEndsAt":"2025-01-01"}"#,
        );
        assert!(closed.entitled, "a lapsed window never unlicenses");
        assert_eq!(closed.maintenance_active, Some(false));
        assert_eq!(closed.maint_unix, Some(1_735_689_600));
    }

    /// [`parse_iso_unix`] at the shapes the relay actually sends, and refusing the ones it
    /// must not guess at. The reference values are the well-known Unix epochs for those
    /// instants; a wrong civil-days implementation misses them by whole days.
    #[test]
    fn iso_instants_parse_and_ambiguous_ones_are_refused() {
        assert_eq!(parse_iso_unix("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso_unix("2000-03-01"),
            Some(951_868_800),
            "leap-year rule"
        );
        assert_eq!(parse_iso_unix("2026-09-10T12:34:56Z"), Some(1_789_043_696));
        assert_eq!(
            parse_iso_unix("2026-09-10T12:34:56.789Z"),
            Some(1_789_043_696)
        );
        assert_eq!(parse_iso_unix(" 2026-09-10 12:34 "), Some(1_789_043_640));
        assert_eq!(parse_iso_unix("2026-09-10T12:34"), Some(1_789_043_640));

        // An offset other than Z is REFUSED rather than silently read as UTC: this value
        // decides whether a customer is offered a build, and guessing hours is worse than
        // answering "no window on record".
        assert_eq!(parse_iso_unix("2026-09-10T12:34:56+02:00"), None);
        assert_eq!(parse_iso_unix("2026-09-10T12:34:56-05:00"), None);

        assert_eq!(parse_iso_unix(""), None);
        assert_eq!(parse_iso_unix("whenever"), None);
        assert_eq!(parse_iso_unix("2026-13-01"), None, "month out of range");
        assert_eq!(
            parse_iso_unix("2026-09-10T25:00:00Z"),
            None,
            "hour out of range"
        );
        assert_eq!(
            parse_iso_unix("1969-12-31"),
            None,
            "before the epoch has no u64"
        );
    }

    /// The window is recorded on EVERY understood answer and is a separate fact from the
    /// entitlement: a relay that stops saying `entitled` must not also wipe the date the
    /// Licence page shows, and an answer carrying no window must leave what is on record
    /// alone rather than clearing it.
    #[test]
    fn a_check_persists_the_maintenance_window_without_touching_the_licence() {
        let dir = temp_dir("maint");
        let path = dir.join("history.json");
        let _ = std::fs::remove_file(&path);
        let now = 1_760_000_000u64;

        apply_check_response(
            &path,
            now,
            200,
            br#"{"ok":true,"entitled":true,"status":"active","maintenanceEndsAt":"2027-09-03T00:00:00Z"}"#,
        )
        .expect("an entitled answer");
        let after = read_history(&path).expect("history");
        assert_eq!(after.maint_unix, 1_819_929_600);
        assert_eq!(after.last_positive_unix, now);

        // A later answer with no window at all leaves the recorded one standing.
        apply_check_response(
            &path,
            now + 60,
            200,
            br#"{"ok":true,"entitled":true,"status":"active"}"#,
        )
        .expect("a second answer");
        assert_eq!(
            read_history(&path).expect("history").maint_unix,
            1_819_929_600,
            "silence about the window is not news about the window"
        );

        // And a CLOSED window still leaves the machine licensed - the one property this
        // whole feature must never break.
        let ent = apply_check_response(
            &path,
            now + 120,
            200,
            br#"{"ok":true,"entitled":true,"status":"active","maintenanceActive":false,"maintenanceEndsAt":"2025-01-01"}"#,
        )
        .expect("a third answer");
        assert_eq!(
            ent,
            Entitlement::Licensed,
            "a lapsed window never unlicenses"
        );
        assert_eq!(
            read_history(&path).expect("history").maint_unix,
            1_735_689_600
        );

        let _ = std::fs::remove_file(&path);
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

    // ---- the offline certificate, as a floor under the relay ----------------------

    #[test]
    fn a_redeem_response_carrying_a_certificate_yields_it() {
        let body = br#"{"ok":true,"keyPrefix":"esk_A1B2","certificate":"aGVhZGVy.c2ln"}"#;
        assert_eq!(
            certificate_from_response(body).as_deref(),
            Some("aGVhZGVy.c2ln")
        );
    }

    #[test]
    fn a_redeem_response_without_one_is_simply_absent() {
        // A relay that has not been taught to forward it yet, an empty string, a body
        // that is not JSON at all: every one of these means "no certificate", never an
        // error, because the breadcrumb licenses the machine exactly as it did before.
        for body in [
            br#"{"ok":true,"keyPrefix":"esk_A1B2"}"#.as_slice(),
            br#"{"ok":true,"certificate":""}"#.as_slice(),
            br#"{"ok":true,"certificate":null}"#.as_slice(),
            br#"{"ok":true,"certificate":123}"#.as_slice(),
            b"not json at all".as_slice(),
            b"".as_slice(),
        ] {
            assert_eq!(certificate_from_response(body), None);
        }
    }

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
                Mode::Business,
                combine_entitlement(Entitlement::Unlicensed, true, true),
                Some(&revoked)
            ),
            Posture::DeauthorizedLoud
        );
    }

    // ---- the machine-wide history lock (2026-09-05 audit, F18) -----------------

    /// Two "sessions" (two OS threads, each opening its own `File` handle to the SAME lock
    /// path - which is what actually distinguishes two logon sessions on Windows, not
    /// which thread happens to run the code) both bump the breadcrumb through
    /// [`update_history_at`] at the same time. If the lock is real mutual exclusion,
    /// neither writer's read-modify-write critical section can overlap the other's, so
    /// BOTH increments land - there is no interleaving in which one is lost, independent
    /// of thread scheduling. Against the pre-fix `Local\` mutex this same shape would not
    /// even prove anything (two threads in one test process share one logon session, so
    /// that mutex serializes them too); what the pre-fix design could not survive is two
    /// DIFFERENT sessions, which `without_a_shared_lock_two_racing_writers_can_lose_an_update`
    /// below reproduces directly, since a real second session can't be created here.
    #[test]
    fn two_concurrent_sessions_through_the_lock_both_preserve_their_change() {
        let dir = temp_dir("lock_concurrent");
        let path = dir.join("license-history.json");
        assert!(write_history(&path, &History::default()));

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let mut joins = Vec::new();
        for who in 0..2u64 {
            let path = path.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            joins.push(std::thread::spawn(move || {
                barrier.wait(); // start both "sessions" as close together as possible
                                // `update_history_at` refuses to write when the lock is not free within
                                // its (test-shortened, ~150 ms) budget, and a slow CI runner can hold the
                                // other session's read-modify-write open longer than that: on 2026-09-08
                                // the GitHub windows runner did, and this test blamed the LOCK for an
                                // increment that was never attempted. So a session that lost the lock
                                // race tries again, as a real second session would on its next run. A lock
                                // that let the second writer THROUGH is still caught below: that write
                                // succeeds and clobbers, and the count comes out one short whatever the
                                // retries did.
                let mut tries = 0u32;
                loop {
                    let wrote = update_history_at(&path, |h| {
                        h.nag_count += 1;
                        if who == 0 {
                            h.key_prefix = "esk_SESA".to_string();
                        } else {
                            h.last_status = "session-b".to_string();
                        }
                    });
                    if wrote {
                        break;
                    }
                    tries += 1;
                    assert!(
                        tries < 200,
                        "session {who} could not take the history lock in 200 tries"
                    );
                }
            }));
        }
        for j in joins {
            j.join().expect("writer thread must not panic");
        }

        let result = read_history(&path).expect("breadcrumb must still parse");
        assert_eq!(
            result.nag_count, 2,
            "both sessions' increments must land - a lost update means the lock let a \
             second writer through while the first's read-modify-write was in flight"
        );
        assert_eq!(result.key_prefix, "esk_SESA", "session A's field survives");
        assert_eq!(
            result.last_status, "session-b",
            "session B's field ALSO survives"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What the OLD `Local\SageThumbs2K.LicenceHistory` mutex actually gave two DIFFERENT
    /// Windows sessions: nothing. Each session's `CreateMutexW` opens or creates a mutex
    /// object in ITS OWN session namespace, so from a second session's point of view there
    /// was no lock in play at all - contending with your own session's other threads,
    /// never with the other session's. Two real logon sessions can't be created in a test
    /// (see the finding's Acceptance note), so this pins the exact interleaving a missing
    /// cross-session lock permits directly, in program order rather than as a timing
    /// gamble: both "sessions" read the SAME starting state before either writes, so the
    /// second write has no idea about the first's change and destroys it.
    ///
    /// This is what makes the fix's teeth visible: replace the two direct
    /// `read_history`/`write_history` calls below with two calls to `update_history_at`
    /// sharing one lock path, and this exact interleaving becomes impossible - the second
    /// reader cannot observe the pre-first-write state, because the first writer's whole
    /// read-modify-write section (including the write) completes, under the lock, before
    /// the second's read-modify-write section is even allowed to start.
    #[test]
    fn without_a_shared_lock_two_racing_writers_can_lose_an_update() {
        let dir = temp_dir("racy_no_lock");
        let path = dir.join("license-history.json");
        assert!(write_history(&path, &History::default()));

        // "Session A" reads first...
        let mut a = read_history(&path).expect("seed must parse");
        a.nag_count = 1;
        // ...then "session B" reads the SAME pre-A-write state: no shared lock stops it,
        // exactly as two different sessions' own separate mutexes would not stop it.
        let mut b = read_history(&path).expect("seed must parse");
        b.last_status = "session-b".to_string();
        // B writes first...
        assert!(write_history(&path, &b));
        // ...and A's write, computed from data that predates B's, silently destroys it.
        assert!(write_history(&path, &a));

        let result = read_history(&path).expect("breadcrumb must still parse");
        assert_eq!(result.nag_count, 1, "A's change survives - it wrote last");
        assert_eq!(
            result.last_status, "",
            "B's change was silently lost: this is the F18 bug, reproduced without a \
             shared lock standing between the two writers"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lock that cannot be taken must not fall back to writing unlocked - that fallback
    /// is exactly the old bug's other half (see the module docs on `HistoryLock`). Hold
    /// the lock file open in this thread for the whole test, so every retry
    /// [`update_history_at`] makes is guaranteed to fail, then assert it gives up having
    /// written nothing: the breadcrumb on disk is still byte-for-byte the seed, never the
    /// mutation the caller asked for.
    #[test]
    fn a_lock_that_times_out_writes_nothing_rather_than_clobbering_newer_history() {
        let dir = temp_dir("lock_timeout");
        let path = dir.join("license-history.json");
        let seed = History {
            was_business: true,
            last_status: "active".to_string(),
            nag_count: 3,
            ..History::default()
        };
        assert!(write_history(&path, &seed));

        // Hold the lock ourselves for the whole test - every attempt inside
        // `update_history_at` below must see it already taken.
        let held = HistoryLock::acquire(&lock_path(&path));
        assert!(
            held.is_some(),
            "the test's own lock acquisition must succeed"
        );

        let wrote = update_history_at(&path, |h| {
            h.nag_count = 999;
            h.last_status = "this must never reach disk".to_string();
        });
        assert!(!wrote, "a timed-out lock must report no write happened");

        drop(held);

        let result = read_history(&path).expect("breadcrumb must still parse");
        assert_eq!(
            result, seed,
            "the file must be untouched: a lock timeout must never silently replace \
             history with a write that never actually held the lock"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
