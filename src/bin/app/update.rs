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
mod verify;
use verify::*;
mod install;
mod task;
use install::*;
pub(crate) use install::{download_and_install, UpdateError};
pub(crate) use task::{remove_update_task, run_one_shot_check, spawn_due_check, sync_update_task};
pub use verify::UPDATE_PUBLIC_KEY;

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

/// Assemble a [`LatestRelease`] from a release JSON object. The tag is passed already
/// stripped of any leading `v`, the publication date is read from `published_field` as
/// ISO-8601, and `security_field` is handed to `security`, which knows whether that field
/// holds GitHub's release notes text or the Worker's boolean.
fn latest_release(
    tag: String,
    json: &serde_json::Value,
    published_field: &str,
    security_field: &str,
    security: impl FnOnce(&serde_json::Value) -> bool,
) -> LatestRelease {
    LatestRelease {
        tag,
        published_unix: json
            .get(published_field)
            .and_then(|v| v.as_str())
            .and_then(crate::license::parse_iso_unix),
        security: json.get(security_field).is_some_and(security),
    }
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
            UpdateCheck::Available(latest_release(
                tag.trim_start_matches(['v', 'V']).to_string(),
                &json,
                "published_at",
                "body",
                |v| v.as_str().is_some_and(is_security_body),
            ))
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
    Some(latest_release(
        tag.to_string(),
        &json,
        "latestPublishedAt",
        "latestSecurity",
        |v| v.as_bool().unwrap_or(false),
    ))
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
        // process. (The `--update-check` one-shot runs its own licence tick through
        // `license::background_tick` - see `run_one_shot_check` - so a machine with no
        // resident helper is covered too.)
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
mod tests;
