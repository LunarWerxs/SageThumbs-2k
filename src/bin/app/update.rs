//! "Check for updates" — ask the GitHub releases API for the latest tag and compare it
//! to the running build. Reuses the sponsor fetch (WinINet HTTPS, bounded timeout, and
//! the `SageThumbs2K` User-Agent the GitHub API requires). Best-effort: any failure
//! (offline, repo renamed/moved, no releases yet, rate-limited) becomes `Failed`, so the
//! UI can fall back to "couldn't reach the update server — check GitHub manually."

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;

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
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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
/// never touches GitHub directly and can't be rate-limited. This fetch doubles as the
/// periodic heartbeat check-in (new=0 — the fresh-install marker is owned by the app's
/// own startup path). Returns the latest tag (e.g. "0.4.9") or None on any failure.
fn latest_from_worker() -> Option<String> {
    // Opt this check-in out of the public tally on a developer test box (see `is_dev_machine`).
    let dev = if sagethumbs2k_core::settings::is_dev_machine() { "&dev=1" } else { "" };
    let url = format!("{BANNER_URL}?v={}&os={}&new=0{dev}", env!("CARGO_PKG_VERSION"), os_tag());
    let bytes = http_fetch(&url, true)?;
    let json: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    let tag = json.get("latest")?.as_str()?.trim();
    (!tag.is_empty()).then(|| tag.to_string())
}

/// LAZY, THROTTLED background update check routed through the sponsor Worker (the
/// resident screenshot helper calls this on a timer). Hits the network at most once per
/// [`CHECK_INTERVAL`] and — unlike [`lazy_check`] — does NOT re-nudge from the cache in
/// between, so a newer version toasts at most once per interval instead of every tick.
/// Falls back to the direct GitHub [`check`] if the Worker didn't supply a version.
pub(crate) fn lazy_check_worker<F: FnOnce(String) + Send + 'static>(on_newer: F) {
    std::thread::spawn(move || {
        let now = now_secs();
        if let Some((last, _)) = read_cache() {
            if now.saturating_sub(last) < CHECK_INTERVAL.as_secs() {
                return; // checked recently — don't re-toast within the interval
            }
        }
        // Worker first (also the heartbeat check-in); GitHub as a fallback.
        let newer = match latest_from_worker() {
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
        };
        if let Some(tag) = newer {
            on_newer(tag);
        }
    });
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
    sha256: Option<String>, // lowercase hex, no "sha256:" prefix
}

/// Pull the Windows installer asset out of GitHub's latest-release JSON — the `.exe` whose
/// name looks like our setup — returning its tag + download URL + size + sha256, or None on
/// any failure (offline, no release, no matching asset).
fn latest_installer_asset() -> Option<(String, InstallerAsset)> {
    let bytes = http_fetch(RELEASES_API, true)?;
    installer_asset_from_json(&serde_json::from_slice(&bytes).ok()?)
}

/// Pure parse of GitHub's latest-release JSON → (tag, installer asset). Split from the fetch
/// so it can be unit-tested against a real release body with no network.
fn installer_asset_from_json(json: &serde_json::Value) -> Option<(String, InstallerAsset)> {
    let tag = json.get("tag_name")?.as_str()?.trim_start_matches(['v', 'V']).to_string();
    let asset = json.get("assets")?.as_array()?.iter().find(|a| {
        a.get("name").and_then(|n| n.as_str()).is_some_and(|n| {
            let n = n.to_ascii_lowercase();
            n.ends_with(".exe") && n.contains("setup")
        })
    })?;
    let url = asset.get("browser_download_url")?.as_str()?.to_string();
    let size = asset.get("size").and_then(serde_json::Value::as_u64).unwrap_or(0);
    let sha256 = asset
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase);
    Some((tag, InstallerAsset { url, size, sha256 }))
}

/// SHA-256 of `data` as lowercase hex, via Windows CNG (no extra crate). None on failure.
fn sha256_hex(data: &[u8]) -> Option<String> {
    use windows::Win32::Security::Cryptography::{BCryptHash, BCRYPT_SHA256_ALG_HANDLE};
    let mut out = [0u8; 32];
    let status = unsafe { BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out) };
    status.is_ok().then(|| out.iter().map(|b| format!("{b:02x}")).collect())
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
    if let Some(want) = &asset.sha256 {
        if sha256_hex(bytes).as_deref() != Some(want.as_str()) {
            return false; // integrity check failed
        }
    }
    true
}

/// Launch the freshly-verified installer SILENTLY + ELEVATED (one UAC prompt). Returns true
/// once the elevated process actually starts (the user accepted elevation); false if they
/// declined or the launch failed. On success the caller should exit — the installer closes
/// this app, upgrades in place, restarts Explorer, and relaunches us with `--updated <ver>`.
fn launch_installer_silent(path: &Path) -> bool {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let verb = crate::win::wide("runas"); // elevate: the setup writes HKLM + Program Files
    let file = crate::win::wide(&path.display().to_string());
    let params = crate::win::wide(INSTALL_FLAGS);
    let ret = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns an HINSTANCE-like value > 32 on success; <= 32 is an error code
    // (e.g. SE_ERR_ACCESSDENIED when the user cancels the UAC prompt).
    ret.0 as usize > 32
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
/// or Err(message) so the UI can offer the manual page. `parent` owns the dialog.
pub(crate) fn download_and_install(parent: HWND) -> Result<String, String> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{
        IProgressDialog, CLSID_ProgressDialog, PROGDLG_AUTOTIME, PROGDLG_NORMAL,
    };

    let (tag, asset) = latest_installer_asset().ok_or("couldn't find the installer on GitHub")?;

    // The shell progress dialog needs COM on this thread. Leaving it initialized afterward is
    // benign (one extra init on the UI thread); we never run the matching uninit, because the
    // success path exits the process and the failure path keeps the app running.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let dlg: IProgressDialog =
        unsafe { CoCreateInstance(&CLSID_ProgressDialog, None, CLSCTX_INPROC_SERVER) }
            .map_err(|_| "couldn't open the progress dialog".to_string())?;

    let title = crate::win::wide("Updating SageThumbs 2K");
    unsafe {
        let _ = dlg.SetTitle(PCWSTR(title.as_ptr()));
        let _ = dlg.StartProgressDialog(Some(parent), None, PROGDLG_NORMAL | PROGDLG_AUTOTIME, None);
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
            set_line(&dlg, 2, &format!("{} of {}", human_mb(done), human_mb(total)));
            true
        },
    );

    let outcome: Result<(), &'static str> = (|| {
        let bytes = bytes.ok_or(if cancelled {
            "you cancelled the update"
        } else {
            "the download failed"
        })?;
        unsafe { set_line(&dlg, 1, "Verifying\u{2026}") };
        if !verify_installer_bytes(&bytes, &asset) {
            return Err("the download failed its integrity check");
        }
        if asset.sha256.is_none() {
            // No per-asset checksum from the release API → only the MZ-header + size checks ran.
            // Log it so a missing release-workflow digest is visible, not a silent downgrade.
            sagethumbs2k_core::safety::log(
                "update: release asset carries no sha256 digest — installer verified by header + size only",
            );
        }
        let mut path = std::env::temp_dir();
        path.push(format!("SageThumbs2K-Setup-{tag}.exe"));
        std::fs::write(&path, &bytes).map_err(|_| "couldn't save the installer")?;
        // Re-verify the ON-DISK bytes right before the elevated launch. `fs::write` then
        // `ShellExecuteW("runas")` is a TOCTOU window: another process with %TEMP% write access
        // could swap the file, and the UAC prompt would then elevate the swapped binary. Re-reading
        // and re-verifying what's actually on disk shrinks that window to near-zero (a swap during
        // or just after the write is caught). On any mismatch, delete and abort.
        let on_disk_ok = std::fs::read(&path).map(|d| verify_installer_bytes(&d, &asset)).unwrap_or(false);
        if !on_disk_ok {
            let _ = std::fs::remove_file(&path);
            return Err("the saved installer failed re-verification");
        }
        unsafe {
            set_line(&dlg, 1, "Installing update\u{2026}");
            let _ = dlg.SetProgress64(1, 1); // full bar; Inno's silent bar now shows the install
        }
        if launch_installer_silent(&path) {
            Ok(())
        } else {
            let _ = std::fs::remove_file(&path); // UAC-cancel / launch failure → don't leave the .exe in %TEMP%
            Err("the update was cancelled at the Windows permission prompt")
        }
    })();

    unsafe {
        let _ = dlg.StopProgressDialog();
    }
    outcome.map(|()| tag).map_err(str::to_string)
}

/// Shown by the installer-spawned `--updated <ver>` relaunch after a silent self-update:
/// a NON-BLOCKING tray balloon ("You're now on <ver>"), NOT a modal dialog — so the update
/// stays genuinely silent (nothing to click, it auto-dismisses). The throwaway-window +
/// temp-icon + balloon dance lives once in [`crate::win::notify_toast`] (the instant
/// capture's failure note shares it).
pub(crate) fn show_updated_toast(ver: &str) {
    unsafe {
        crate::win::notify_toast(
            "SageThumbs 2K updated",
            &format!("You're now on version {ver}."),
            std::time::Duration::from_secs(6),
        );
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
    fn picks_setup_exe_and_normalizes_digest() {
        let json = serde_json::json!({
            "tag_name": "v0.6.3",
            "assets": [
                { "name": "notes.txt", "browser_download_url": "https://x/notes.txt", "size": 1 },
                { "name": "SageThumbs2K-Setup-0.6.3.exe",
                  "browser_download_url": "https://github.com/o/r/releases/download/v0.6.3/SageThumbs2K-Setup-0.6.3.exe",
                  "size": 9_223_820u64,
                  "digest": "sha256:09D79A0C6589D7DC5AF5472CB8B1B56AAC0DFF51A47003B1146A9409F65C9835" }
            ]
        });
        let (tag, asset) = super::installer_asset_from_json(&json).expect("asset");
        assert_eq!(tag, "0.6.3");
        assert!(asset.url.ends_with("SageThumbs2K-Setup-0.6.3.exe"));
        assert_eq!(asset.size, 9_223_820);
        assert_eq!(
            asset.sha256.as_deref(),
            Some("09d79a0c6589d7dc5af5472cb8b1b56aac0dff51a47003b1146a9409f65c9835")
        );
    }
}
