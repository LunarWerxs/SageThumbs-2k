//! "Check for updates" — ask the GitHub releases API for the latest tag and compare it
//! to the running build. Reuses the sponsor fetch (WinINet HTTPS, bounded timeout, and
//! the `SageThumbs2K` User-Agent the GitHub API requires). Best-effort: any failure
//! (offline, repo renamed/moved, no releases yet, rate-limited) becomes `Failed`, so the
//! UI can fall back to "couldn't reach the update server — check GitHub manually."

use std::io::{Read, Seek, Write};
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

use crate::sponsors::{http_fetch, os_tag, BANNER_URL};

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
    /// A newer release exists; carries its display tag (e.g. "0.4.6").
    Available(String),
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
            UpdateCheck::Available(tag.trim_start_matches(['v', 'V']).to_string())
        }
        (Some(_), Some(_)) => UpdateCheck::UpToDate,
        _ => UpdateCheck::Failed, // unparseable tag — don't guess
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The tiny throttle/cache file ("`<unix_secs>\n<latest_tag>\n`"), next to the diagnostics
/// log in `%LOCALAPPDATA%`.
fn cache_path() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("SageThumbs2K-update.txt"))
}

fn read_cache() -> Option<(u64, String)> {
    let text = std::fs::read_to_string(cache_path()?).ok()?;
    let mut lines = text.lines();
    let secs = lines.next()?.trim().parse::<u64>().ok()?;
    let tag = lines.next()?.trim().to_string();
    (!tag.is_empty()).then_some((secs, tag))
}

fn write_cache(secs: u64, tag: &str) {
    if let Some(p) = cache_path() {
        let _ = std::fs::write(p, format!("{secs}\n{tag}\n"));
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
        if let Some((last, tag)) = read_cache() {
            if now.saturating_sub(last) < CHECK_INTERVAL.as_secs() {
                if is_newer(&tag) {
                    on_newer(tag);
                }
                return;
            }
        }
        // Stale or first run: one real check. Cache a definitive result (up-to-date or a
        // newer tag) so we don't re-hit for a day; on a transient failure leave the cache
        // untouched so the NEXT Settings open retries instead of waiting out the interval.
        match check() {
            UpdateCheck::Available(tag) => {
                write_cache(now, &tag);
                on_newer(tag);
            }
            UpdateCheck::UpToDate => write_cache(now, env!("CARGO_PKG_VERSION")),
            UpdateCheck::Failed => {}
        }
    });
}

/// Ask the sponsor Worker for the latest version. The Worker already serves a `latest`
/// field in its manifest (sourced from GitHub server-side + edge-cached), so the client
/// never touches GitHub directly and can't be rate-limited. Reuses the startup manifest
/// request with new=0. Returns the latest tag (e.g. "0.4.9") or None on any failure.
fn latest_from_worker() -> Option<String> {
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
    (!tag.is_empty()).then(|| tag.to_string())
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
fn check_throttled() -> Option<String> {
    let now = now_secs();
    if !check_due() {
        return None; // checked recently — don't re-report within the interval
    }
    // Worker first; GitHub as a fallback.
    match latest_from_worker() {
        Some(tag) => {
            write_cache(now, &tag); // cache whatever the latest is (newer or not)
            is_newer(&tag).then_some(tag)
        }
        None => match check() {
            UpdateCheck::Available(tag) => {
                write_cache(now, &tag);
                Some(tag)
            }
            UpdateCheck::UpToDate => {
                write_cache(now, env!("CARGO_PKG_VERSION"));
                None
            }
            UpdateCheck::Failed => None,
        },
    }
}

/// [`check_throttled`] on a background thread — the resident screenshot helper's timer path.
pub(crate) fn lazy_check_worker<F: FnOnce(String) + Send + 'static>(on_newer: F) {
    std::thread::spawn(move || {
        if let Some(tag) = check_throttled() {
            on_newer(tag);
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
    if let Some(tag) = check_throttled() {
        unsafe {
            crate::win::notify_toast(
                "SageThumbs 2K update available",
                &format!("Version {tag} is ready. Open SageThumbs 2K to install it."),
                Duration::from_secs(8),
            );
        }
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
    Some((tag, InstallerAsset { url, size, sha256 }))
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

/// Atomically create the downloaded installer and keep an open handle that permits
/// readers but denies other writers/deleters. Holding that handle through
/// `ShellExecuteW("runas")` closes the pathname replacement window between the final
/// hash check and the elevated process opening the image.
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
        let opened = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&path);
        let mut file = match opened {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("couldn't save the installer"),
        };
        if file.write_all(bytes).is_err() || file.sync_all().is_err() {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err("couldn't save the installer");
        }
        if file.rewind().is_err() {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err("couldn't verify the saved installer");
        }
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
/// Keep these three cases distinct: only [`UpdateError::Cancelled`] may be silent.
pub(crate) enum UpdateError {
    /// The user backed out themselves (progress-dialog Cancel, or declining the Windows
    /// permission prompt). The one case that must not nag.
    Cancelled,
    /// Something outside the user's immediate control refused to run the verified installer
    /// — antivirus, Smart App Control, or an administrator policy. Carries the explanation.
    Blocked(String),
    /// Everything else: offline, no matching release asset, a failed integrity check.
    Failed(String),
}

impl UpdateError {
    /// The user-facing sentence for this failure ("" for a plain cancel, which says nothing).
    pub(crate) fn message(&self) -> &str {
        match self {
            UpdateError::Cancelled => "",
            UpdateError::Blocked(m) | UpdateError::Failed(m) => m,
        }
    }
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
    if last_error == ERROR_CANCELLED {
        return UpdateError::Cancelled;
    }
    UpdateError::Failed(format!(
        "The update installer couldn't be started (Windows error {se_code}). Installing an \
         update needs an administrator; if you're signed in as a standard user, download \
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
            UpdateError::Blocked(m) | UpdateError::Failed(m) => m,
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

    let (tag, asset) = latest_installer_asset().ok_or_else(|| {
        UpdateError::Failed(
            "Couldn't find the installer for this PC on the GitHub releases page.".into(),
        )
    })?;

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
    match launched {
        Ok(()) => Ok(tag),
        Err(e) => {
            let _ = std::fs::remove_file(&path); // never leave a setup .exe behind in %TEMP%
            Err(e)
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
    use super::parse_ver;

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

        // Anything else is a plain failure, and it still says something out loud.
        let other = classify(31, 0, false, false);
        assert!(matches!(other, E::Failed(_)));
        assert!(other.message().contains("administrator"));
    }

    #[test]
    fn installer_file_stays_write_locked_until_launch() {
        let bytes = b"MZlocked-installer-test";
        let asset = super::InstallerAsset {
            url: String::new(),
            size: bytes.len() as u64,
            sha256: super::sha256_hex(bytes).expect("SHA-256"),
        };
        let (path, lock) =
            super::write_locked_installer("test", bytes, &asset).expect("create locked installer");
        assert!(
            std::fs::OpenOptions::new().write(true).open(&path).is_err(),
            "a second writer must not be able to replace the verified installer"
        );
        drop(lock);
        std::fs::remove_file(path).expect("remove test installer");
    }
}
