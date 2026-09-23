//! Download, verify, launch: the self-update run and the errors it can end in.

use super::*;

use windows::Win32::UI::Shell::IProgressDialog;

/// Switches handed to the freshly-downloaded Inno setup for an unattended in-place upgrade.
/// `/SILENT` = bare progress bar, no wizard; `/SUPPRESSMSGBOXES` + `/FORCECLOSEAPPLICATIONS`
/// let it close+restart Explorer to swap the in-use DLL without prompting; `/NORESTART`
/// blocks a reboot prompt; `/UPDATED` is OUR marker the installer keys the post-update
/// "you're now on <ver>" relaunch off (see installer.iss `WasSelfUpdate`).
pub(super) const INSTALL_FLAGS: &str =
    "/SILENT /SUPPRESSMSGBOXES /NORESTART /FORCECLOSEAPPLICATIONS /UPDATED";

/// Why a one-click update didn't complete. This used to be a bare `String` that every
/// failure — user cancel, antivirus block, group policy, a dead network — collapsed into
/// "the update was cancelled at the Windows permission prompt", which the caller then
/// swallowed on the word "cancel". A user whose antivirus ate the installer saw NOTHING.
/// Keep these cases distinct: only [`UpdateError::Cancelled`] may be silent.
pub enum UpdateError {
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
    pub fn message(&self) -> &str {
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
pub(super) fn unverified_release_error() -> UpdateError {
    UpdateError::Unverified(format!(
        "This update's signature couldn't be verified, so SageThumbs 2K didn't install it. \
         Download it by hand from {RELEASES_URL} instead."
    ))
}

/// Is Smart App Control on and ENFORCING? SAC blocks unsigned executables outright and is
/// default-on for clean Windows 11 installs, so it is the likeliest silent killer of a
/// downloaded, unsigned setup. `VerifiedAndReputablePolicyState`: 0 = off, 1 = enforcement,
/// 2 = evaluation (audits, doesn't block). Read-only; absent key = not enforcing.
pub(super) fn smart_app_control_enforcing() -> bool {
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
pub(super) fn classify_launch_failure(
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
pub(super) fn launch_installer_silent(path: &Path, owner: HWND) -> Result<(), UpdateError> {
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
    st2k_base::safety::log(&format!(
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
pub(super) unsafe fn set_line(dlg: &IProgressDialog, line: u32, text: &str) {
    let w = crate::win::wide(text);
    let _ = dlg.SetLine(line, PCWSTR(w.as_ptr()), false, None);
}

/// Human-readable size for the progress sub-line (e.g. 9_223_820 → "8.8 MB").
pub(super) fn human_mb(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
}

/// The shell progress dialog for the download, already showing "Downloading update" under
/// `parent`. It needs COM on this thread; the caller always runs this on the updater's spawned
/// worker thread, so leaving COM initialized afterward is benign — the apartment is torn down
/// when that thread exits, and no matching uninit is ever needed here.
fn open_progress_dialog(parent: HWND) -> Result<IProgressDialog, UpdateError> {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{CLSID_ProgressDialog, PROGDLG_AUTOTIME, PROGDLG_NORMAL};

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
        // A dialog that fails to open only costs the progress display: the download and the
        // cancel check both still work, so it is no reason to refuse the update.
        let _ =
            dlg.StartProgressDialog(Some(parent), None, PROGDLG_NORMAL | PROGDLG_AUTOTIME, None);
        set_line(&dlg, 1, "Downloading update\u{2026}");
    }
    Ok(dlg)
}

/// Turn downloaded installer bytes into a launchable, still-locked file: integrity check,
/// signature check, write, and the stamped-version check. Any refusal leaves nothing behind.
fn verify_and_stage(
    dlg: &IProgressDialog,
    bytes: Vec<u8>,
    asset: &InstallerAsset,
    sig_url: &str,
    tag: &str,
) -> Result<(PathBuf, std::fs::File), UpdateError> {
    unsafe { set_line(dlg, 1, "Verifying\u{2026}") };
    if !verify_installer_bytes(&bytes, asset) {
        return Err(UpdateError::Failed(
            "The downloaded update failed its integrity check, so it was not run.".into(),
        ));
    }
    // The size + sha256 above only prove the download matches what the GitHub API's JSON
    // claimed - the same response an attacker who controlled that endpoint (or the asset
    // it points at) would also control. The signature is the actual trust anchor: it must
    // verify against the key COMPILED INTO THIS BINARY, which such an attacker cannot
    // rewrite. Never launch on a missing or failing signature, whatever the digest says.
    let sig_hex = http_fetch_capped(sig_url, true, MAX_SIG_BYTES, SIG_TIMEOUT_SECS)
        .and_then(|b| String::from_utf8(b).ok());
    let signed = sig_hex
        .as_deref()
        .map(str::trim)
        .is_some_and(|hex| verify_signature(&UPDATE_PUBLIC_KEY, &bytes, hex));
    if !signed {
        return Err(unverified_release_error());
    }
    let written = write_locked_installer(tag, &bytes, asset)
        .map_err(|m| UpdateError::Failed(format!("The update couldn't be prepared: {m}.")))?;
    // The signature proves these bytes are a release WE signed; it does not say which
    // one. A feed that hands out a genuine, older installer under a newer tag would
    // pass everything above and downgrade the machine (2026-09-19 audit concern 3).
    // The version Inno stamps into the setup's own resource is inside the signed bytes,
    // so it is the binding: it must be the version the feed advertised and newer than
    // this running build, or the file is never launched.
    if let Err(why) =
        stamped_version_is_the_advertised_upgrade(&written.0, tag, env!("CARGO_PKG_VERSION"))
    {
        drop(written.1);
        cleanup_installer_payload(&written.0);
        return Err(UpdateError::Failed(why));
    }
    unsafe {
        set_line(dlg, 1, "Installing update\u{2026}");
        let _ = dlg.SetProgress64(1, 1); // full bar; Inno's silent bar now shows the install
    }
    Ok(written)
}

/// The whole one-click flow behind the Settings "download & install" action, with a live
/// native progress dialog: resolve the latest installer, STREAM it down (bar driven by
/// bytes), verify it, then launch it silently + elevated. The dialog runs its own message-
/// pumping thread, so the bar stays smooth while this thread blocks in the download loop.
/// Returns the new version tag on success (the caller exits so the installer can take over),
/// or a classified [`UpdateError`] so the UI can explain itself and offer the manual page.
/// `parent` owns the progress dialog AND, once that is down, the elevation prompt.
pub fn download_and_install(parent: HWND) -> Result<String, UpdateError> {
    // Issue #12: the portable zip's whole promise is "no installer, no admin rights". This is
    // the actual choke point every caller funnels through, so it is refused here even if a
    // caller (e.g. the About page's Update button) forgets its own `!settings::portable()`
    // check — `launch_installer_silent` below is what elevates and writes Program Files, and
    // it must never run for a portable copy on a PC the user may not have admin rights to.
    if st2k_base::settings::portable() {
        return Err(UpdateError::Blocked(format!(
            "This is the portable copy of SageThumbs 2K, which never installs itself or asks \
             for administrator rights. Download the latest portable zip from {RELEASES_URL} \
             and replace the old files with the new ones."
        )));
    }
    sweep_stale_installers();

    let (tag, asset) = latest_installer_asset().ok_or_else(|| {
        UpdateError::Failed(
            "Couldn't find the installer for this PC on the GitHub releases page.".into(),
        )
    })?;
    // Fail fast on an unsigned release BEFORE spending a multi-MB download on it: the size +
    // sha256 the JSON also carries come from the same response an attacker who controlled it
    // would control too, so a missing signature is refused exactly like a bad one.
    let sig_url = asset.sig_url.clone().ok_or_else(unverified_release_error)?;

    let dlg = open_progress_dialog(parent)?;

    // Stream the download, driving the bar from bytes-so-far; Cancel aborts cleanly.
    let total = asset.size;
    let mut cancelled = false;
    let bytes = crate::sponsors::http_download_streaming(
        &asset.url,
        MAX_INSTALLER_BYTES,
        DOWNLOAD_TIMEOUT_SECS,
        &mut |done| download_progress_tick(&dlg, total, done, &mut cancelled),
    );

    // Everything up to (but NOT including) the elevated launch happens under the dialog.
    let downloaded = bytes.ok_or_else(|| {
        if cancelled {
            UpdateError::Cancelled
        } else {
            UpdateError::Failed(
                "The update download didn't finish. Check your internet connection and \
                 try again."
                    .into(),
            )
        }
    });
    let prepared =
        downloaded.and_then(|bytes| verify_and_stage(&dlg, bytes, &asset, &sig_url, &tag));

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
                          // On failure nothing will ever run `path`, so this removes it. On success it is a no-op:
                          // the running setup has the file mapped as its image, and Windows refuses to delete a
                          // mapped image (the same rule `SwapAsideInUseDll` works around by renaming). Which is also
                          // what makes the call harmless, since Inno's Setup.tmp re-opens setup.exe by path for its
                          // payload. The file left behind is removed by `sweep_stale_installers` next time.
    cleanup_installer_payload(&path);
    match launched {
        Ok(()) => Ok(tag),
        Err(e) => Err(e),
    }
}

/// One tick of the streaming-download progress callback: abort if the user cancelled the
/// shell dialog, otherwise drive its bar and sub-line from bytes-so-far; returns whether the
/// download should keep going.
fn download_progress_tick(
    dlg: &IProgressDialog,
    total: u64,
    done: u64,
    cancelled: &mut bool,
) -> bool {
    unsafe {
        if dlg.HasUserCancelled().as_bool() {
            *cancelled = true;
            return false;
        }
        let denom = if total != 0 { total } else { done.max(1) };
        let _ = dlg.SetProgress64(done, denom);
        set_line(
            dlg,
            2,
            &format!("{} of {}", human_mb(done), human_mb(total)),
        );
        true
    }
}

/// Best-effort removal of the downloaded installer payload, run after `launch_installer_silent`
/// returns regardless of outcome. See the call site for why this is safe even on success.
pub(super) fn cleanup_installer_payload(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Remove the setup payloads earlier updates staged in %TEMP% (`write_locked_installer`'s
/// `SageThumbs2K-Setup-*.exe`), which [`cleanup_installer_payload`] cannot delete while their
/// setup runs. Only files over an hour old: a setup still running is refused by Windows anyway,
/// and one staged by a concurrent update a moment ago is left alone.
fn sweep_stale_installers() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let Some(cutoff) = SystemTime::now().checked_sub(std::time::Duration::from_secs(3600)) else {
        return;
    };
    for e in entries.flatten() {
        if is_staged_installer_name(&e.file_name().to_string_lossy())
            && e.metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < cutoff)
        {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// `write_locked_installer`'s naming: `SageThumbs2K-Setup-<tag>-<pid>-<nonce>-<attempt>.exe`.
fn is_staged_installer_name(name: &str) -> bool {
    name.strip_prefix("SageThumbs2K-Setup-")
        .and_then(|rest| rest.strip_suffix(".exe"))
        .is_some_and(|mid| mid.matches('-').count() >= 3)
}

#[cfg(test)]
mod sweep_tests {
    use super::is_staged_installer_name;

    /// Only the updater's own staged payloads are swept: a setup the USER downloaded into
    /// %TEMP% (release name, with or without the arch suffix) is never touched.
    #[test]
    fn only_the_updaters_own_staged_names_are_swept() {
        assert!(is_staged_installer_name(
            "SageThumbs2K-Setup-3.2.0-1234-1789043696000000000-0.exe"
        ));
        assert!(!is_staged_installer_name("SageThumbs2K-Setup-3.2.0.exe"));
        assert!(!is_staged_installer_name(
            "SageThumbs2K-Setup-3.2.0-arm64.exe"
        ));
        assert!(!is_staged_installer_name("SageThumbs2K-Portable-3.2.0.zip"));
        assert!(!is_staged_installer_name("other-1-2-3.exe"));
    }
}
