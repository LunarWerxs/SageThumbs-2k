//! Handing a verb to the app EXE: the launch itself, the file-list hand-off, and the test probe that intercepts it.

use super::*;

/// Launch the companion EXE, forwarding `args` (empty → the Options/Settings window).
/// Resolves the EXE from the DLL's own directory (host-process-safe).
pub(super) fn launch_app(args: &[&str]) -> bool {
    // Test seam: swallows the launch (recording the argv) so a unit test can never
    // start a real process. Always false in a real build — see `intercept_launch`.
    if intercept_launch(args) {
        return true;
    }
    // A failed launch used to vanish without a trace — the menu item just "did nothing"
    // (missing companion EXE on a broken install, or spawn failure). Log it so the
    // Diagnostics log at least explains a dead menu item.
    let Some(exe) = st2k_base::host::sibling_of_dll(st2k_base::host::APP_EXE) else {
        st2k_base::safety::log(
            "launch_app: companion EXE not found next to the DLL — menu action dropped",
        );
        return false;
    };
    if let Err(e) = std::process::Command::new(exe).args(args).spawn() {
        st2k_base::safety::log(&format!("launch_app: spawn failed: {e}"));
        return false;
    }
    true
}

/// Whether this [`launch_app`] call was intercepted instead of performed. **Always
/// `false` in a real build** — the launcher spawns exactly as it always has.
///
/// Under `cfg(test)` it records the argv in [`launch_probe`] and returns `true`, so a
/// unit test never starts a real process. This is the same "absent → no-op" gate
/// [`st2k_exe`] gives the routed verbs, applied to the OTHER sibling lookup — and it
/// has to be an explicit seam, because that lookup's absence can't be relied on. The
/// module docs spell out why: cargo puts `SageThumbs2K.exe` in the very `deps\`
/// directory a test binary runs from, so `sibling_of_dll(APP_EXE)` resolves and a test
/// really does spawn the companion GUI app. That app opens a dialog and, through its
/// `read_listfile`, DELETES the listfile it was handed — which is what made
/// [`tests::rapid_same_kind_launches_get_distinct_listfile_names`] flaky: three real
/// `--convert` processes raced its scan and ate the files it was counting.
#[cfg(test)]
pub(super) fn intercept_launch(args: &[&str]) -> bool {
    launch_probe::record(args);
    true
}

#[cfg(not(test))]
pub(super) fn intercept_launch(_args: &[&str]) -> bool {
    false
}

/// Whether [`launch_with_list`] actually handed the list off to the companion app.
pub(super) enum ListLaunch {
    /// Nothing in `paths` matched the filter — no list to hand off (not a failure;
    /// the caller reports this the same as [`ListLaunch::Launched`]).
    Nothing,
    /// The list file was written and handed off to the companion app.
    Launched,
    /// The list file couldn't be written (a full or redirected `%TEMP%`) or the launch
    /// failed — the menu item would otherwise silently do nothing, with no trace of why.
    Failed,
}

/// Write `paths` (after `filter`) to a uniquely-named temp `.lst` file and launch the
/// companion EXE with `flag <listfile>` — the shared body behind the four
/// "handoff a file list to a companion-app dialog" launchers below, which used to
/// repeat this write-then-launch shape with only the filter/prefix/flag differing.
///
/// The filename mixes the host PID with a per-process atomic counter, not the PID
/// alone: the DLL runs inside one long-lived `explorer.exe`/`dllhost.exe` host, so two
/// near-simultaneous launches of the *same* kind from that host used to compute the
/// identical `st2k_<kind>_<pid>.lst` path, and the second write could clobber the
/// first before the spawned app read it. The counter makes every call's filename
/// unique for the life of the host process. No cleanup is needed on success: the
/// companion app's `read_listfile` deletes the file once it's read — while the failure
/// paths above delete it themselves, so a broken install leaves nothing behind.
pub(super) fn launch_with_list(
    paths: &[String],
    filter: impl Fn(&str) -> bool,
    prefix: &str,
    flag: &str,
) -> ListLaunch {
    let filtered: Vec<String> = paths
        .iter()
        .filter(|p| filter(p.as_str()))
        .cloned()
        .collect();
    if filtered.is_empty() {
        return ListLaunch::Nothing;
    }
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut lf = std::env::temp_dir();
    lf.push(format!("st2k_{prefix}_{}_{n}.lst", std::process::id()));
    if let Err(e) = std::fs::write(&lf, filtered.join("\r\n")) {
        st2k_base::safety::log_error(&format!(
            "launch_with_list: couldn't write {}: {e}",
            lf.display()
        ));
        let _ = std::fs::remove_file(&lf); // a failed write can still leave a partial file
        return ListLaunch::Failed;
    }
    let Some(s) = lf.to_str() else {
        st2k_base::safety::log_error(&format!(
            "launch_with_list: temp path {} isn't valid Unicode",
            lf.display()
        ));
        let _ = std::fs::remove_file(&lf);
        return ListLaunch::Failed;
    };
    if !launch_app(&[flag, s]) {
        let _ = std::fs::remove_file(&lf);
        return ListLaunch::Failed;
    }
    ListLaunch::Launched
}

/// Launch the companion EXE's Convert… dialog over the selected images. Resolves the
/// EXE from the DLL's OWN directory (NOT current_exe(), which in the shell host
/// returns explorer.exe/dllhost.exe) — a temp-file handoff is robust to many files /
/// odd names where a command line would overflow or mis-quote.
pub(super) fn launch_convert_dialog(paths: &[String]) -> ListLaunch {
    launch_with_list(paths, is_image, "convert", "--convert")
}

/// Launch the companion EXE's keyless uploader over the selected images. The app
/// POSTs each file and copies the resulting link(s) to the clipboard; the ORIGINAL
/// files are never modified or deleted (the app's `--upload-keep` path keeps them,
/// unlike the screenshot `--upload` path which deletes its throwaway capture).
pub(super) fn launch_upload(paths: &[String]) -> ListLaunch {
    launch_with_list(paths, is_image, "upload", "--upload-keep")
}

/// Launch the companion EXE's "Files to folder" name-prompt dialog over the
/// selected files (unfiltered — any file type).
pub(super) fn launch_files_to_folder(paths: &[String]) -> ListLaunch {
    launch_with_list(paths, |_| true, "f2f", "--files-to-folder")
}

/// Launch the companion EXE's "Rename with pattern…" dialog over the selected files
/// (unfiltered — any file type, same as [`launch_files_to_folder`]).
pub(super) fn launch_rename_with_pattern(paths: &[String]) -> ListLaunch {
    launch_with_list(paths, |_| true, "rnpattern", "--rename-with-pattern")
}

/// Launch the companion EXE's "Tags to folders" dialog over the selected audio files.
pub(super) fn launch_tags_to_folders(audio: &[String]) -> ListLaunch {
    launch_with_list(audio, |_| true, "ttf", "--tags-to-folders")
}

/// Open the verbose, copyable "Image info" window in the companion app (it gathers the
/// full file/image/EXIF metadata via `read_info_verbose` and shows it in a scrollable
/// dialog — far more than the old one-line message box).
pub(super) fn show_info(path: &str) {
    launch_app(&["--image-info", path]);
}

/// The launches [`intercept_launch`] swallowed, so a test can assert on what *would*
/// have been spawned — a stronger check than the side effect it replaces.
#[cfg(test)]
pub(super) mod launch_probe {
    use std::sync::{Mutex, MutexGuard};

    static LAUNCHES: Mutex<Vec<Vec<String>>> = Mutex::new(Vec::new());

    /// A test panicking elsewhere poisons the lock; that must not cascade into a
    /// second, unrelated failure here.
    fn log() -> MutexGuard<'static, Vec<Vec<String>>> {
        LAUNCHES.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub(in super::super) fn record(args: &[&str]) {
        log().push(args.iter().map(|a| (*a).to_string()).collect());
    }

    /// Every argv recorded so far, in call order. Unit tests share one process and run
    /// in parallel, so a caller must FILTER this down to its own launches (by a
    /// pid-unique listfile name, say) rather than assume it owns the log — which is
    /// also why there's deliberately no `clear()` for two tests to race on.
    pub(in super::super) fn recorded() -> Vec<Vec<String>> {
        log().clone()
    }
}
