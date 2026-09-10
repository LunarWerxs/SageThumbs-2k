//! "Check for updates" — ask the GitHub releases API for the latest tag and compare it
//! to the running build. Reuses the sponsor fetch (WinINet HTTPS, bounded timeout, and
//! the `SageThumbs2K` User-Agent the GitHub API requires). Best-effort: any failure
//! (offline, repo renamed/moved, no releases yet, rate-limited) becomes `Failed`, so the
//! UI can fall back to "couldn't reach the update server — check GitHub manually."

use std::io::{Read, Write};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, HWND};
use windows::Win32::Storage::FileSystem::FILE_SHARE_READ;
use windows::Win32::System::SystemInformation::{
    GetNativeSystemInfo, PROCESSOR_ARCHITECTURE, PROCESSOR_ARCHITECTURE_AMD64,
    PROCESSOR_ARCHITECTURE_ARM64, SYSTEM_INFO,
};

use ed25519_dalek::{Signature, Verifier, VerifyingKey};

use crate::sponsors::{http_fetch, http_fetch_capped, os_tag, BANNER_URL};

/// The GitHub "latest release" endpoint for this repo.
const RELEASES_API: &str = "https://api.github.com/repos/LunarWerxs/SageThumbs-2k/releases/latest";

/// Where the user is pointed to check / download by hand (also the README badge target).
pub(crate) const RELEASES_URL: &str = "https://github.com/LunarWerxs/SageThumbs-2k/releases";

/// Settings-panel custom message (`WM_APP + 8`; `WM_APP_SPONSORS` is `+7`): the lazy
/// background check found a newer release. Posted from the worker; the dialog turns the
/// "Check for updates" button into a quiet nudge. Carries a `Box<String>` (the tag) in
/// `LPARAM` — the handler reclaims it.
pub(crate) const WM_APP_UPDATE: u32 = 0x8000 + 8;

/// Don't hit the network more than once per this interval — a previous result (cached on
/// disk) answers in between, so opening Settings repeatedly never hammers GitHub.
const CHECK_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);

pub(crate) enum UpdateCheck {
    /// Running the newest published release (or newer, e.g. a dev build).
    UpToDate,
    /// A newer release exists; carries everything known about it (tag, publication date,
    /// whether it is a security release) so the caller can decide whether this machine's
    /// updates window covers it - see [`update_offer`].
    Available(LatestRelease),
    /// Couldn't reach / parse the update server — tell the user to check manually.
    Failed,
}

/// Parse a version string ("v0.4.6", "0.4.6", "0.4.6-rc1") into `(major, minor, patch)`.
/// Tolerant: a missing minor/patch is 0; a pre-release/build suffix is dropped.
fn parse_ver(s: &str) -> Option<(u32, u32, u32)> {
    let core = s.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next().unwrap_or(core); // strip -rc1 / +build
    let mut it = core.split('.');
    let maj = it.next()?.parse::<u32>().ok()?;
    let min = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    let pat = it.next().and_then(|p| p.parse().ok()).unwrap_or(0);
    Some((maj, min, pat))
}

/// Synchronously query GitHub for the latest release and compare to this build. Bounded
/// by the fetch's own per-phase timeout, so a dead network returns `Failed` quickly.
pub(crate) fn check() -> UpdateCheck {
    let Some(bytes) = http_fetch(RELEASES_API, true) else {
        return UpdateCheck::Failed;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return UpdateCheck::Failed;
    };
    // No "tag_name" → an error body (404 when there are no releases, a rate-limit notice,
    // etc.) → treat as unreachable so the UI offers the manual fallback.
    let Some(tag) = json.get("tag_name").and_then(|v| v.as_str()) else {
        return UpdateCheck::Failed;
    };
    match (parse_ver(tag), parse_ver(env!("CARGO_PKG_VERSION"))) {
        (Some(latest), Some(current)) if latest > current => {
            // The same two extra facts the Worker manifest carries, read straight off
            // GitHub's own release object on this fallback path, so a machine that reaches
            // GitHub but not the Worker still gets an honest window decision instead of a
            // date-less "offer it to everyone".
            UpdateCheck::Available(LatestRelease {
                tag: tag.trim_start_matches(['v', 'V']).to_string(),
                published_unix: json
                    .get("published_at")
                    .and_then(|v| v.as_str())
                    .and_then(crate::license::parse_iso_unix),
                security: json
                    .get("body")
                    .and_then(|v| v.as_str())
                    .is_some_and(is_security_body),
            })
        }
        (Some(_), Some(_)) => UpdateCheck::UpToDate,
        _ => UpdateCheck::Failed, // unparseable tag — don't guess
    }
}

/// The literal marker a security release puts in its notes, matched case-insensitively as
/// PLAIN TEXT (never a regex), so the notes are free to wrap it in any formatting. Kept in
/// step with the Worker's own `SECURITY_RELEASE_MARKER`; documented for release authors in
/// `docs/RELEASE-SECURITY.md`.
const SECURITY_RELEASE_MARKER: &str = "[security-release]";

/// Do these release notes mark a security release?
fn is_security_body(body: &str) -> bool {
    body.to_ascii_lowercase().contains(SECURITY_RELEASE_MARKER)
}

/// The three facts the manifest carries about the newest published release. Bundled so the
/// throttle cache, the worker fetch and the offer decision all move one value instead of
/// three loose parameters that could be paired up wrongly.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LatestRelease {
    /// The display tag, e.g. "3.0.1".
    pub tag: String,
    /// When GitHub published it, in Unix seconds. `None` when the manifest did not say -
    /// an older Worker, or a GitHub hiccup - which reads as "no publication date on record"
    /// and leaves the build offered to everyone (see [`update_offer`]).
    pub published_unix: Option<u64>,
    /// The release notes carried the `[security-release]` marker. A security release is
    /// offered to every licensed installation regardless of its updates window; see
    /// `docs/RELEASE-SECURITY.md`.
    pub security: bool,
}

impl LatestRelease {
    /// A tag with nothing else known - what the direct-GitHub fallback [`check`] and every
    /// pre-2026-09-10 cache file can say. Deliberately the "offer it to everyone" shape.
    fn bare(tag: String) -> Self {
        Self {
            tag,
            published_unix: None,
            security: false,
        }
    }

    /// A release we know nothing about at all, not even its tag - the About card's
    /// "the post carried no payload" fallback. Same "offer it" shape as [`bare`].
    pub(crate) fn unknown() -> Self {
        Self::bare(String::new())
    }
}

/// What to do about a release that is newer than the running build.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Offer {
    /// Offer the install, exactly as this app always has.
    Install,
    /// The build was published after this machine's 12 months of updates ended: say so,
    /// offer the renewal, and do NOT install it. `ends_unix` is when the window closed, for
    /// the message to name.
    OutsideWindow { ends_unix: u64 },
    /// Nothing to offer - no newer release is known.
    None,
}

/// THE decision: given what this machine's licence says and what the newest release is,
/// should the app offer to install it?
///
/// Pure, and every branch is pinned by a test below. The rules, in the order they are
/// applied and with the reason each one exists:
///
/// 1. **No updates window on record → Install.** This is a personal-use copy, an unredeemed
///    one, or a licence whose updates never lapse. It is also every machine that has not
///    yet heard from the relay, which is why "unknown" must never read as "closed".
/// 2. **A security release → Install.** Bought within the window or not, a licensed
///    installation gets security fixes. This outranks the window on purpose and is the one
///    rule that must survive any future edit here.
/// 3. **No publication date on record → Install.** We cannot honestly say a build is
///    outside a window we cannot place it against, so we do not.
/// 4. **Published after the window closed → `OutsideWindow`.** Offer the renewal instead.
///    Note the comparison is `>`, so a build published exactly at the boundary is inside -
///    the same inclusive rule `licence_cert::verify` uses for `build_date <= maint`.
/// 5. Otherwise → **Install**.
///
/// ⛔ NOTHING HERE EVER STOPS THE APP. The worst outcome this function can produce is that a
/// new build is not offered; the installed version keeps working, with every feature, for as
/// long as the customer likes. That is what "perpetual licence" means and it is the whole
/// reason this decision lives apart from `license::Entitlement`.
pub(crate) fn update_offer(
    snap: &crate::license::LicenceSnapshot,
    latest_published_unix: Option<u64>,
    latest_security: bool,
) -> Offer {
    let Some(ends_unix) = snap.maint_unix else {
        return Offer::Install;
    };
    if latest_security {
        return Offer::Install;
    }
    let Some(published) = latest_published_unix else {
        return Offer::Install;
    };
    if published > ends_unix {
        Offer::OutsideWindow { ends_unix }
    } else {
        Offer::Install
    }
}

/// [`update_offer`] over an optional release: `None` in, [`Offer::None`] out. The shape every
/// caller actually has, since the throttled check answers `Option<LatestRelease>`.
pub(crate) fn offer_for(
    snap: &crate::license::LicenceSnapshot,
    latest: Option<&LatestRelease>,
) -> Offer {
    match latest {
        Some(l) => update_offer(snap, l.published_unix, l.security),
        None => Offer::None,
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The tiny throttle/cache file ("`<unix_secs>\n<latest_tag>\n`"). Beside the portable ini
/// when running portable (issue #118/G118 — a portable copy must not leave anything in the
/// host's `%LOCALAPPDATA%`, the same split `settings.rs`/`sync_client.rs` already apply to
/// every other setting); next to the diagnostics log in `%LOCALAPPDATA%` otherwise.
fn cache_path() -> Option<PathBuf> {
    if let Some(ini) = sagethumbs2k_core::settings::ini_path() {
        return ini.parent().map(|d| d.join("SageThumbs2K-update.txt"));
    }
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("SageThumbs2K-update.txt"))
}

/// Read the throttle/cache file. Lines 3 and 4 (publication date, security flag) were added
/// 2026-09-10 and are OPTIONAL: a cache written by an older build is two lines and parses
/// into a [`LatestRelease::bare`], which is the "offer it to everyone" shape - so upgrading
/// never makes the app briefly refuse a build over a date it simply has not fetched yet.
fn read_cache() -> Option<(u64, LatestRelease)> {
    parse_cache(&std::fs::read_to_string(cache_path()?).ok()?)
}

/// The pure half of [`read_cache`], so every shape of cache file - including the two-line one
/// every pre-2026-09-10 build wrote - can be pinned by a test without touching the disk.
fn parse_cache(text: &str) -> Option<(u64, LatestRelease)> {
    let mut lines = text.lines();
    let secs = lines.next()?.trim().parse::<u64>().ok()?;
    let tag = lines.next()?.trim().to_string();
    if tag.is_empty() {
        return None;
    }
    // `0` is the on-disk spelling of "not known", the same convention the licence
    // breadcrumb's own `maint_unix` uses.
    let published_unix = lines
        .next()
        .and_then(|l| l.trim().parse::<u64>().ok())
        .filter(|&p| p != 0);
    let security = lines.next().is_some_and(|l| l.trim() == "1");
    Some((
        secs,
        LatestRelease {
            tag,
            published_unix,
            security,
        },
    ))
}

fn write_cache(secs: u64, latest: &LatestRelease) {
    if let Some(p) = cache_path() {
        let _ = std::fs::write(
            p,
            format!(
                "{secs}\n{}\n{}\n{}\n",
                latest.tag,
                latest.published_unix.unwrap_or(0),
                u8::from(latest.security)
            ),
        );
    }
}

/// Is `tag` strictly newer than the running build?
fn is_newer(tag: &str) -> bool {
    matches!(
        (parse_ver(tag), parse_ver(env!("CARGO_PKG_VERSION"))),
        (Some(latest), Some(current)) if latest > current
    )
}

/// Kick off a LAZY, THROTTLED, background update check. Runs entirely on a worker thread
/// (never blocks the Settings window opening), hits the network at most once per
/// [`CHECK_INTERVAL`] — answering from the on-disk cache in between — and is SILENT unless
/// a newer version is known, in which case it calls `on_newer(tag)` from the worker thread
/// (the caller marshals to the UI, e.g. via `PostMessage`). Up-to-date / offline never nag.
pub(crate) fn lazy_check<F: FnOnce(String) + Send + 'static>(on_newer: F) {
    std::thread::spawn(move || {
        let now = now_secs();
        // Within the interval: answer from the cache (no network), but still nudge about a
        // previously-found update so the user isn't left unaware between checks.
        if let Some((last, latest)) = read_cache() {
            if now.saturating_sub(last) < CHECK_INTERVAL.as_secs() {
                if is_newer(&latest.tag) {
                    on_newer(latest.tag);
                }
                return;
            }
        }
        // Stale or first run: one real check. Cache a definitive result (up-to-date or a
        // newer tag) so we don't re-hit for a day; on a transient failure leave the cache
        // untouched so the NEXT Settings open retries instead of waiting out the interval.
        match check() {
            UpdateCheck::Available(latest) => {
                write_cache(now, &latest);
                on_newer(latest.tag);
            }
            UpdateCheck::UpToDate => write_cache(
                now,
                &LatestRelease::bare(env!("CARGO_PKG_VERSION").to_string()),
            ),
            UpdateCheck::Failed => {}
        }
    });
}

/// Ask the sponsor Worker for the latest release. The Worker already serves `latest`,
/// `latestPublishedAt` and `latestSecurity` in its manifest (sourced from GitHub
/// server-side + edge-cached), so the client never touches GitHub directly and can't be
/// rate-limited. Reuses the startup manifest request with new=0. `None` on any failure.
///
/// Only `latest` is required. A Worker that has not been redeployed with the 2026-09-10
/// fields answers the other two as absent, which [`update_offer`] reads as "offer it" -
/// the pre-existing behaviour, never a refusal.
fn latest_from_worker() -> Option<LatestRelease> {
    // Tag the request with &dev=1 on a developer test box (see `is_dev_machine`).
    let dev = if sagethumbs2k_core::settings::is_dev_machine() {
        "&dev=1"
    } else {
        ""
    };
    let url = format!(
        "{BANNER_URL}?v={}&os={}&new=0{dev}",
        env!("CARGO_PKG_VERSION"),
        os_tag()
    );
    let bytes = http_fetch(&url, true)?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let tag = json.get("latest")?.as_str()?.trim();
    if tag.is_empty() {
        return None;
    }
    Some(LatestRelease {
        tag: tag.to_string(),
        published_unix: json
            .get("latestPublishedAt")
            .and_then(|v| v.as_str())
            .and_then(crate::license::parse_iso_unix),
        security: json
            .get("latestSecurity")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
    })
}

/// Has the network-check throttle expired? A cheap disk read, no network — the guard the
/// piggyback launcher ([`spawn_due_check`]) uses so an ordinary app launch costs one file
/// read on all but the first launch of the day.
fn check_due() -> bool {
    match read_cache() {
        Some((last, _)) => now_secs().saturating_sub(last) >= CHECK_INTERVAL.as_secs(),
        None => true,
    }
}

/// THROTTLED update check routed through the sponsor Worker, run SYNCHRONOUSLY on the
/// calling thread. Hits the network at most once per [`CHECK_INTERVAL`] and — unlike
/// [`lazy_check`] — does NOT re-nudge from the cache in between, so a newer version is
/// reported at most once per interval instead of on every tick. Falls back to the direct
/// GitHub [`check`] if the Worker didn't supply a version. `Some(tag)` = newer release.
fn check_throttled() -> Option<LatestRelease> {
    let now = now_secs();
    if !check_due() {
        return None; // checked recently — don't re-report within the interval
    }
    // Worker first; GitHub as a fallback.
    match latest_from_worker() {
        Some(latest) => {
            write_cache(now, &latest); // cache whatever the latest is (newer or not)
            is_newer(&latest.tag).then_some(latest)
        }
        None => match check() {
            UpdateCheck::Available(latest) => {
                write_cache(now, &latest);
                Some(latest)
            }
            UpdateCheck::UpToDate => {
                write_cache(
                    now,
                    &LatestRelease::bare(env!("CARGO_PKG_VERSION").to_string()),
                );
                None
            }
            UpdateCheck::Failed => None,
        },
    }
}

/// [`check_throttled`] on a background thread — the resident screenshot helper's timer path.
pub(crate) fn lazy_check_worker<F: FnOnce(String) + Send + 'static>(on_newer: F) {
    std::thread::spawn(move || {
        let latest = check_throttled();
        // The licence entitlement re-check rides this same worker thread and cadence rather
        // than getting a timer, task, or setting of its own: this is the one place the daily
        // update check actually does its network work off the UI thread on a long-lived
        // process (the piggyback launcher's `--update-check` one-shot runs synchronously and
        // exits immediately, so it stays excluded — see `spawn_due_check` / `main.rs`).
        // `refresh_entitlement` throttles its own network hit internally (~6 h) and is a
        // no-op for a machine with no reason to care about a business seat, so calling it
        // every time this worker fires — daemon startup, its periodic re-arm, or whichever
        // ordinary launch next finds the daily cache stale — is correct. Per its own
        // contract it fails open and returns `None` on any error (offline, rejected,
        // unparsable) without ever surfacing anything to the user, so the result is
        // discarded here exactly like a skipped (not-due) check — nothing to log or react to.
        let _ = crate::license::refresh_entitlement();
        if let Some(latest) = latest {
            on_newer(latest.tag);
        }
    });
}

// ---- Reaching users with no resident helper -------------------------------------------
//
// The resident screenshot/tray helper is OPT-IN, so for most installs it never runs and its
// 6 h update timer never exists. Two paths below cover everyone else, and neither adds a
// resident process:
//
//   * `--update-check` (`run_one_shot_check`) — a one-shot that does the same throttled
//     check, toasts if newer, and exits. Driven by a per-user Scheduled Task registered at
//     install time (see `install_update_task`), which is what makes the check periodic on a
//     machine where nothing of ours is running.
//   * `spawn_due_check` — fired from any ordinary app launch (context-menu verb, Convert
//     dialog, Quick preview, Settings). Costs one cached file read when the throttle hasn't
//     expired; otherwise it spawns the SAME one-shot detached, so the calling process never
//     waits on the network and never has to outlive the toast. This is the backstop for
//     machines where the Scheduled Task couldn't be created (locked-down policy).

/// The per-user Scheduled Task that keeps update checks alive with no resident process.
const UPDATE_TASK: &str = "SageThumbs2K_UpdateCheck";

/// Run `schtasks.exe` with no console flash, returning its output.
fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("schtasks.exe")
        .args(args)
        .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
        .output()
}

/// Register (or refresh) the per-user update-check task: `SageThumbs2K.exe --update-check`,
/// daily with a 6 h repetition, at the user's NORMAL token (`/rl LIMITED` — the check writes
/// only `%LOCALAPPDATA%` and pops a tray balloon; it never needs admin). The 6 h cadence
/// mirrors what the resident helper does; the actual network hit stays throttled to once a
/// day inside [`check_throttled`], so the extra ticks only cover machines that were asleep.
/// Returns false if `schtasks` refused (policy, missing binary) — the piggyback path then
/// carries the feature on its own. Best-effort with logging; never fatal.
pub(crate) fn install_update_task() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let tr = format!("\"{}\" --update-check", exe.display());
    #[rustfmt::skip]
    let created = schtasks(&["/create", "/f", "/tn", UPDATE_TASK, "/sc", "DAILY", "/st", "09:00",
                             "/ri", "360", "/du", "9999:59", "/rl", "LIMITED", "/tr", &tr]);
    match created {
        Ok(o) if o.status.success() => true,
        Ok(o) => {
            sagethumbs2k_core::safety::log(&format!(
                "update: schtasks create failed ({}): {}",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ));
            false
        }
        Err(e) => {
            sagethumbs2k_core::safety::log(&format!("update: schtasks unavailable ({e})"));
            false
        }
    }
}

/// Drop the update-check task (the user turned auto-check off, or we're uninstalling).
/// A missing task is not an error.
pub(crate) fn remove_update_task() {
    let _ = schtasks(&["/delete", "/f", "/tn", UPDATE_TASK]);
}

/// Make the Scheduled Task match the "Automatically check for updates" setting. Called
/// after every install and whenever the Settings checkbox is applied, so turning the
/// setting off genuinely removes the task instead of leaving an inert one behind.
pub(crate) fn sync_update_task() {
    if sagethumbs2k_core::settings::update_auto_check() {
        install_update_task();
    } else {
        remove_update_task();
    }
}

/// `--update-check`: the one-shot the Scheduled Task (and [`spawn_due_check`]) runs. Honors
/// the user's auto-check setting, does one throttled check, and pops a non-blocking tray
/// balloon if a newer release exists. Silent when up to date, offline, or throttled.
pub(crate) fn run_one_shot_check() {
    if !sagethumbs2k_core::settings::update_auto_check() {
        return;
    }
    let Some(latest) = check_throttled() else {
        return;
    };
    // The window decision is made HERE as well as in the About card, because this one-shot
    // is the only thing many installs ever run - the tray balloon must not promise an
    // install this machine's licence will then decline to perform.
    let snap = crate::license::snapshot();
    let body = match offer_for(&snap, Some(&latest)) {
        Offer::OutsideWindow { ends_unix } => crate::win::t("upd_outside_toast")
            .replace("{ver}", &latest.tag)
            .replace("{date}", &crate::settings_dlg::format_unix_date(ends_unix)),
        // `None` is unreachable with a `Some(..)` release, and treating it like Install is
        // the same "say nothing surprising" direction the rest of this module takes.
        Offer::Install | Offer::None => {
            crate::win::t("upd_toast_body").replace("{ver}", &latest.tag)
        }
    };
    unsafe {
        crate::win::notify_toast(
            crate::win::t("upd_toast_title"),
            &body,
            Duration::from_secs(8),
        );
    }
}

/// Piggyback the update check on an ordinary app launch: if the once-a-day throttle has
/// expired, spawn the detached `--update-check` one-shot and return immediately. The caller
/// does no network work and can exit whenever it likes — the toast belongs to the child.
pub(crate) fn spawn_due_check() {
    if !sagethumbs2k_core::settings::update_auto_check() || !check_due() {
        return;
    }
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = std::process::Command::new(exe)
        .arg("--update-check")
        .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
        .spawn();
}

// ---- One-click self-update (download → verify → silent install) ----------------------

/// Generous cap for the downloaded installer (the real setup is ~9–15 MB; this is a
/// hostile-input bound — an over-cap response is treated as a failed download, never run).
const MAX_INSTALLER_BYTES: usize = 128 * 1024 * 1024;

/// Receive window (seconds) for the installer download — far longer than the manifest's 5 s
/// since this pulls multiple MB over whatever connection the user has.
const DOWNLOAD_TIMEOUT_SECS: u64 = 120;

/// The detached signature file is 128 hex characters plus a little slack for whitespace -
/// this cap is generous by three orders of magnitude, purely to bound a hostile response.
const MAX_SIG_BYTES: usize = 4096;

/// Receive window (seconds) for the `.sig` fetch - it is a few hundred bytes, so this stays
/// tight rather than reusing the multi-MB installer's [`DOWNLOAD_TIMEOUT_SECS`].
const SIG_TIMEOUT_SECS: u64 = 15;

/// The public half of the ed25519 key that signs every release artifact
/// (`examples/update-sign.rs` holds the private half; `examples/update-keygen.rs` mints the
/// pair). Every release asset's bytes must carry a valid detached signature against this key
/// before the installer is ever launched - see [`verify_signature`] and [`download_and_install`].
///
/// PLACEHOLDER: all zeros until the integrator runs `examples/update-keygen.rs` and pastes its
/// printed array literal here. An all-zero key is not a parse error for `ed25519-dalek` - it
/// decodes to a (weak, useless) point on the curve - so nothing verifies against it, and every
/// self-update correctly refuses rather than silently accepting an unsigned release. The
/// `the_compiled_in_key_is_not_the_placeholder` test below fails until this is a real key; that
/// is the point of the test, not a bug in it.
pub const UPDATE_PUBLIC_KEY: [u8; 32] = [
    0x16, 0x9f, 0xce, 0x0a, 0xde, 0x4a, 0xec, 0xed, 0x2d, 0xcb, 0x36, 0xa3, 0x76, 0xc1, 0x27, 0x74,
    0x38, 0x44, 0x77, 0x91, 0x84, 0xf5, 0x10, 0x9b, 0xc3, 0x8c, 0x00, 0x60, 0x3f, 0xa8, 0x32, 0x5b,
];

/// Switches handed to the freshly-downloaded Inno setup for an unattended in-place upgrade.
/// `/SILENT` = bare progress bar, no wizard; `/SUPPRESSMSGBOXES` + `/FORCECLOSEAPPLICATIONS`
/// let it close+restart Explorer to swap the in-use DLL without prompting; `/NORESTART`
/// blocks a reboot prompt; `/UPDATED` is OUR marker the installer keys the post-update
/// "you're now on <ver>" relaunch off (see installer.iss `WasSelfUpdate`).
const INSTALL_FLAGS: &str = "/SILENT /SUPPRESSMSGBOXES /NORESTART /FORCECLOSEAPPLICATIONS /UPDATED";

/// One published installer asset: where to fetch it, its exact byte size, and (when GitHub
/// supplies it) the sha256 digest we verify the bytes against before running it elevated.
struct InstallerAsset {
    url: String,
    size: u64,
    sha256: String, // lowercase hex, no "sha256:" prefix
    /// The `browser_download_url` of the sibling `<installer-name>.sig` asset, when the
    /// release published one. `None` means the release is unsigned - [`download_and_install`]
    /// refuses to launch in that case rather than skipping the check.
    sig_url: Option<String>,
}

/// Pull the Windows installer asset out of GitHub's latest-release JSON — the exact versioned
/// setup executable — returning its tag + download URL + size + sha256, or None on
/// any failure (offline, no release, no matching asset).
fn latest_installer_asset() -> Option<(String, InstallerAsset)> {
    let bytes = http_fetch(RELEASES_API, true)?;
    installer_asset_from_json(&serde_json::from_slice(&bytes).ok()?)
}

/// Pure parse of GitHub's latest-release JSON → (tag, installer asset). Split from the fetch
/// so it can be unit-tested against a real release body with no network.
fn installer_asset_from_json(json: &serde_json::Value) -> Option<(String, InstallerAsset)> {
    installer_asset_from_json_for_arch(json, native_installer_arch())
}

/// Choose the installer for the native Windows architecture, not merely this process.
/// That distinction matters on ARM64: an older x64 SageThumbs build can run under
/// emulation, but native Explorer needs the ARM64 shell extension after the update.
fn native_installer_arch() -> &'static str {
    let mut info = SYSTEM_INFO::default();
    unsafe {
        GetNativeSystemInfo(&mut info);
        installer_arch_for_native(
            info.Anonymous.Anonymous.wProcessorArchitecture,
            std::env::consts::ARCH,
        )
    }
}

/// Pure half of [`native_installer_arch`] so the x64-on-ARM64 migration rule is
/// covered on any CI host.
fn installer_arch_for_native(
    native_arch: PROCESSOR_ARCHITECTURE,
    process_arch: &'static str,
) -> &'static str {
    match native_arch {
        PROCESSOR_ARCHITECTURE_ARM64 => "aarch64",
        PROCESSOR_ARCHITECTURE_AMD64 => "x86_64",
        _ => process_arch,
    }
}

/// Architecture-aware half of [`installer_asset_from_json`]. Keeping the target explicit
/// makes the release-asset contract testable on either development architecture: x64 gets
/// the established setup name, while ARM64 must never download that x64 installer.
fn installer_asset_from_json_for_arch(
    json: &serde_json::Value,
    arch: &str,
) -> Option<(String, InstallerAsset)> {
    let raw_tag = json.get("tag_name")?.as_str()?;
    let (major, minor, patch) = parse_ver(raw_tag)?;
    let tag = format!("{major}.{minor}.{patch}");
    let expected_name = match arch {
        "x86_64" => format!("SageThumbs2K-Setup-{tag}.exe"),
        "aarch64" => format!("SageThumbs2K-Setup-{tag}-arm64.exe"),
        _ => return None, // no published self-update installer for this architecture
    };
    let asset = json.get("assets")?.as_array()?.iter().find(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(&expected_name))
    })?;
    let url = asset.get("browser_download_url")?.as_str()?.to_string();
    let (host, path) = crate::http::split_https(&url)?;
    if host != "github.com" || !path.starts_with("/LunarWerxs/SageThumbs-2k/releases/download/") {
        return None;
    }
    let size = asset
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let sha256 = asset
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase)
        .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))?;

    // The detached signature ships as a SEPARATE release asset named "<installer-name>.sig"
    // beside the installer, not a field on the installer's own JSON - `release.ps1` uploads it
    // that way so the existing digest-verification loop (step 5) covers it for free. Its
    // absence is not a lookup failure: an unsigned release is a real (refused) state, not a
    // malformed one, so this stays `Option` rather than folding into the `?` chain above.
    let sig_name = format!("{expected_name}.sig");
    let sig_url = json.get("assets")?.as_array()?.iter().find_map(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .filter(|n| n.eq_ignore_ascii_case(&sig_name))
            .and_then(|_| a.get("browser_download_url"))
            .and_then(|u| u.as_str())
            .map(str::to_string)
    });

    Some((
        tag,
        InstallerAsset {
            url,
            size,
            sha256,
            sig_url,
        },
    ))
}

/// SHA-256 of `data` as lowercase hex, via Windows CNG (no extra crate). None on failure.
fn sha256_hex(data: &[u8]) -> Option<String> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    status
        .is_ok()
        .then(|| out.iter().map(|b| format!("{b:02x}")).collect())
}

/// Parse 128 lowercase-or-uppercase hex characters into a raw 64-byte ed25519 signature.
/// `None` on anything else - wrong length, non-ASCII, non-hex - worked byte-wise so a
/// downloaded `.sig` file can never panic this on a bad char boundary.
fn parse_sig_hex(sig_hex: &str) -> Option<[u8; 64]> {
    let bytes = sig_hex.as_bytes();
    if bytes.len() != 128 || !bytes.is_ascii() {
        return None;
    }
    let mut out = [0u8; 64];
    for i in 0..64 {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Verify a detached ed25519 signature over `bytes`. `sig_hex` is the `.sig` asset's raw
/// content - 128 hex characters, no framing. Any parse failure (bad length, non-hex, a `key`
/// that doesn't decode to a curve point) is simply `false`, same as a bad signature: there is
/// no distinguishable "malformed" outcome for the caller to accidentally treat as anything
/// other than "not verified".
fn verify_signature(key: &[u8; 32], bytes: &[u8], sig_hex: &str) -> bool {
    let Some(sig_bytes) = parse_sig_hex(sig_hex) else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(key) else {
        return false;
    };
    verifying_key
        .verify(bytes, &Signature::from_bytes(&sig_bytes))
        .is_ok()
}

/// Validate downloaded installer bytes before we ever run them elevated: a real PE, the
/// exact advertised size, and (when GitHub supplied a digest) a matching sha256. False =
/// refuse — we'd rather fall back to the manual page than run an unverified installer. We
/// write the bytes ourselves (no Mark-of-the-Web), so the silent launch won't trip SmartScreen.
fn verify_installer_bytes(bytes: &[u8], asset: &InstallerAsset) -> bool {
    if bytes.len() < 2 || &bytes[..2] != b"MZ" {
        return false; // not a Windows executable
    }
    if asset.size != 0 && bytes.len() as u64 != asset.size {
        return false; // truncated / wrong length
    }
    if sha256_hex(bytes).as_deref() != Some(asset.sha256.as_str()) {
        return false; // integrity check failed
    }
    true
}

/// Atomically create the downloaded installer, then hold a READ-ONLY handle that permits
/// readers but denies other writers and deleters. Holding that handle through
/// `ShellExecuteW("runas")` closes the pathname replacement window between the final hash
/// check and the elevated process opening the image — and the final verification is read
/// back THROUGH that handle, so the bytes we bless are the bytes it is protecting.
///
/// THE LOCK MUST NOT CARRY WRITE ACCESS. Windows maps an executable image by opening the
/// file with `FILE_SHARE_READ | FILE_SHARE_DELETE`, and that share mode cannot coexist with
/// an existing writer — so a read+WRITE lock makes the launch itself fail with
/// `ERROR_SHARING_VIOLATION`, which `ShellExecuteW` reports as `SE_ERR_SHARE` (26). That is
/// precisely what shipped in 1.3.3: one-click self-update failed on EVERY machine, every
/// time, and the failure text blamed the user for not being an administrator. A read-only
/// lock denies writers and deleters exactly as well (both tested below) while leaving the
/// image mappable.
fn write_locked_installer(
    tag: &str,
    bytes: &[u8],
    asset: &InstallerAsset,
) -> Result<(PathBuf, std::fs::File), &'static str> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for attempt in 0..16u8 {
        let path = std::env::temp_dir().join(format!(
            "SageThumbs2K-Setup-{tag}-{}-{nonce}-{attempt}.exe",
            std::process::id()
        ));
        // Share NOTHING while the bytes are going down: nobody may even read a half-written
        // setup, let alone race the write.
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(&path);
        let mut file = match opened {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("couldn't save the installer"),
        };
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file); // the write handle is gone before anything tries to run the image
        if written.is_err() {
            let _ = std::fs::remove_file(&path);
            return Err("couldn't save the installer");
        }
        // Re-open read-only and verify through THIS handle — the one held across the launch.
        let Ok(mut file) = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&path)
        else {
            let _ = std::fs::remove_file(&path);
            return Err("couldn't lock the saved installer");
        };
        let mut on_disk = Vec::with_capacity(bytes.len());
        if file.read_to_end(&mut on_disk).is_err() || !verify_installer_bytes(&on_disk, asset) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err("the saved installer failed re-verification");
        }
        return Ok((path, file));
    }
    Err("couldn't reserve a temporary installer path")
}

/// Why a one-click update didn't complete. This used to be a bare `String` that every
/// failure — user cancel, antivirus block, group policy, a dead network — collapsed into
/// "the update was cancelled at the Windows permission prompt", which the caller then
/// swallowed on the word "cancel". A user whose antivirus ate the installer saw NOTHING.
/// Keep these cases distinct: only [`UpdateError::Cancelled`] may be silent.
pub(crate) enum UpdateError {
    /// The user backed out themselves (progress-dialog Cancel, or declining the Windows
    /// permission prompt). The one case that must not nag.
    Cancelled,
    /// Something outside the user's immediate control refused to run the verified installer
    /// — antivirus, Smart App Control, or an administrator policy. Carries the explanation.
    Blocked(String),
    /// Everything else: offline, no matching release asset, a failed integrity check.
    Failed(String),
    /// The release's detached signature was missing, unreadable, or did not verify against
    /// [`UPDATE_PUBLIC_KEY`]. Distinct from [`UpdateError::Failed`] because this is a trust
    /// refusal, not an environment problem - the size + sha256 checks can pass while this
    /// still refuses, exactly when the trust anchor they both come from (the same GitHub API
    /// response) is the thing being spoofed.
    Unverified(String),
}

impl UpdateError {
    /// The user-facing sentence for this failure ("" for a plain cancel, which says nothing).
    pub(crate) fn message(&self) -> &str {
        match self {
            UpdateError::Cancelled => "",
            UpdateError::Blocked(m) | UpdateError::Failed(m) | UpdateError::Unverified(m) => m,
        }
    }
}

/// The refusal used for every signature-trust failure - missing `.sig` asset, an unreadable
/// download, or a signature that doesn't verify. One message for all three: the caller can't
/// usefully act differently on any of them, and naming a moving GitHub-hosted trust anchor as
/// the culprit is more honest than trying to distinguish "attacker" from "network hiccup".
fn unverified_release_error() -> UpdateError {
    UpdateError::Unverified(format!(
        "This update's signature couldn't be verified, so SageThumbs 2K didn't install it. \
         Download it by hand from {RELEASES_URL} instead."
    ))
}

/// Is Smart App Control on and ENFORCING? SAC blocks unsigned executables outright and is
/// default-on for clean Windows 11 installs, so it is the likeliest silent killer of a
/// downloaded, unsigned setup. `VerifiedAndReputablePolicyState`: 0 = off, 1 = enforcement,
/// 2 = evaluation (audits, doesn't block). Read-only; absent key = not enforcing.
fn smart_app_control_enforcing() -> bool {
    windows_registry::LOCAL_MACHINE
        .open(r"SYSTEM\CurrentControlSet\Control\CI\Policy")
        .and_then(|k| k.get_u32("VerifiedAndReputablePolicyState"))
        .is_ok_and(|v| v == 1)
}

/// Turn a failed `ShellExecuteW` into an honest, distinguishable reason. Pure so the whole
/// mapping is unit-testable without a UAC prompt.
///
/// `se_code` is the `<= 32` return value, `last_error` whatever `GetLastError` held right
/// after it, and `installer_gone` whether the verified setup we just wrote has vanished
/// from `%TEMP%` — the strongest available signal that antivirus quarantined it, since
/// nothing else deletes that file between the write and the launch.
fn classify_launch_failure(
    se_code: u32,
    last_error: u32,
    installer_gone: bool,
    sac_enforcing: bool,
) -> UpdateError {
    const SE_ERR_FNF: u32 = 2;
    const SE_ERR_PNF: u32 = 3;
    const SE_ERR_ACCESSDENIED: u32 = 5;
    const SE_ERR_SHARE: u32 = 26;
    const ERROR_VIRUS_INFECTED: u32 = 225;
    const ERROR_VIRUS_DELETED: u32 = 226;
    const ERROR_CANCELLED: u32 = 1223;

    let sac_note = if sac_enforcing {
        " Windows Smart App Control is switched on, and it blocks apps it hasn't seen \
         signed before — that is the most likely cause here."
    } else {
        ""
    };

    // The setup file disappearing between our own verified write and this launch is not
    // something Windows or the user does — that is a scanner quarantining it.
    if installer_gone
        || matches!(se_code, SE_ERR_FNF | SE_ERR_PNF)
        || matches!(last_error, ERROR_VIRUS_INFECTED | ERROR_VIRUS_DELETED)
    {
        return UpdateError::Blocked(format!(
            "Your antivirus removed the downloaded installer before it could run.{sac_note} \
             Download SageThumbs 2K from the releases page instead."
        ));
    }
    if se_code == SE_ERR_ACCESSDENIED {
        // A declined UAC prompt reports access-denied with ERROR_CANCELLED behind it; a
        // policy/scanner block reports access-denied with something else (or nothing).
        if last_error == ERROR_CANCELLED {
            return UpdateError::Cancelled;
        }
        return UpdateError::Blocked(format!(
            "Windows refused to start the update installer.{sac_note} This is usually \
             antivirus or an administrator policy. You can download SageThumbs 2K from the \
             releases page instead."
        ));
    }
    // Something else has the setup file open for writing. We no longer do that to ourselves
    // (see `write_locked_installer`), so this now means a real outside holder — a scanner,
    // a backup agent, or the search indexer that woke up on a new .exe in %TEMP%.
    if se_code == SE_ERR_SHARE {
        return UpdateError::Blocked(format!(
            "Another program is holding the downloaded update open, so Windows wouldn't start \
             it.{sac_note} That is usually antivirus, a backup tool, or the search indexer \
             scanning the new file. Try again in a moment, or download SageThumbs 2K from the \
             releases page."
        ));
    }
    if last_error == ERROR_CANCELLED {
        return UpdateError::Cancelled;
    }
    // No administrator claim here. A user who declined (or could not satisfy) the elevation
    // prompt is already handled above as Cancelled/Blocked, so blaming permissions for every
    // OTHER failure code is simply a guess — and it was the wrong guess for the whole of the
    // SE_ERR_SHARE era, sending people to hunt for an admin account over our own file lock.
    UpdateError::Failed(format!(
        "Windows wouldn't start the update installer (error {se_code}). You can download \
         SageThumbs 2K from the releases page instead."
    ))
}

/// Launch the freshly-verified installer SILENTLY + ELEVATED (one UAC prompt). `Ok` once the
/// elevated process actually starts; otherwise a classified reason. On success the caller
/// should exit — the installer closes this app, upgrades in place, restarts Explorer, and
/// relaunches us with `--updated <ver>`.
///
/// `owner` OWNS the consent prompt. Passing `None` here (as this did until 2026-08-03) leaves
/// the UAC dialog ownerless, so it can land behind whatever is in front and read to the user
/// as "the update button does nothing" — invisible on a machine that elevates without a
/// prompt at all. The caller also tears its progress dialog down BEFORE calling this, so
/// there is nothing of ours left above the prompt.
fn launch_installer_silent(path: &Path, owner: HWND) -> Result<(), UpdateError> {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = crate::win::wide("runas"); // elevate: the setup writes HKLM + Program Files
    let file = crate::win::wide(&path.display().to_string());
    let params = crate::win::wide(INSTALL_FLAGS);
    let (ret, last_error) = unsafe {
        let ret = ShellExecuteW(
            Some(owner),
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        (ret, GetLastError().0)
    };
    // ShellExecuteW returns an HINSTANCE-like value > 32 on success; <= 32 is an error code.
    let se_code = ret.0 as usize;
    if se_code > 32 {
        return Ok(());
    }
    let err = classify_launch_failure(
        se_code as u32,
        last_error,
        !path.exists(),
        smart_app_control_enforcing(),
    );
    sagethumbs2k_core::safety::log(&format!(
        "update: installer launch failed (ShellExecute={se_code}, GetLastError={last_error}, \
         file_present={}): {}",
        path.exists(),
        match &err {
            UpdateError::Cancelled => "user cancelled",
            UpdateError::Blocked(m) | UpdateError::Failed(m) | UpdateError::Unverified(m) => m,
        }
    ));
    Err(err)
}

/// Set one line (1-based) of the shell progress dialog. Best-effort.
unsafe fn set_line(dlg: &windows::Win32::UI::Shell::IProgressDialog, line: u32, text: &str) {
    let w = crate::win::wide(text);
    let _ = dlg.SetLine(line, PCWSTR(w.as_ptr()), false, None);
}

/// Human-readable size for the progress sub-line (e.g. 9_223_820 → "8.8 MB").
fn human_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// The whole one-click flow behind the Settings "download & install" action, with a live
/// native progress dialog: resolve the latest installer, STREAM it down (bar driven by
/// bytes), verify it, then launch it silently + elevated. The dialog runs its own message-
/// pumping thread, so the bar stays smooth while this thread blocks in the download loop.
/// Returns the new version tag on success (the caller exits so the installer can take over),
/// or a classified [`UpdateError`] so the UI can explain itself and offer the manual page.
/// `parent` owns the progress dialog AND, once that is down, the elevation prompt.
pub(crate) fn download_and_install(parent: HWND) -> Result<String, UpdateError> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        CLSID_ProgressDialog, IProgressDialog, PROGDLG_AUTOTIME, PROGDLG_NORMAL,
    };

    // Issue #12: the portable zip's whole promise is "no installer, no admin rights". This is
    // the actual choke point every caller funnels through, so it is refused here even if a
    // caller (e.g. the About page's Update button) forgets its own `!settings::portable()`
    // check — `launch_installer_silent` below is what elevates and writes Program Files, and
    // it must never run for a portable copy on a PC the user may not have admin rights to.
    if sagethumbs2k_core::settings::portable() {
        return Err(UpdateError::Blocked(format!(
            "This is the portable copy of SageThumbs 2K, which never installs itself or asks \
             for administrator rights. Download the latest portable zip from {RELEASES_URL} \
             and replace the old files with the new ones."
        )));
    }

    let (tag, asset) = latest_installer_asset().ok_or_else(|| {
        UpdateError::Failed(
            "Couldn't find the installer for this PC on the GitHub releases page.".into(),
        )
    })?;
    // Fail fast on an unsigned release BEFORE spending a multi-MB download on it: the size +
    // sha256 the JSON also carries come from the same response an attacker who controlled it
    // would control too, so a missing signature is refused exactly like a bad one.
    let sig_url = asset.sig_url.clone().ok_or_else(unverified_release_error)?;

    // The shell progress dialog needs COM on this thread. Leaving it initialized afterward is
    // benign (one extra init on the UI thread); we never run the matching uninit, because the
    // success path exits the process and the failure path keeps the app running.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let dlg: IProgressDialog =
        unsafe { CoCreateInstance(&CLSID_ProgressDialog, None, CLSCTX_INPROC_SERVER) }.map_err(
            |_| UpdateError::Failed("Couldn't open the download progress dialog.".into()),
        )?;

    let title = crate::win::wide("Updating SageThumbs 2K");
    unsafe {
        let _ = dlg.SetTitle(PCWSTR(title.as_ptr()));
        let _ =
            dlg.StartProgressDialog(Some(parent), None, PROGDLG_NORMAL | PROGDLG_AUTOTIME, None);
        set_line(&dlg, 1, "Downloading update\u{2026}");
    }

    // Stream the download, driving the bar from bytes-so-far; Cancel aborts cleanly.
    let total = asset.size;
    let mut cancelled = false;
    let bytes = crate::sponsors::http_download_streaming(
        &asset.url,
        MAX_INSTALLER_BYTES,
        DOWNLOAD_TIMEOUT_SECS,
        &mut |done| unsafe {
            if dlg.HasUserCancelled().as_bool() {
                cancelled = true;
                return false;
            }
            let denom = if total != 0 { total } else { done.max(1) };
            let _ = dlg.SetProgress64(done, denom);
            set_line(
                &dlg,
                2,
                &format!("{} of {}", human_mb(done), human_mb(total)),
            );
            true
        },
    );

    // Everything up to (but NOT including) the elevated launch happens under the dialog.
    let prepared: Result<(PathBuf, std::fs::File), UpdateError> = (|| {
        let bytes = bytes.ok_or_else(|| {
            if cancelled {
                UpdateError::Cancelled
            } else {
                UpdateError::Failed(
                    "The update download didn't finish. Check your internet connection and \
                     try again."
                        .into(),
                )
            }
        })?;
        unsafe { set_line(&dlg, 1, "Verifying\u{2026}") };
        if !verify_installer_bytes(&bytes, &asset) {
            return Err(UpdateError::Failed(
                "The downloaded update failed its integrity check, so it was not run.".into(),
            ));
        }
        // The size + sha256 above only prove the download matches what the GitHub API's JSON
        // claimed - the same response an attacker who controlled that endpoint (or the asset
        // it points at) would also control. The signature is the actual trust anchor: it must
        // verify against the key COMPILED INTO THIS BINARY, which such an attacker cannot
        // rewrite. Never launch on a missing or failing signature, whatever the digest says.
        let sig_hex = http_fetch_capped(&sig_url, true, MAX_SIG_BYTES, SIG_TIMEOUT_SECS)
            .and_then(|b| String::from_utf8(b).ok());
        let signed = sig_hex
            .as_deref()
            .map(str::trim)
            .is_some_and(|hex| verify_signature(&UPDATE_PUBLIC_KEY, &bytes, hex));
        if !signed {
            return Err(unverified_release_error());
        }
        let written = write_locked_installer(&tag, &bytes, &asset)
            .map_err(|m| UpdateError::Failed(format!("The update couldn't be prepared: {m}.")))?;
        unsafe {
            set_line(&dlg, 1, "Installing update\u{2026}");
            let _ = dlg.SetProgress64(1, 1); // full bar; Inno's silent bar now shows the install
        }
        Ok(written)
    })();

    // Take the progress dialog DOWN before the elevation prompt goes up. It is a topmost
    // shell dialog, and leaving it in front is one of the ways a UAC consent prompt ends up
    // behind something — the user sees a taskbar flash, nothing else, and reports that the
    // updater "does nothing".
    unsafe {
        let _ = dlg.StopProgressDialog();
    }

    let (path, installer_lock) = prepared?;
    let launched = launch_installer_silent(&path, parent);
    drop(installer_lock); // the elevated process has opened the image (or the launch failed)
                          // `path` is disposable either way once we get here — on failure nothing will ever run
                          // it, and on success the elevated child has its OWN open handle on it by now
                          // (ShellExecuteW has returned, meaning the child process started), which is what
                          // makes deleting it safe: the same way a running .exe on Windows can be deleted from
                          // its directory while it keeps executing from the handle it already holds. If the
                          // child somehow opened it without FILE_SHARE_DELETE, this silently no-ops rather than
                          // failing the update; the goal is just to not leave a 10-15 MB setup .exe behind in
                          // %TEMP% on the (common) successful path, which the old success arm never did.
    cleanup_installer_payload(&path);
    match launched {
        Ok(()) => Ok(tag),
        Err(e) => Err(e),
    }
}

/// Best-effort removal of the downloaded installer payload, run after `launch_installer_silent`
/// returns regardless of outcome. See the call site for why this is safe even on success.
fn cleanup_installer_payload(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// `--update-selftest <setup.exe>`: the smoke test CI and the release gate run against a
/// freshly BUILT installer — the real post-download pipeline, end to end: byte
/// verification, the locked temp copy, and the elevated silent launch, through the exact
/// functions the About-card updater calls. Only the network download is substituted (the
/// bytes come from disk; the digest is computed from them, so `verify_installer_bytes`
/// still runs for real). Headless by design: no progress dialog and a null owner — on CI
/// runners and admin dev boxes the `runas` verb elevates without a prompt. The exit code
/// is the contract: success once the elevated installer PROCESS is running; the harness
/// (`scripts/test-self-update.ps1`) then watches the upgrade actually land on disk.
///
/// This exists because 1.3.3..=1.10.0 shipped an updater whose own write-mode lock made
/// every launch die with `SE_ERR_SHARE`, and no test noticed for twenty releases because
/// nothing ever drove the real pipeline against a real executable. This entry point is
/// what makes that class of failure a red build instead of a bug report.
pub(crate) fn run_selftest(setup: &Path) -> bool {
    let log = |m: &str| sagethumbs2k_core::safety::log(&format!("update-selftest: {m}"));
    let Ok(bytes) = std::fs::read(setup) else {
        log(&format!("couldn't read {}", setup.display()));
        return false;
    };
    let Some(sha256) = sha256_hex(&bytes) else {
        log("sha256 unavailable");
        return false;
    };
    let asset = InstallerAsset {
        url: String::new(),
        size: bytes.len() as u64,
        sha256,
        sig_url: None, // this harness substitutes only the download; see the doc comment above
    };
    if !verify_installer_bytes(&bytes, &asset) {
        log("verification refused the installer bytes");
        return false;
    }
    let (path, lock) = match write_locked_installer("selftest", &bytes, &asset) {
        Ok(pair) => pair,
        Err(m) => {
            log(m);
            return false;
        }
    };
    let launched = launch_installer_silent(&path, HWND::default());
    drop(lock);
    match launched {
        Ok(()) => {
            log("elevated installer launched");
            true
        }
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            log(&format!("launch failed: {}", e.message()));
            false
        }
    }
}

/// Shown by the installer-spawned `--updated <ver>` relaunch after a silent self-update:
/// a NON-BLOCKING tray balloon, NOT a modal dialog — so the update stays genuinely silent
/// (nothing to click, it auto-dismisses). The throwaway-window + temp-icon + balloon dance
/// lives once in [`crate::win::notify_toast`] (the instant capture's failure note shares it).
///
/// `installed` is the version the INSTALLER was built as (it passes its own compile-time
/// `AppVer`); `CARGO_PKG_VERSION` is what this running image ACTUALLY is. They normally
/// match. They don't when Windows couldn't replace a file that was still in use and Inno
/// deferred it to a reboot — `/NORESTART` means we never reboot, so `{app}\SageThumbs2K.exe`
/// is still the OLD binary when this relaunch fires. Claiming "you're now on <installed>"
/// there is simply false, and it's exactly what makes a stuck update look like a mystery
/// ("it said it updated and it's still on the old version"), so report what's true instead.
pub(crate) fn show_updated_toast(installed: &str) {
    let (title, body) = updated_toast_text(installed, env!("CARGO_PKG_VERSION"));
    unsafe {
        crate::win::notify_toast(title, &body, std::time::Duration::from_secs(6));
    }
}

/// Pure message choice for [`show_updated_toast`] — split out so the "don't claim a version
/// we aren't running" rule is unit-tested without a tray icon. An unparseable `installed`
/// (never seen from our own installer) falls back to the success wording rather than
/// alarming the user about a restart that isn't needed.
fn updated_toast_text(installed: &str, running: &str) -> (&'static str, String) {
    let mismatch = matches!(
        (parse_ver(installed), parse_ver(running)),
        (Some(i), Some(r)) if i != r
    );
    if mismatch {
        (
            "SageThumbs 2K update needs a restart",
            format!(
                "Version {installed} was downloaded, but Windows couldn't replace files that \
                 were still in use. Restart Windows to finish - you're still on {running} \
                 until then."
            ),
        )
    } else {
        (
            "SageThumbs 2K updated",
            format!("You're now on version {running}."),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        cleanup_installer_payload, is_security_body, offer_for, parse_cache, parse_ver,
        update_offer, LatestRelease, Offer,
    };
    use ed25519_dalek::{Signer, SigningKey};
    use std::os::windows::process::CommandExt;
    use std::path::Path;

    /// The downloaded installer must be swept regardless of outcome — before this fix, only
    /// the failure arm of `download_and_install`'s match on `launch_installer_silent` ever
    /// deleted it, leaving a real ~10-15 MB setup .exe behind in %TEMP% on every ordinary,
    /// successful update.
    #[test]
    fn cleanup_installer_payload_removes_the_file() {
        let path = std::env::temp_dir().join(format!(
            "st2k_update_cleanup_{}_{}.exe",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, b"fake installer bytes").unwrap();
        assert!(path.exists());

        cleanup_installer_payload(&path);

        assert!(
            !path.exists(),
            "the installer payload must be swept on every outcome, not only on failure"
        );
    }

    #[test]
    fn parses_and_orders_versions() {
        assert_eq!(parse_ver("v0.4.6"), Some((0, 4, 6)));
        assert_eq!(parse_ver("0.4.5"), Some((0, 4, 5)));
        assert_eq!(parse_ver("V1.0"), Some((1, 0, 0)));
        assert_eq!(parse_ver("2"), Some((2, 0, 0)));
        assert_eq!(parse_ver("0.4.6-rc1"), Some((0, 4, 6)));
        assert_eq!(parse_ver("0.5.0+build7"), Some((0, 5, 0)));
        assert_eq!(parse_ver("not-a-version"), None);

        // The ordering the check relies on (tuple compare = correct semver ordering here).
        assert!(parse_ver("0.4.6") > parse_ver("0.4.5"));
        assert!(parse_ver("0.5.0") > parse_ver("0.4.9"));
        assert!(parse_ver("1.0.0") > parse_ver("0.9.9"));
        assert!(parse_ver("0.4.5") <= parse_ver("0.4.5")); // equal = up to date
    }

    #[test]
    fn updated_toast_never_claims_a_version_we_arent_running() {
        // Normal silent update: installer version == this image's version.
        let (title, body) = super::updated_toast_text("1.3.8", "1.3.8");
        assert_eq!(title, "SageThumbs 2K updated");
        assert!(body.contains("now on version 1.3.8"), "{body}");

        // Deferred-to-reboot replace: the installer was 1.3.8 but we're still the old EXE.
        // The toast must NOT say "you're now on 1.3.8" — that's the mystery-update report.
        let (title, body) = super::updated_toast_text("1.3.8", "1.3.7");
        assert_eq!(title, "SageThumbs 2K update needs a restart");
        assert!(!body.contains("now on version"), "{body}");
        assert!(body.contains("Restart Windows"), "{body}");
        assert!(body.contains("still on 1.3.7"), "{body}");

        // A "v"-prefixed tag is the same version, not a mismatch.
        assert_eq!(
            super::updated_toast_text("v1.3.8", "1.3.8").0,
            "SageThumbs 2K updated"
        );

        // Unparseable installer version → don't cry "restart" at the user.
        assert_eq!(
            super::updated_toast_text("", "1.3.8").0,
            "SageThumbs 2K updated"
        );
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(
            super::sha256_hex(b"abc").as_deref(),
            Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
        );
        assert_eq!(
            super::sha256_hex(b"").as_deref(),
            Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
        );
    }

    /// A fixed-seed key so the test is deterministic - not the real signing key, and never
    /// will be; see `the_compiled_in_key_is_not_the_placeholder` below for that one.
    fn test_signing_key() -> SigningKey {
        SigningKey::from_bytes(&[7u8; 32])
    }

    fn hex_sig(sig: &ed25519_dalek::Signature) -> String {
        sig.to_bytes().iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn sign_then_verify_round_trips() {
        let key = test_signing_key();
        let bytes = b"a release artifact's exact bytes";
        let sig_hex = hex_sig(&key.sign(bytes));
        assert!(super::verify_signature(
            &key.verifying_key().to_bytes(),
            bytes,
            &sig_hex
        ));
    }

    #[test]
    fn a_flipped_byte_fails_verification() {
        let key = test_signing_key();
        let bytes = b"a release artifact's exact bytes";
        let sig_hex = hex_sig(&key.sign(bytes));
        let tampered = b"A release artifact's exact bytes"; // first byte flipped
        assert!(!super::verify_signature(
            &key.verifying_key().to_bytes(),
            tampered,
            &sig_hex
        ));
    }

    #[test]
    fn a_wrong_key_fails_verification() {
        let key = test_signing_key();
        let other_key = SigningKey::from_bytes(&[9u8; 32]);
        let bytes = b"a release artifact's exact bytes";
        let sig_hex = hex_sig(&key.sign(bytes));
        assert!(!super::verify_signature(
            &other_key.verifying_key().to_bytes(),
            bytes,
            &sig_hex
        ));
    }

    #[test]
    fn malformed_hex_is_refused_without_panicking() {
        let key = test_signing_key().verifying_key().to_bytes();
        for junk in [
            "",
            "not-hex-at-all-but-128-chars-long-so-length-alone-cannot-be-what-refuses-itxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
            "deadbeef", // far too short
            "gg", // not hex, and too short
        ] {
            assert!(
                !super::verify_signature(&key, b"anything", junk),
                "{junk:?} should not verify"
            );
        }
        // Exactly 128 chars but with one non-hex character - the length check alone must not
        // be enough to pass this.
        let mut almost_valid = "a".repeat(128);
        almost_valid.replace_range(0..1, "z");
        assert!(!super::verify_signature(&key, b"anything", &almost_valid));
        // A non-ASCII (multi-byte) 128-char string must not panic on the byte-index slicing.
        let non_ascii: String = "é".repeat(64); // 128 bytes, but not ASCII hex
        assert!(!super::verify_signature(&key, b"anything", &non_ascii));
    }

    #[test]
    fn the_compiled_in_key_is_not_the_placeholder() {
        // Fails until the integrator runs `examples/update-keygen.rs` and pastes its printed
        // public-key array literal over `UPDATE_PUBLIC_KEY` in this file. That is intentional:
        // an all-zero key makes every signature check refuse (see the constant's doc comment),
        // so a build that still carries the placeholder is safe, just permanently un-updatable
        // - this test is what turns "permanently un-updatable" into a build-time signal instead
        // of a silent trap discovered only when a real self-update is attempted.
        assert_ne!(
            super::UPDATE_PUBLIC_KEY,
            [0u8; 32],
            "UPDATE_PUBLIC_KEY is still the placeholder - paste in the real key from \
             examples/update-keygen.rs before shipping a build that must self-update"
        );
    }

    #[test]
    fn finds_the_sibling_sig_asset_beside_the_installer() {
        let json = serde_json::json!({
            "tag_name": "v0.7.0",
            "assets": [
                { "name": "SageThumbs2K-Setup-0.7.0.exe",
                  "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe",
                  "size": 100u64,
                  "digest": "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" },
                { "name": "SageThumbs2K-Setup-0.7.0.exe.sig",
                  "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe.sig" }
            ]
        });
        let (_, asset) =
            super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
        assert_eq!(
            asset.sig_url.as_deref(),
            Some("https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe.sig")
        );
    }

    #[test]
    fn a_release_with_no_sig_asset_has_no_sig_url() {
        let json = serde_json::json!({
            "tag_name": "v0.7.0",
            "assets": [
                { "name": "SageThumbs2K-Setup-0.7.0.exe",
                  "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.7.0/SageThumbs2K-Setup-0.7.0.exe",
                  "size": 100u64,
                  "digest": "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" }
            ]
        });
        let (_, asset) =
            super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
        assert_eq!(asset.sig_url, None);
    }

    #[test]
    fn picks_x64_setup_exe_and_normalizes_digest() {
        let json = serde_json::json!({
            "tag_name": "v0.6.3",
            "assets": [
                { "name": "notes.txt", "browser_download_url": "https://x/notes.txt", "size": 1 },
                { "name": "SageThumbs2K-Setup-debug.exe",
                  "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.6.3/SageThumbs2K-Setup-debug.exe",
                  "size": 42u64,
                  "digest": "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" },
                { "name": "SageThumbs2K-Setup-0.6.3.exe",
                  "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.6.3/SageThumbs2K-Setup-0.6.3.exe",
                  "size": 9_223_820u64,
                  "digest": "sha256:09D79A0C6589D7DC5AF5472CB8B1B56AAC0DFF51A47003B1146A9409F65C9835" }
            ]
        });
        let (tag, asset) =
            super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
        assert_eq!(tag, "0.6.3");
        assert!(asset.url.ends_with("SageThumbs2K-Setup-0.6.3.exe"));
        assert_eq!(asset.size, 9_223_820);
        assert_eq!(
            asset.sha256,
            "09d79a0c6589d7dc5af5472cb8b1b56aac0dff51a47003b1146a9409f65c9835"
        );
    }

    #[test]
    fn picks_only_the_matching_arm64_setup_exe() {
        let json = serde_json::json!({
            "tag_name": "v0.6.3",
            "assets": [
                { "name": "SageThumbs2K-Setup-0.6.3.exe",
                  "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.6.3/SageThumbs2K-Setup-0.6.3.exe",
                  "size": 100u64,
                  "digest": "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA" },
                { "name": "SageThumbs2K-Setup-0.6.3-arm64.exe",
                  "browser_download_url": "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v0.6.3/SageThumbs2K-Setup-0.6.3-arm64.exe",
                  "size": 200u64,
                  "digest": "sha256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB" }
            ]
        });

        let (_, x64) =
            super::installer_asset_from_json_for_arch(&json, "x86_64").expect("x64 asset");
        assert!(x64.url.ends_with("SageThumbs2K-Setup-0.6.3.exe"));
        assert_eq!(x64.size, 100);

        let (_, arm64) =
            super::installer_asset_from_json_for_arch(&json, "aarch64").expect("ARM64 asset");
        assert!(arm64.url.ends_with("SageThumbs2K-Setup-0.6.3-arm64.exe"));
        assert_eq!(arm64.size, 200);

        let x64_only = serde_json::json!({
            "tag_name": "v0.6.3",
            "assets": [json["assets"][0].clone()]
        });
        assert!(
            super::installer_asset_from_json_for_arch(&x64_only, "aarch64").is_none(),
            "ARM64 must not accept the x64 installer"
        );
        assert!(super::installer_asset_from_json_for_arch(&json, "x86").is_none());
    }

    #[test]
    fn native_windows_architecture_controls_cross_arch_update() {
        use windows::Win32::System::SystemInformation::{
            PROCESSOR_ARCHITECTURE, PROCESSOR_ARCHITECTURE_AMD64, PROCESSOR_ARCHITECTURE_ARM64,
        };

        assert_eq!(
            super::installer_arch_for_native(PROCESSOR_ARCHITECTURE_ARM64, "x86_64"),
            "aarch64",
            "an emulated x64 build on ARM64 must migrate to the native installer"
        );
        assert_eq!(
            super::installer_arch_for_native(PROCESSOR_ARCHITECTURE_AMD64, "x86_64"),
            "x86_64"
        );
        assert_eq!(
            super::installer_arch_for_native(PROCESSOR_ARCHITECTURE(u16::MAX), "aarch64"),
            "aarch64",
            "an unknown Windows architecture must fall back to the process target"
        );
    }

    #[test]
    fn installer_asset_requires_digest_and_canonical_repo_url() {
        let base = serde_json::json!({
            "tag_name": "v1.2.3",
            "assets": [{
                "name": "SageThumbs2K-Setup-1.2.3.exe",
                "browser_download_url":
                    "https://github.com/LunarWerxs/SageThumbs-2k/releases/download/v1.2.3/setup.exe",
                "size": 123
            }]
        });
        assert!(super::installer_asset_from_json(&base).is_none());

        let mut wrong_host = base;
        wrong_host["assets"][0]["digest"] = serde_json::json!(
            "sha256:09d79a0c6589d7dc5af5472cb8b1b56aac0dff51a47003b1146a9409f65c9835"
        );
        wrong_host["assets"][0]["browser_download_url"] =
            serde_json::json!("https://downloads.example.test/setup.exe");
        assert!(super::installer_asset_from_json(&wrong_host).is_none());
    }

    #[test]
    fn launch_failures_stay_distinguishable() {
        use super::{classify_launch_failure as classify, UpdateError as E};

        // A declined UAC prompt: access-denied with ERROR_CANCELLED behind it. The ONLY
        // case the UI is allowed to swallow.
        assert!(matches!(classify(5, 1223, false, false), E::Cancelled));

        // Same access-denied return, but nothing cancelled — a policy or scanner refusal.
        // This used to be reported as "cancelled at the Windows permission prompt" and then
        // silently discarded, which is the bug: the user saw nothing at all.
        let blocked = classify(5, 0, false, false);
        assert!(matches!(blocked, E::Blocked(_)));
        assert!(!blocked.message().is_empty());
        assert!(
            !blocked.message().contains("cancel"),
            "{}",
            blocked.message()
        );

        // The verified installer vanishing from %TEMP% between write and launch is a
        // quarantine, whatever ShellExecute claims.
        assert!(matches!(classify(5, 1223, true, false), E::Blocked(m) if m.contains("antivirus")));
        assert!(matches!(classify(2, 0, false, false), E::Blocked(_))); // SE_ERR_FNF
        assert!(matches!(classify(226, 226, false, false), E::Blocked(_))); // ERROR_VIRUS_DELETED

        // Smart App Control is named only when it is actually enforcing.
        assert!(classify(5, 0, false, true)
            .message()
            .contains("Smart App Control"));
        assert!(!classify(5, 0, false, false)
            .message()
            .contains("Smart App Control"));

        // A sharing violation is its own diagnosis now. It used to fall through to the
        // generic branch, which told the user to go find an administrator - for a file our
        // own write handle was holding shut.
        let shared = classify(26, 0, false, false);
        assert!(matches!(shared, E::Blocked(_)));
        assert!(shared.message().contains("holding the downloaded update"));
        assert!(!shared.message().contains("administrator"));

        // Anything else is a plain failure, and it still says something out loud - without
        // guessing at permissions, which is a cause the earlier branches already cover.
        let other = classify(31, 0, false, false);
        assert!(matches!(other, E::Failed(_)));
        assert!(other.message().contains("error 31"));
        assert!(other.message().contains("releases page"));
        assert!(
            !other.message().contains("administrator"),
            "{}",
            other.message()
        );
    }

    #[test]
    fn installer_file_stays_write_locked_until_launch() {
        let bytes = b"MZlocked-installer-test";
        let asset = super::InstallerAsset {
            url: String::new(),
            size: bytes.len() as u64,
            sha256: super::sha256_hex(bytes).expect("SHA-256"),
            sig_url: None,
        };
        let (path, lock) =
            super::write_locked_installer("test", bytes, &asset).expect("create locked installer");
        assert!(
            std::fs::OpenOptions::new().write(true).open(&path).is_err(),
            "a second writer must not be able to replace the verified installer"
        );
        assert!(
            std::fs::remove_file(&path).is_err(),
            "a deleter must not be able to remove the verified installer either"
        );
        drop(lock);
        std::fs::remove_file(path).expect("remove test installer");
    }

    /// The regression that shipped in 1.3.3 and broke one-click self-update for twenty
    /// releases: the lock held across the launch carried WRITE access, and Windows will not
    /// map an executable image whose file somebody else has open for writing. Every update
    /// attempt died with `ERROR_SHARING_VIOLATION` -> `SE_ERR_SHARE` (26), reported to the
    /// user as "installing an update needs an administrator".
    ///
    /// Assert it against a REAL `CreateProcess` on a REAL image, because that is the only
    /// thing that would have caught it - the unit test above passed happily throughout, since
    /// denying writers was never the part that was broken.
    #[test]
    fn locked_installer_can_still_be_launched() {
        let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let system_exe = Path::new(&root).join("System32").join("whoami.exe");
        let Ok(bytes) = std::fs::read(&system_exe) else {
            eprintln!("skipping: {} is not readable", system_exe.display());
            return;
        };
        let asset = super::InstallerAsset {
            url: String::new(),
            size: bytes.len() as u64,
            sha256: super::sha256_hex(&bytes).expect("SHA-256"),
            sig_url: None,
        };
        let (path, lock) = super::write_locked_installer("launch", &bytes, &asset)
            .expect("create locked installer");

        // ShellExecuteW("runas") ultimately maps the image exactly as this does.
        let spawned = std::process::Command::new(&path)
            .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
            .output();
        // ...and the lock is still doing its job while that happens.
        let writer_refused = std::fs::OpenOptions::new().write(true).open(&path).is_err();

        drop(lock);
        let _ = std::fs::remove_file(&path);

        assert!(
            spawned.is_ok(),
            "the lock held across the launch blocked the launch itself: {:?}",
            spawned.err()
        );
        assert!(writer_refused, "the lock stopped protecting the installer");
    }

    #[test]
    fn selftest_refuses_a_non_executable_before_any_launch() {
        let path =
            std::env::temp_dir().join(format!("st2k-selftest-notpe-{}.bin", std::process::id()));
        std::fs::write(&path, b"definitely not a PE image").unwrap();
        // Fails at verification (no MZ), so nothing is ever handed to ShellExecuteW —
        // which is also what makes this safe to run un-elevated in any environment.
        assert!(!super::run_selftest(&path));
        std::fs::remove_file(path).unwrap();
    }
    // ---- The updates-window offer decision -------------------------------------------

    /// A `LicenceSnapshot` with nothing in it but the two fields `update_offer` reads. Built
    /// by hand rather than through `license::at`, which would go to the registry, the
    /// breadcrumb file and the credential store for facts this decision does not use.
    fn snap_with_window(maint_unix: Option<u64>) -> crate::license::LicenceSnapshot {
        crate::license::LicenceSnapshot {
            mode: crate::license::Mode::Business,
            posture: crate::license::Posture::Silent,
            key_prefix: "esk_A1B2".into(),
            last_positive_unix: WINDOW_END - 1000,
            last_status: "active".into(),
            last_reason: String::new(),
            cert_expires_unix: None,
            maint_unix,
            now_unix: WINDOW_END + 1000,
        }
    }

    const WINDOW_END: u64 = 1_800_000_000;
    const DAY: u64 = 24 * 60 * 60;

    /// A personal-use / never-redeemed copy has no window at all, and must be offered every
    /// build exactly as it always has been. This is also the state of every licensed machine
    /// that has not yet heard a window from the relay, which is why "unknown" can never be
    /// allowed to read as "closed".
    #[test]
    fn no_window_on_record_always_installs() {
        let snap = snap_with_window(None);
        assert_eq!(
            update_offer(&snap, Some(WINDOW_END + 10 * DAY), false),
            Offer::Install
        );
        assert_eq!(update_offer(&snap, None, false), Offer::Install);
        assert_eq!(
            update_offer(&snap, Some(WINDOW_END + 10 * DAY), true),
            Offer::Install
        );
    }

    /// Inside the window: an ordinary install, and the boundary is INCLUSIVE - a build
    /// published at the very instant the window ends is one the customer paid for. Same
    /// `<=` rule `licence_cert::verify` applies to `build_date <= maint`.
    #[test]
    fn a_build_inside_the_window_installs_and_the_boundary_is_inclusive() {
        let snap = snap_with_window(Some(WINDOW_END));
        assert_eq!(
            update_offer(&snap, Some(WINDOW_END - DAY), false),
            Offer::Install
        );
        assert_eq!(update_offer(&snap, Some(WINDOW_END), false), Offer::Install);
    }

    /// One second past the end is outside, and the decision names the date so the message
    /// can say WHEN rather than just "no".
    #[test]
    fn a_build_published_after_the_window_offers_the_renewal() {
        let snap = snap_with_window(Some(WINDOW_END));
        assert_eq!(
            update_offer(&snap, Some(WINDOW_END + 1), false),
            Offer::OutsideWindow {
                ends_unix: WINDOW_END
            }
        );
    }

    /// ⛔ THE RULE THAT MUST SURVIVE EVERY FUTURE EDIT HERE: a security release is offered to
    /// a licensed installation whatever its window says. A customer who has stopped paying
    /// for new features has not stopped being someone we shipped software to.
    #[test]
    fn a_security_release_overrides_a_closed_window() {
        let snap = snap_with_window(Some(WINDOW_END));
        assert_eq!(
            update_offer(&snap, Some(WINDOW_END + 365 * DAY), true),
            Offer::Install
        );
    }

    /// No publication date (an un-redeployed Worker, a GitHub hiccup, a cache file written by
    /// an older build): we cannot place the build against the window, so we do not pretend to.
    #[test]
    fn an_unknown_publication_date_installs() {
        let snap = snap_with_window(Some(WINDOW_END));
        assert_eq!(update_offer(&snap, None, false), Offer::Install);
    }

    /// `offer_for` is the only thing that can answer `Offer::None`, and it does so for exactly
    /// one input: no release known.
    #[test]
    fn offer_for_answers_none_only_when_there_is_no_release() {
        let snap = snap_with_window(Some(WINDOW_END));
        assert_eq!(offer_for(&snap, None), Offer::None);
        let late = LatestRelease {
            tag: "9.9.9".into(),
            published_unix: Some(WINDOW_END + DAY),
            security: false,
        };
        assert_eq!(
            offer_for(&snap, Some(&late)),
            Offer::OutsideWindow {
                ends_unix: WINDOW_END
            }
        );
    }

    /// The security marker is matched as plain text, case-insensitively, anywhere in the
    /// notes - so release notes may format it however they like - and nothing else trips it.
    #[test]
    fn the_security_marker_is_recognised_only_as_itself() {
        assert!(is_security_body(
            "## Fixes\n\n[security-release] CVE-2026-1 in the SVG path."
        ));
        assert!(is_security_body("**[SECURITY-RELEASE]**"));
        assert!(!is_security_body("A security fix, but nobody marked it."));
        assert!(
            !is_security_body("security-release"),
            "the brackets are the marker"
        );
        assert!(!is_security_body(""));
    }

    /// The cache file gained two lines on 2026-09-10. A two-line file written by an older
    /// build must still parse, into the "offer it to everyone" shape - upgrading must never
    /// briefly refuse a build over a date the new code simply has not fetched yet.
    #[test]
    fn an_old_two_line_cache_still_parses_as_a_dateless_release() {
        let (secs, latest) = parse_cache("1700000000\n3.0.1\n").expect("two-line cache");
        assert_eq!(secs, 1_700_000_000);
        assert_eq!(latest, LatestRelease::bare("3.0.1".into()));

        let (_, latest) = parse_cache("1700000000\n3.0.2\n1800000000\n1\n").expect("four-line");
        assert_eq!(
            latest,
            LatestRelease {
                tag: "3.0.2".into(),
                published_unix: Some(1_800_000_000),
                security: true,
            }
        );

        // `0` on line 3 is the on-disk spelling of "not known", never 1970.
        let (_, latest) = parse_cache("1700000000\n3.0.3\n0\n0\n").expect("zeroed");
        assert_eq!(latest.published_unix, None);
        assert!(!latest.security);

        assert!(parse_cache("").is_none());
        assert!(
            parse_cache("1700000000\n\n").is_none(),
            "an empty tag is no answer"
        );
        assert!(parse_cache("not-a-number\n3.0.1\n").is_none());
    }
}
