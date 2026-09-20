//! The command-line modes the EXE answers before it opens Settings: shots, conversions, captures, folder tools, heals and user-state maintenance.

use super::*;

/// Hidden, side-effect-free UI integration route (`--screenshot-automation`) plus the
/// hidden dev measurement flags (`--bench-preview` / `--bench-nav` / `--bench-mash`).
/// Checked first: the automation route takes precedence over every other output-capable
/// mode even in a malformed mixed invocation, preserving its privacy/safety contract —
/// synthetic full-screen pixels only, with clipboard, file, dialog, and upload paths
/// fenced inside the overlay. Returns `true` if a flag fired (caller should return).
pub(super) unsafe fn dispatch_diagnostic_modes(hinst: HINSTANCE, args: &[String]) -> bool {
    if args.iter().any(|a| a == "--screenshot-automation") {
        crate::screenshot::run_capture_automation(hinst);
        return true;
    }
    // `--bench-preview <dir>`: times the Quick preview's REAL decode path over a folder —
    // a cold pass, then a warm pass off the cache — so the arrow-key stepping cost is a
    // number rather than an impression. Console output, no window, no side effects.
    if let Some((dir, _)) = bench_flag_args(args, "--bench-preview") {
        crate::preview::run_bench(&dir);
        return true;
    }
    // `--bench-nav <dir> <steps>`: the same measurement one level up — real viewer window,
    // real WM_KEYDOWN arrow presses, timed from keypress to painted.
    if let Some((dir, steps)) = bench_flag_args(args, "--bench-nav") {
        crate::preview::run_nav_bench(hinst, &dir, steps);
        return true;
    }
    // `--bench-mash <dir> <keys>`: the HELD arrow key, pressed without waiting for each
    // paint, so several decodes really are in flight at once. `ST2K_NO_CANCEL=1` switches
    // abandonment off for an A/B on the same binary.
    if let Some((dir, keys)) = bench_flag_args(args, "--bench-mash") {
        crate::preview::run_mash_bench(hinst, &dir, keys);
        return true;
    }
    false
}

/// The update-plumbing CLI flags: `--update-check` (the throttled one-shot the Scheduled
/// Task runs, and the piggyback in `main` spawns), `--update-selftest <setup.exe>` (the CI
/// / release-gate smoke test), `--first-run-seen` (suppress the welcome window on an
/// upgrade), and `--update-task [remove]` (register/drop the per-user Scheduled Task).
/// Returns `true` if a flag fired (caller should return).
pub(super) unsafe fn dispatch_update_modes(args: &[String]) -> bool {
    if args.iter().any(|a| a == "--update-check") {
        crate::update::run_one_shot_check();
        return true;
    }
    if let Some(pos) = args.iter().position(|a| a == "--update-selftest") {
        let ok = args
            .get(pos + 1)
            .is_some_and(|p| crate::update::run_selftest(std::path::Path::new(p)));
        std::process::exit(if ok { 0 } else { 1 });
    }
    if args.iter().any(|a| a == "--first-run-seen") {
        crate::first_run::mark_shown();
        return true;
    }
    if let Some(pos) = args.iter().position(|a| a == "--update-task") {
        if args.get(pos + 1).map(String::as_str) == Some("remove") {
            crate::update::remove_update_task();
        } else {
            crate::update::sync_update_task();
        }
        return true;
    }
    false
}

/// Builds the `ShotOpts` for `--shot --window preview`: `--file <path>` input (synthetic
/// gradient if absent), plus optional headless state forcing — `--hot N` (button N
/// hovered), `--focus N` (caption-toolbar button N keyboard-focused, same `N` numbering as
/// `--hot`), `--focus-transport N` (transport-strip button N keyboard-focused), `--pinned`,
/// `--pdf-page N`, `--frame N` (animation frame), `--play` (video strip), `--source` (raw
/// text of a normally-rendered file), and the rest.
pub(super) fn build_shot_preview_opts(args: &[String]) -> crate::preview::ShotOpts {
    let val = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|p| args.get(p + 1))
    };
    crate::preview::ShotOpts {
        file: val("--file").cloned(),
        hot: val("--hot").and_then(|s| s.parse().ok()),
        focus: val("--focus").and_then(|s| s.parse().ok()),
        focus_transport: val("--focus-transport").and_then(|s| s.parse().ok()),
        pinned: args.iter().any(|a| a == "--pinned"),
        pdf_page: val("--pdf-page").and_then(|s| s.parse().ok()),
        frame: val("--frame").and_then(|s| s.parse().ok()),
        play: args.iter().any(|a| a == "--play"),
        dpi: val("--dpi").and_then(|s| s.parse().ok()),
        scroll: val("--scroll").and_then(|s| s.parse().ok()),
        wheel: val("--wheel").and_then(|s| s.parse().ok()),
        wheel_ctrl: args.iter().any(|a| a == "--ctrl"),
        wheel_shift: args.iter().any(|a| a == "--shift"),
        sel: val("--sel").and_then(|s| parse_arg_pair(s, |c| c == ',')),
        find: val("--find").cloned(),
        wait_ms: val("--wait-ms").and_then(|s| s.parse().ok()),
        source: args.iter().any(|a| a == "--source"),
        toggle_source: args.iter().any(|a| a == "--toggle-source"),
        toggle_theme: args.iter().any(|a| a == "--toggle-theme"),
        size: val("--size").and_then(|s| parse_arg_pair(s, |c| c == 'x' || c == 'X')),
    }
}

/// Splits an argument value at the first `sep` character and parses both trimmed sides into
/// a pair, or `None` when either side is missing or unparseable.
fn parse_arg_pair<T: std::str::FromStr>(s: &str, sep: impl Fn(char) -> bool) -> Option<(T, T)> {
    let (a, b) = s.split_once(sep)?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// The default (`settings`) window of `--shot`: builds the requested tab (or drives the
/// settings-wide search headlessly when `--search <needle>` is present, optionally picking
/// the first hit with a trailing `!`) and renders it.
pub(super) unsafe fn run_shot_settings_window(
    hinst: HINSTANCE,
    dark: bool,
    out: &str,
    args: &[String],
) -> bool {
    let tab = args
        .iter()
        .position(|a| a == "--tab")
        .and_then(|p| args.get(p + 1))
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(0);
    if let Some(needle) = args
        .iter()
        .position(|a| a == "--search")
        .and_then(|p| args.get(p + 1))
    {
        settings_dlg::run_shot_search(hinst, dark, out, needle)
    } else {
        settings_dlg::run_shot(hinst, dark, out, tab)
    }
}

/// The body of `--shot <out.png> [--tab N] [--window settings|convert|eyedropper|...]`:
/// picks the window named by `--window` (default `settings`) and renders it INVISIBLY
/// (off-screen) to `out`. `pos` is the index of the `--shot` flag itself.
///
/// `--dpi N` (any window, not just `preview`) forces the headless-shot DPI override BEFORE
/// the window is built, so a locale/layout regression test can capture the same dialog at
/// 96 and 192 without a physical high-DPI monitor (2026-09-05 audit finding F36 wants both
/// covered for the Convert and first-run windows, which previously had no `--dpi` wiring at
/// all). `preview`'s own `ShotOpts.dpi` re-applies the same value, which is harmless: `set_dpi_override`
/// is an idempotent store, not a toggle.
pub(super) unsafe fn run_shot_mode(
    hinst: HINSTANCE,
    dark: bool,
    args: &[String],
    pos: usize,
) -> bool {
    if let Some(dpi) = args
        .iter()
        .position(|a| a == "--dpi")
        .and_then(|p| args.get(p + 1))
        .and_then(|s| s.parse::<i32>().ok())
    {
        crate::win::set_dpi_override(dpi);
    }
    let window = args
        .iter()
        .position(|a| a == "--window")
        .and_then(|p| args.get(p + 1))
        .map(String::as_str)
        .unwrap_or("settings");
    let Some(out) = args.get(pos + 1) else {
        return false;
    };
    match window {
        "convert" => crate::convert::run_shot_convert(out),
        // The Convert dialog's failure report, over canned failures: it only appears when a
        // batch actually fails, which a shot cannot arrange.
        "convert-report" => crate::convert_report::run_shot_convert_report(out),
        "eyedropper" => crate::eyedropper::run_shot_eyedropper(out),
        "feedback" => crate::feedback::run_shot_feedback(out),
        "about" => crate::about::run_shot_about(out),
        "doctor" => crate::doctor_report::run_shot_doctor(out),
        "firstrun" => crate::first_run::run_shot_first_run(out),
        "firstrun2" => crate::first_run::run_shot_first_run2(out),
        // The OCR result window, over canned text (no recognizer run) — or the
        // real text of `--file <img>` when you want to see an actual scan.
        "ocr" => {
            let file = args
                .iter()
                .position(|a| a == "--file")
                .and_then(|p| args.get(p + 1));
            crate::ocr_result::run_shot_ocr(out, file.map(String::as_str))
        }
        "preview" => {
            let opts = build_shot_preview_opts(args);
            crate::preview::run_shot_preview(hinst, dark, out, &opts)
        }
        _ => run_shot_settings_window(hinst, dark, out, args),
    }
}

/// `--convert <listfile>` (the batch-convert dialog), `--shot-gif <out.gif>` (walks every
/// Settings tab and encodes a regenerable README/site walkthrough GIF, checked before
/// `--shot` by exact match so the shorter flag never swallows it), and `--shot` itself (see
/// [`run_shot_mode`]). Returns `true` if a flag fired (caller should return).
pub(super) unsafe fn dispatch_convert_and_shot_modes(
    hinst: HINSTANCE,
    dark: bool,
    args: &[String],
) -> bool {
    if let Some(pos) = args.iter().position(|a| a == "--convert") {
        if let Some(listfile) = args.get(pos + 1) {
            run_convert_dialog(hinst, listfile);
        }
        return true;
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot-gif") {
        let ok = args
            .get(pos + 1)
            .is_some_and(|out| settings_dlg::run_shot_gif(hinst, dark, out));
        std::process::exit(i32::from(!ok));
    }
    if let Some(pos) = args.iter().position(|a| a == "--shot") {
        let ok = run_shot_mode(hinst, dark, args, pos);
        std::process::exit(i32::from(!ok));
    }
    false
}

/// The read-only diagnostic and file-verb CLI flags: `--explorer-selection`,
/// `--eyedropper`, `--prebuild <folder>`, `--image-info <path>`, `--ocr <png>`,
/// `--ocr-keep <path> [--page N]` and `--preview [path]`. Returns `true` if a flag fired
/// (caller should return).
pub(super) unsafe fn dispatch_file_and_capture_modes(hinst: HINSTANCE, args: &[String]) -> bool {
    // `--explorer-selection` prints what a global hotkey would act on right now — one path
    // per line, nothing when there is no selection — and exits. This exists because "I
    // pressed the hotkey and nothing happened" is otherwise unanswerable: it separates "the
    // hotkey never fired" from "the hotkey fired but Explorer reported no selection".
    if args.iter().any(|a| a == "--explorer-selection") {
        run_explorer_selection(args);
        return true;
    }
    // Eyedropper mode: `--eyedropper` (spawned by the DLL verb) opens the
    // system-wide screen color picker.
    if args.iter().any(|a| a == "--eyedropper") {
        run_eyedropper(hinst);
        return true;
    }
    // Pre-build thumbnails: `--prebuild <folder>` (the folder right-click entry) walks the
    // folder and fills Explorer's thumbnail cache, showing progress.
    if let Some(pos) = args.iter().position(|a| a == "--prebuild") {
        run_prebuild_mode(args, pos);
        return true;
    }
    // Read-only document modes: `--image-info <path>`, `--ocr <png>` and
    // `--ocr-keep <path> [--page N]` — whichever appears first wins.
    if run_document_modes(args) {
        return true;
    }
    // Quick preview: `--preview [path]` launches the single-instance QuickLook-style
    // viewer. A second launch forwards its path to the running viewer and exits.
    if let Some(pos) = args.iter().position(|a| a == "--preview") {
        // The business-licence lock reaches the Quick preview too (`licence_state`).
        if refused_by_licence("licence_preview_locked") {
            return true;
        }
        let path = args
            .get(pos + 1)
            .filter(|p| !p.starts_with("--"))
            .map(String::as_str);
        crate::preview::run_preview(hinst, path);
        return true;
    }
    false
}

/// Runs `--explorer-selection`: waits the optional `--after-ms N` delay (the resolver needs
/// the real foreground window, and a console launch steals it), then prints the selection
/// path, one per line.
unsafe fn run_explorer_selection(args: &[String]) {
    // `--after-ms N` waits first. Necessary, not a convenience: the resolver reads the
    // FOREGROUND Explorer window, and launching this console tool makes the CONSOLE the
    // foreground window — so without a delay it always reports "nothing".
    let wait = args
        .iter()
        .position(|a| a == "--after-ms")
        .and_then(|p| args.get(p + 1))
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if wait > 0 {
        std::thread::sleep(std::time::Duration::from_millis(wait.min(60_000)));
    }
    if let explorer_selection::PreviewTarget::Path(p) = explorer_selection::preview_target() {
        println!("{p}");
    }
}

/// Runs `--prebuild <folder>`: applies the folder-verb licence lock, then repairs the
/// shell-quoted drive root and fills Explorer's thumbnail cache.
unsafe fn run_prebuild_mode(args: &[String], pos: usize) {
    // The folder verb is a registry entry, not a menu the DLL gates, so the lock is
    // applied here: a stopped copy would only fill the cache with icons anyway.
    if refused_by_licence("licence_locked_notice") {
        return;
    }
    if let Some(dir) = args.get(pos + 1) {
        // A DRIVE ROOT arrives here as `E:"` — see `prebuild::unmangle_shell_path` for
        // why the shell's own quoting does that and why it cannot be fixed in the
        // registry string. Repairing it here also heals installs that already wrote
        // the old command.
        let dir = sagethumbs2k_core::prebuild::unmangle_shell_path(dir);
        prebuild_dlg::run_prebuild(&dir);
    }
}

/// Runs whichever of the read-only document modes (`--image-info <path>`, `--ocr <png>`,
/// `--ocr-keep <path> [--page N]`) is present; `true` when one fired.
unsafe fn run_document_modes(args: &[String]) -> bool {
    // Image info: `--image-info <path>` (spawned by the DLL's Image info verb) shows
    // a verbose, copyable metadata dump for the file.
    if let Some(pos) = args.iter().position(|a| a == "--image-info") {
        if let Some(path) = args.get(pos + 1) {
            image_info::run_image_info(path);
        }
        return true;
    }
    // Screen OCR: `--ocr <png>` (spawned by the capture overlay's OCR button /
    // Ctrl+T) reads the text out of the throwaway capture, copies it, and shows it.
    if let Some(pos) = args.iter().position(|a| a == "--ocr") {
        if let Some(path) = args.get(pos + 1) {
            ocr_result::run_ocr(path);
        }
        return true;
    }
    // Screen OCR on a file the user owns: `--ocr-keep <path> [--page N]` (the Quick
    // preview's OCR toolbar button). Unlike `--ocr` it does NOT delete its input. Checked
    // before `--ocr` (exact match, so they don't overlap).
    if let Some(pos) = args.iter().position(|a| a == "--ocr-keep") {
        if let Some(path) = args.get(pos + 1) {
            let page = args
                .iter()
                .position(|a| a == "--page")
                .and_then(|p| args.get(p + 1))
                .and_then(|s| s.parse::<u32>().ok());
            ocr_result::run_ocr_keep(path, page);
        }
        return true;
    }
    false
}

/// The business-licence lock on a shell-launched mode (the folder verb's pre-build, the
/// Quick preview): when the copy is stopped, say why and where the key goes (a toast, since
/// the process has no window yet) and report that the mode was refused.
pub(super) unsafe fn refused_by_licence(notice_key: &str) -> bool {
    if !license::shell_locked() {
        return false;
    }
    crate::win::notify_toast(
        "SageThumbs 2K",
        t(notice_key),
        std::time::Duration::from_secs(5),
    );
    true
}

/// The screenshot-related CLI flags: `--screenshot-instant`, `--screenshot-ocr`,
/// `--screenshot`, `--screenshot-daemon`, `--hotkey-action`, `--upload <png>`,
/// `--upload-keep <listfile>` and `--screenshot-toggle`. Returns `true` if a flag fired
/// (caller should return).
pub(super) unsafe fn dispatch_screenshot_modes(hinst: HINSTANCE, args: &[String]) -> bool {
    // Instant capture: grabs the whole screen straight to the clipboard + a PNG, no
    // overlay. Checked before `--screenshot` (exact match, so they don't overlap).
    if args.iter().any(|a| a == "--screenshot-instant") {
        crate::screenshot::capture_instant();
        return true;
    }
    // Screen OCR mode: opens the same overlay, but the first finished region drag reads
    // its text and closes — no editor. Checked before `--screenshot` (exact match).
    if args.iter().any(|a| a == "--screenshot-ocr") {
        crate::screenshot::run_capture_ocr(hinst);
        return true;
    }
    // Screenshot mode: opens the Flameshot-style capture + annotation overlay
    // (region -> draw -> copy/save). Wired to a hotkey by the opt-in tray daemon.
    if args.iter().any(|a| a == "--screenshot") {
        crate::screenshot::run_capture(hinst);
        return true;
    }
    // Screenshot daemon: runs the opt-in tray helper that registers the global hotkey and
    // spawns captures. Launched at logon only after the user enables it in Settings.
    if args.iter().any(|a| a == "--screenshot-daemon") {
        crate::screenshot::run_daemon(hinst);
        return true;
    }
    // Custom action hotkey: spawned by the daemon when the user's assigned chord fires;
    // runs whichever action they bound in Settings > Screenshots.
    if args.iter().any(|a| a == "--hotkey-action") {
        crate::hotkey::run_hotkey_action(hinst);
        return true;
    }
    // Upload mode: POSTs a capture to a keyless host and copies the URL to the clipboard.
    if let Some(pos) = args.iter().position(|a| a == "--upload") {
        if let Some(path) = args.get(pos + 1) {
            crate::screenshot::run_upload(path);
        }
        return true;
    }
    // Upload-keep mode: uploads the USER files listed to the keyless host and copies the
    // link(s) to the clipboard, WITHOUT deleting the originals (only `--upload` deletes,
    // since its file is a throwaway capture). Exact-match above means `--upload` never
    // swallows this longer flag. An optional trailing `--url-to <file>` is how `st2k upload`
    // reuses this same path headlessly: see `run_upload_keep`'s doc comment for the contract.
    if let Some(pos) = args.iter().position(|a| a == "--upload-keep") {
        if let Some(listfile) = args.get(pos + 1) {
            let url_to = args
                .iter()
                .position(|a| a == "--url-to")
                .and_then(|p| args.get(p + 1))
                .map(String::as_str);
            crate::screenshot::run_upload_keep(listfile, url_to);
        }
        return true;
    }
    // Toggle the screenshot hotkey on/off (HKCU autostart + the tray daemon).
    if args.iter().any(|a| a == "--screenshot-toggle") {
        crate::screenshot::set_enabled(!crate::screenshot::is_enabled());
        return true;
    }
    false
}

/// `--files-to-folder <listfile>`, `--rename-with-pattern <listfile>`, and
/// `--tags-to-folders <listfile>` (all spawned by DLL verbs over a multi-file
/// selection). Returns `true` if a flag fired (caller should return).
pub(super) unsafe fn dispatch_folder_modes(hinst: HINSTANCE, args: &[String]) -> bool {
    if let Some(pos) = args.iter().position(|a| a == "--files-to-folder") {
        if let Some(listfile) = args.get(pos + 1) {
            run_files_to_folder_dialog(hinst, listfile);
        }
        return true;
    }
    if let Some(pos) = args.iter().position(|a| a == "--rename-with-pattern") {
        if let Some(listfile) = args.get(pos + 1) {
            run_rename_with_pattern_dialog(hinst, listfile);
        }
        return true;
    }
    if let Some(pos) = args.iter().position(|a| a == "--tags-to-folders") {
        if let Some(listfile) = args.get(pos + 1) {
            run_tags_to_folders_dialog(hinst, listfile);
        }
        return true;
    }
    false
}

/// The install-time heal flags: `--updated <ver>` (launched by the installer's [Run] step
/// right after a SILENT self-update finishes — heals the hotkey daemon the installer had
/// to kill, then pops a non-blocking "you're now on <ver>" toast) and `--heal-hotkeys` (run
/// after EVERY install, including manual/silent reinstalls that never pass `/UPDATED`).
/// Returns `true` if a flag fired (caller should return).
pub(super) unsafe fn dispatch_heal_modes(args: &[String]) -> bool {
    if let Some(pos) = args.iter().position(|a| a == "--updated") {
        heal_after_install();
        let ver = args
            .get(pos + 1)
            .map_or(env!("CARGO_PKG_VERSION"), String::as_str);
        crate::update::show_updated_toast(ver);
        offer_thumbnail_refresh(ver);
        return true;
    }
    if args.iter().any(|a| a == "--heal-hotkeys") {
        heal_after_install();
        return true;
    }
    false
}

/// Re-spawn this EXE with `--rebuild-thumbnail-cache-now` (detached — no wait, no window)
/// and return, so a caller invoking `--rebuild-thumbnail-cache` synchronously (the
/// installer's postinstall [Run] step) is not blocked for the ~30s
/// `restart_explorer_clearing_cache` can take. If the re-spawn itself fails, fall back to
/// doing the work directly rather than silently skipping what the postinstall checkbox
/// promised.
pub(super) fn detach_rebuild_thumbnail_cache() {
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        let _ = sagethumbs2k_core::shellcmd::restart_explorer_clearing_cache();
        return;
    };
    let spawned = std::process::Command::new(&exe)
        .arg("--rebuild-thumbnail-cache-now")
        .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
        .spawn();
    if spawned.is_err() {
        let _ = sagethumbs2k_core::shellcmd::restart_explorer_clearing_cache();
    }
}

/// After a silent self-update relaunch, a decoder/format fix shows no difference for any
/// file Explorer already thumbnailed — only clearing `thumbcache_*.db` does, and until now
/// the only UI for that was a postinstall checkbox `/UPDATED` deliberately skips (see G88 /
/// review #88: "silent self-update never invalidates the thumbcache"). Records
/// `CacheStaleSince=<ver>` in HKCU so the state is visible even if the toast is missed or
/// dismissed unread, offers a one-click fix, and clears the marker once the refresh
/// actually completes. The restart runs INSIDE the click handler, on this thread: this is
/// the short-lived relaunch process `show_updated_toast` pops its balloon from, and it exits
/// the moment the toast returns, so a detached worker would be torn down mid-restart, its
/// verify loop and explorer.exe fallback with it. Blocking here for the ~30 s cycle costs
/// nothing: the process has no other work, and the restart takes the tray icon with it.
pub(super) unsafe fn offer_thumbnail_refresh(ver: &str) {
    let _ = sagethumbs2k_core::settings::set_string("CacheStaleSince", ver);
    crate::win::notify_toast_action(
        "Refresh thumbnails now?",
        "New thumbnails won't appear for files Explorer already cached until the cache is \
         cleared. Click to refresh thumbnails now (restarts Explorer).",
        std::time::Duration::from_secs(8),
        || {
            let _ = sagethumbs2k_core::shellcmd::restart_explorer_clearing_cache();
            let _ = sagethumbs2k_core::settings::set_string("CacheStaleSince", "");
        },
    );
}

/// `--queue-cache-rebuild` (the installer, running this as the ORIGINAL user): queue a
/// one-shot `--rebuild-thumbnail-cache` in this user's RunOnce for the next sign-in. The
/// installer asks for it when the DLL swap is waiting on a restart; written from the
/// elevated installer itself the value would land in whichever admin's hive answered the
/// UAC prompt, not the user's.
pub(super) fn queue_cache_rebuild() {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let cmd = format!("\"{}\" --rebuild-thumbnail-cache", exe.display());
    if let Ok(k) =
        windows_registry::CURRENT_USER.create(r"Software\Microsoft\Windows\CurrentVersion\RunOnce")
    {
        let _ = k.set_string("SageThumbs2KRebuildCache", &cmd);
    }
}

/// `--sync-user-shell` / `--remove-user-shell` (register.rs's per-user shell hooks, run
/// `runasoriginaluser` by the installer so they land in the interactive user's hive rather
/// than the elevated [Code] process's), `--remove-user-state` (uninstall-time: wipe this
/// user's HKCU settings, leave the reinstall tombstone, and drop the daemon's autostart
/// entry), and the deployment-script pair `--export-settings <file>` / `--import-settings
/// <file>` (round-trip the whole settings tree to/from a JSON file headlessly — the same
/// `settings_io` the Diagnostics ▸ Export/Import buttons use, without opening a window).
/// Those two exit the process directly (0 success, 1 failure — a missing path argument is
/// a failure, same as an I/O error) rather than returning, matching `--update-selftest`'s
/// shape in [`dispatch_update_modes`]. Returns `true` if a flag fired (caller should
/// return).
pub(super) unsafe fn dispatch_user_state_modes(args: &[String]) -> bool {
    if args.iter().any(|a| a == "--sync-user-shell") {
        if let Err(e) = sagethumbs2k_core::register::sync_user_shell() {
            sagethumbs2k_core::safety::log(&format!("--sync-user-shell failed: {e}"));
        }
        // The installer runs this as the original user right after the wizard, which is
        // the first moment a copy declared Business can be seen: the evaluation clock
        // starts here, whether or not the user ever opens Settings.
        license::start_trial_if_due();
        return true;
    }
    if args.iter().any(|a| a == "--remove-user-shell") {
        sagethumbs2k_core::register::remove_user_shell();
        return true;
    }
    if args.iter().any(|a| a == "--remove-user-state") {
        remove_user_state();
        return true;
    }
    if args.iter().any(|a| a == "--queue-cache-rebuild") {
        queue_cache_rebuild();
        return true;
    }
    if let Some(pos) = args.iter().position(|a| a == "--export-settings") {
        // Atomic (2026-09-05 audit, F13): a straight `fs::write` here could truncate a
        // prior backup at the same path on a failed overwrite. See
        // `settings_io::export_settings_to_path`.
        let ok = args.get(pos + 1).is_some_and(|path| {
            crate::settings_io::export_settings_to_path(std::path::Path::new(path)).is_ok()
        });
        std::process::exit(if ok { 0 } else { 1 });
    }
    if let Some(pos) = args.iter().position(|a| a == "--import-settings") {
        let ok = args.get(pos + 1).is_some_and(|path| {
            std::fs::read_to_string(path)
                .ok()
                .and_then(|text| crate::settings_io::import_settings(&text).ok())
                .is_some()
        });
        std::process::exit(if ok { 0 } else { 1 });
    }
    false
}

/// The body of `--remove-user-state`: for the CURRENT user only, wipe every trace of us
/// that an elevated, machine-wide uninstall step cannot reach — this app never runs
/// elevated by itself (see [`is_elevated`]'s doc above), so the installer must invoke this
/// `runasoriginaluser` for it to land in the right hive at all.
///
/// Wipes `HKCU\Software\SageThumbs2K` entirely, then re-leaves the reinstall tombstone —
/// the same "wipe the root, then leave one value behind" shape
/// [`sagethumbs2k_core::settings::clear_tombstone`]'s doc comment describes the machine-wide
/// uninstaller performing — via the general string setter (which targets the exact same
/// key `tombstone_version`/`clear_tombstone` read and clear), and drops the screenshot
/// daemon's logon autostart entry. `RUN_KEY`/`RUN_NAME` mirror the private constants in
/// `screenshot::enable` (that module owns the daemon's own add/remove of the same value;
/// its consts aren't `pub`, so the literal is repeated here — keep both in sync).
pub(super) unsafe fn remove_user_state() {
    let _ = windows_registry::CURRENT_USER.remove_tree(sagethumbs2k_core::settings::ROOT);
    let _ = sagethumbs2k_core::settings::set_string("Tombstone", env!("CARGO_PKG_VERSION"));

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_NAME: &str = "SageThumbs2KScreenshot";
    if let Ok(k) = windows_registry::CURRENT_USER.open(RUN_KEY) {
        let _ = k.remove_value(RUN_NAME);
    }
}
