//! SageThumbs 2K — Options.
//!
//! A native Win32 settings window (a faithful, modernized port of the original
//! SageThumbs Options dialog) that edits HKCU\Software\SageThumbs2K via the
//! crate's `settings` module, plus a per-format checkbox list. It is also the
//! `Application` entry the sparse package needs.
//!
//! Built programmatically (CreateWindowExW) rather than from a dialog-template
//! resource: the layout is computed and DPI-scaled at runtime (no .rc dialog
//! template to keep in sync), a faithful match to how the original was laid out.
//! (Aside: build.rs *does* run `windres` for the icon/version resource — it just
//! compiles into OUT_DIR, which sidesteps the spaces in this project's path.)
//!
//! Reachable settings take effect immediately (the provider reads them per
//! request). Changing the per-format list rewrites the HKCR `shellex` keys,
//! which needs elevation — handled by re-running `regsvr32` (which honors the
//! per-extension flags we just wrote) elevated, exactly as the original did.
//!
//! This file is the facade / entry point. The UI is split into submodules:
//! `win` (shared Win32 primitives), `dark` (dark mode), `sponsors` (the remote
//! banner), `settings_dlg` (the main window), `about`, `convert`,
//! `files_to_folder`, `tags_to_folders`, and `eyedropper`.
// `not(test)`: under `cargo test` we need the console subsystem so the harness can
// print results; the shipped binary stays a GUI ("windows") subsystem app.
#![cfg_attr(not(test), windows_subsystem = "windows")]
#![allow(non_snake_case)]

mod about;
mod convert;
mod cred_store;
mod dark;
mod dialog_hook;
mod doctor_report;
mod explorer_selection;
mod eyedropper;
mod feedback;
mod files_to_folder;
mod first_run;
mod gdip;
mod hotkey;
mod http;
mod image_info;
mod license;
/// The "you could be signed in" prompt: app glue (persistence, identity, the decision).
mod nudge;
/// The shared LunarWerx decision engine, vendored VERBATIM. Never edit it here — see `nudge.rs`.
///
/// `dead_code` is allowed for exactly that reason: this is a byte-for-byte copy of
/// `packages/connections-connect/ports/nudge.rs` and it carries the whole API every LunarWerx app
/// might use. SageThumbs uses a subset. Trimming the rest would make the copy stop matching
/// upstream, which is the one property that lets a `diff` prove this app has not drifted into
/// asking differently from the others.
#[allow(dead_code, reason = "verbatim vendored copy; see the module doc above")]
mod nudge_engine;
mod oauth;
mod ocr_result;
mod prebuild_dlg;
mod preview;
mod screenshot;
mod settings_dlg;
mod settings_io;
mod sponsors;
mod sync_client;
mod tags_to_folders;
mod update;
mod upload_result;
mod win;

use core::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_BAR_CLASSES, ICC_LINK_CLASS, ICC_LISTVIEW_CLASSES,
    ICC_PROGRESS_CLASS, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use sagethumbs2k_core::i18n;

use crate::convert::run_convert_dialog;
use crate::dark::{dark_bg_brush, dark_control, dark_titlebar, init_dark_app, is_dark};
use crate::eyedropper::run_eyedropper;
use crate::files_to_folder::run_files_to_folder_dialog;
use crate::tags_to_folders::run_tags_to_folders_dialog;
use crate::win::{app_icon, t};

/// Is this process running with an ELEVATED (admin) token? The installer's post-install
/// [Run] steps carry `runasoriginaluser`, but when Setup itself was launched pre-elevated
/// — which is exactly how the SELF-UPDATE launches it (`ShellExecuteW("runas")`) — Inno
/// has no original non-elevated token and falls back to running them ELEVATED. A hotkey
/// daemon spawned from that context inherits the elevation and is then UIPI-deaf to the
/// non-elevated Settings window's `WM_RELOAD` forever (hotkey changes silently stop
/// applying), and every capture helper it spawns runs as admin too.
///
/// The token check itself lives in the core crate (`prebuild::is_elevated`), which needs the
/// same answer for a different reason: the thumbnail cache is per user, so an elevated
/// pre-build would fill the administrator's cache instead. One implementation, two callers.
unsafe fn is_elevated() -> bool {
    sagethumbs2k_core::prebuild::is_elevated()
}

/// De-elevate the heal through a ONE-SHOT `LIMITED` scheduled task: the task starts
/// `--heal-hotkeys` with the interactive user's NORMAL token (`/rl LIMITED` strips the
/// admin half even for an admin account), and that non-elevated instance takes the plain
/// `heal_if_wanted` path below. A scheduled task needs no running Explorer — which is
/// down at this exact moment (Restart Manager only restarts it AFTER the [Run] section) —
/// so the shell-token de-elevation trick would not work here. Best-effort with logging;
/// on any schtasks failure fall back to healing elevated (worse than a clean heal, but
/// far better than leaving the hotkeys dead until next logon).
fn schedule_unelevated_heal() {
    use std::os::windows::process::CommandExt;
    const TASK: &str = "SageThumbs2K_HealHotkeys";
    let Ok(exe) = std::env::current_exe() else {
        crate::screenshot::heal_if_wanted();
        return;
    };
    let tr = format!("\"{}\" --heal-hotkeys", exe.display());
    let run = |args: &[&str]| {
        std::process::Command::new("schtasks.exe")
            .args(args)
            .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
            .output()
    };
    // `/sc once /st 00:00` only satisfies schtasks' mandatory-schedule syntax — the task
    // is fired immediately via `/run` and removed right after.
    #[rustfmt::skip]
    let created = run(&["/create", "/f", "/tn", TASK, "/sc", "once", "/st", "00:00",
                        "/rl", "LIMITED", "/tr", &tr]);
    match created {
        Ok(o) if o.status.success() => {
            let _ = run(&["/run", "/tn", TASK]);
            // Give Task Scheduler a moment to actually start the process before the
            // task definition disappears out from under it.
            std::thread::sleep(std::time::Duration::from_secs(2));
            let _ = run(&["/delete", "/f", "/tn", TASK]);
        }
        Ok(o) => {
            sagethumbs2k_core::safety::log(&format!(
                "heal: schtasks create failed ({}): {} — healing elevated instead",
                o.status,
                String::from_utf8_lossy(&o.stderr).trim()
            ));
            crate::screenshot::heal_if_wanted();
        }
        Err(e) => {
            sagethumbs2k_core::safety::log(&format!(
                "heal: schtasks unavailable ({e}) — healing elevated instead"
            ));
            crate::screenshot::heal_if_wanted();
        }
    }
}

/// The install-time heal (`--heal-hotkeys` / `--updated`): restart the hotkey daemon the
/// installer had to kill — WITHOUT letting it inherit an elevated token (see
/// [`is_elevated`]). Elevated → reroute through the LIMITED scheduled task; normal → heal
/// directly. No-op when the feature is off.
fn heal_after_install() {
    if unsafe { is_elevated() } {
        schedule_unelevated_heal();
    } else {
        crate::screenshot::heal_if_wanted();
    }
}

/// Should this launch opportunistically fire the throttled update check? Everything the
/// user can reach — Settings, the Convert dialog, Quick preview, the right-click verbs —
/// says yes. The exclusions are the modes where a spawned child or a tray balloon would be
/// wrong: the headless `--shot*` captures (must stay deterministic and side-effect free),
/// the automation route (synthetic pixels only, by contract), the install-time one-shots
/// (the installer drives its own check), and the resident daemon (it owns a 6 h timer of
/// its own). `--update-check` itself is excluded so it can never re-spawn itself.
fn update_piggyback_wanted(args: &[String]) -> bool {
    const EXCLUDED: &[&str] = &[
        "--shot",
        "--shot-gif",
        "--screenshot-automation",
        "--screenshot-daemon",
        "--update-check",
        "--update-task",
        "--update-selftest",
        "--first-run-seen",
        "--updated",
        "--heal-hotkeys",
        // Restarts Explorer and exits; piggybacking an update check onto that would leave a
        // network call running out of a process whose whole job was one `cmd` line.
        "--rebuild-thumbnail-cache",
        "--rebuild-thumbnail-cache-now",
        // The hidden dev measurement flags: console-output-only, no window, no side effects —
        // a network call spawned mid-benchmark would be a stray side effect in a mode whose
        // whole point is a clean, repeatable number.
        "--bench-preview",
        "--bench-nav",
        "--bench-mash",
        // Documented as read-only and side-effect-free ("prints what a global hotkey would
        // act on right now... and exits"); it must stay that way even under `--after-ms`.
        "--explorer-selection",
        // Install/uninstall-time one-shots, same reasoning as `--heal-hotkeys`/`--updated`.
        "--sync-user-shell",
        "--remove-user-shell",
        "--remove-user-state",
        "--queue-cache-rebuild",
        // Deployment-script one-shots: console-output-only (an exit code), no window, and
        // (`--export-settings`) must never trigger a network call as a side effect of
        // something a provisioning script runs unattended.
        "--export-settings",
        "--import-settings",
    ];
    !args.iter().any(|a| EXCLUDED.contains(&a.as_str()))
}

fn main() {
    // Capture panics to the diagnostics log before the process aborts (panic=abort).
    sagethumbs2k_core::safety::install_panic_hook("app");
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();

        // Resolve the UI language (HKCU override or system) before any control
        // is created so the dialog opens already localized.
        i18n::ensure_init();

        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES
                | ICC_LINK_CLASS
                | ICC_STANDARD_CLASSES
                | ICC_BAR_CLASSES
                | ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);

        let dark = is_dark();
        if dark {
            init_dark_app();
        }

        // Convert… mode: `--convert <listfile>` (spawned by the DLL verb) shows
        // the batch-convert dialog instead of the Options window.
        let args: Vec<String> = std::env::args().collect();

        // Piggyback the update check on ANY ordinary launch of this app. The periodic check
        // otherwise lives only in the OPT-IN resident screenshot helper, so an install where
        // the user never enabled screenshots gets no update checks at all — the reason
        // "the auto-updater isn't working" reports arrived from people the owner couldn't
        // reproduce. Costs one cached file read unless the once-a-day throttle has expired,
        // in which case it spawns the detached `--update-check` one-shot and returns; this
        // process never waits on the network and needn't outlive the toast.
        if update_piggyback_wanted(&args) {
            crate::update::spawn_due_check();
        }

        // Every headless / one-shot CLI mode, checked in the same relative order the
        // original single if-chain used (a flag's precedence over another matters when an
        // invocation names more than one), just split into topical dispatchers so each one
        // stays small. Each returns `true` when it already handled the launch (including
        // via `std::process::exit`, for the modes that always exit rather than fall
        // through), meaning this process should return without ever building a window.
        if dispatch_diagnostic_modes(hinst, &args) {
            return;
        }
        if dispatch_update_modes(&args) {
            return;
        }
        if dispatch_convert_and_shot_modes(hinst, dark, &args) {
            return;
        }
        if dispatch_file_and_capture_modes(hinst, &args) {
            return;
        }
        if dispatch_screenshot_modes(hinst, &args) {
            return;
        }
        if dispatch_folder_modes(hinst, &args) {
            return;
        }
        if dispatch_heal_modes(&args) {
            return;
        }
        // Optional postinstall step (a checkbox on setup's last page): restart Explorer and
        // drop thumbcache_*.db.
        //
        // A fresh install genuinely needs this, which is not obvious. Registering the provider
        // does not invalidate anything Explorer already cached, and for every one of our
        // formats it HAS cached something: the generic icon it drew before we existed. Those
        // entries keep being served, so the user installs a thumbnailer, sees no thumbnails,
        // and concludes it is broken. Same mechanism the FormatBadge toggle already clears the
        // cache for.
        //
        // Reuses the exact string the "Rebuild thumbnail cache" / "Repair file associations"
        // buttons use, through `cmd_c`, so the kill-then-relaunch stays one `cmd` line and
        // cannot repeat issue #5 (Explorer killed, relaunch mis-quoted, user left with no
        // shell). Never silent-by-default: setup only runs this if the box is ticked.
        if args.iter().any(|a| a == "--rebuild-thumbnail-cache") {
            // `restart_explorer_clearing_cache` can take up to ~30s (it waits out the
            // taskkill, then polls for the taskbar to come back). This flag is invoked
            // synchronously from the installer's postinstall [Run] step, which by default
            // blocks Setup's own UI for however long we take — so detach: re-spawn ourselves
            // with the actual work and return immediately, rather than making the installer
            // (or whatever else launched us this way) sit through it.
            detach_rebuild_thumbnail_cache();
            return;
        }
        // The detached half of the above: does the real (blocking) work and exits. Not
        // reachable from a normal launch — only from the re-spawn just above.
        if args.iter().any(|a| a == "--rebuild-thumbnail-cache-now") {
            let _ = sagethumbs2k_core::shellcmd::restart_explorer_clearing_cache();
            return;
        }
        if dispatch_user_state_modes(&args) {
            return;
        }

        // FIRST RUN: offer Quick preview + the screenshot hotkey once, before Settings
        // appears. This is the launch the installer's postinstall step performs, so it is
        // the one moment a brand-new user is reliably looking at the app. No-op afterwards.
        // Placed after every headless/one-shot mode has returned, so only a real, visible
        // launch can ever raise it.
        crate::first_run::show_if_first_run();

        // Opening the Settings window is the natural moment to self-heal the hotkey service:
        // if it's enabled (or a custom hotkey is bound) but not running — e.g. it was
        // killed, or a prior logon never brought it up — restart it now so
        // the user doesn't have to click "Restart". No-op when it's already running / not wanted.
        crate::screenshot::heal_if_wanted();

        // `--tab N` on the NORMAL launch, not just inside `--shot`: the Quick preview viewer's
        // caption gear opens Settings straight on the Quick preview page. It used to be parsed
        // only by the headless capture path, so passing it to a real launch silently opened
        // page 0 (CLAUDE.md SS6.1.1 records a probe that was fooled by exactly that).
        let want_tab = wanted_tab(&args);

        if handle_single_instance(want_tab) {
            return;
        }

        let hwnd = create_and_show_settings_window(hinst, dark, want_tab);
        run_message_loop(hwnd);
    }
}

/// Hidden, side-effect-free UI integration route (`--screenshot-automation`) plus the
/// hidden dev measurement flags (`--bench-preview` / `--bench-nav` / `--bench-mash`).
/// Checked first: the automation route takes precedence over every other output-capable
/// mode even in a malformed mixed invocation, preserving its privacy/safety contract —
/// synthetic full-screen pixels only, with clipboard, file, dialog, and upload paths
/// fenced inside the overlay. Returns `true` if a flag fired (caller should return).
unsafe fn dispatch_diagnostic_modes(hinst: HINSTANCE, args: &[String]) -> bool {
    if args.iter().any(|a| a == "--screenshot-automation") {
        crate::screenshot::run_capture_automation(hinst);
        return true;
    }
    // `--bench-preview <dir>`: times the Quick preview's REAL decode path over a folder —
    // a cold pass, then a warm pass off the cache — so the arrow-key stepping cost is a
    // number rather than an impression. Console output, no window, no side effects.
    if let Some(pos) = args.iter().position(|a| a == "--bench-preview") {
        let dir = args.get(pos + 1).cloned().unwrap_or_default();
        crate::preview::run_bench(&dir);
        return true;
    }
    // `--bench-nav <dir> <steps>`: the same measurement one level up — real viewer window,
    // real WM_KEYDOWN arrow presses, timed from keypress to painted.
    if let Some(pos) = args.iter().position(|a| a == "--bench-nav") {
        let dir = args.get(pos + 1).cloned().unwrap_or_default();
        let steps = args
            .get(pos + 2)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(20);
        crate::preview::run_nav_bench(hinst, &dir, steps);
        return true;
    }
    // `--bench-mash <dir> <keys>`: the HELD arrow key, pressed without waiting for each
    // paint, so several decodes really are in flight at once. `ST2K_NO_CANCEL=1` switches
    // abandonment off for an A/B on the same binary.
    if let Some(pos) = args.iter().position(|a| a == "--bench-mash") {
        let dir = args.get(pos + 1).cloned().unwrap_or_default();
        let keys = args
            .get(pos + 2)
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(20);
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
unsafe fn dispatch_update_modes(args: &[String]) -> bool {
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
/// hovered), `--pinned`, `--pdf-page N`, `--frame N` (animation frame), `--play` (video
/// strip), `--source` (raw text of a normally-rendered file), and the rest.
fn build_shot_preview_opts(args: &[String]) -> crate::preview::ShotOpts {
    let val = |name: &str| {
        args.iter()
            .position(|a| a == name)
            .and_then(|p| args.get(p + 1))
    };
    crate::preview::ShotOpts {
        file: val("--file").cloned(),
        hot: val("--hot").and_then(|s| s.parse().ok()),
        pinned: args.iter().any(|a| a == "--pinned"),
        pdf_page: val("--pdf-page").and_then(|s| s.parse().ok()),
        frame: val("--frame").and_then(|s| s.parse().ok()),
        play: args.iter().any(|a| a == "--play"),
        dpi: val("--dpi").and_then(|s| s.parse().ok()),
        scroll: val("--scroll").and_then(|s| s.parse().ok()),
        wheel: val("--wheel").and_then(|s| s.parse().ok()),
        wheel_ctrl: args.iter().any(|a| a == "--ctrl"),
        wheel_shift: args.iter().any(|a| a == "--shift"),
        sel: val("--sel").and_then(|s| {
            let (a, b) = s.split_once(',')?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        }),
        find: val("--find").cloned(),
        wait_ms: val("--wait-ms").and_then(|s| s.parse().ok()),
        source: args.iter().any(|a| a == "--source"),
        toggle_source: args.iter().any(|a| a == "--toggle-source"),
        toggle_theme: args.iter().any(|a| a == "--toggle-theme"),
        size: val("--size").and_then(|s| {
            let (w, h) = s.split_once(['x', 'X'])?;
            Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
        }),
    }
}

/// The default (`settings`) window of `--shot`: builds the requested tab (or drives the
/// settings-wide search headlessly when `--search <needle>` is present, optionally picking
/// the first hit with a trailing `!`) and renders it.
unsafe fn run_shot_settings_window(
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
unsafe fn run_shot_mode(hinst: HINSTANCE, dark: bool, args: &[String], pos: usize) -> bool {
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
unsafe fn dispatch_convert_and_shot_modes(hinst: HINSTANCE, dark: bool, args: &[String]) -> bool {
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
unsafe fn dispatch_file_and_capture_modes(hinst: HINSTANCE, args: &[String]) -> bool {
    // `--explorer-selection` prints what a global hotkey would act on right now — one path
    // per line, nothing when there is no selection — and exits. This exists because "I
    // pressed the hotkey and nothing happened" is otherwise unanswerable: it separates "the
    // hotkey never fired" from "the hotkey fired but Explorer reported no selection".
    if args.iter().any(|a| a == "--explorer-selection") {
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
        if let Some(p) = explorer_selection::preview_target() {
            println!("{p}");
        }
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
        if let Some(dir) = args.get(pos + 1) {
            // A DRIVE ROOT arrives here as `E:"` — see `prebuild::unmangle_shell_path` for
            // why the shell's own quoting does that and why it cannot be fixed in the
            // registry string. Repairing it here also heals installs that already wrote
            // the old command.
            let dir = sagethumbs2k_core::prebuild::unmangle_shell_path(dir);
            prebuild_dlg::run_prebuild(&dir);
        }
        return true;
    }
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
    // Quick preview: `--preview [path]` launches the single-instance QuickLook-style
    // viewer. A second launch forwards its path to the running viewer and exits.
    if let Some(pos) = args.iter().position(|a| a == "--preview") {
        let path = args
            .get(pos + 1)
            .filter(|p| !p.starts_with("--"))
            .map(String::as_str);
        crate::preview::run_preview(hinst, path);
        return true;
    }
    false
}

/// The screenshot-related CLI flags: `--screenshot-instant`, `--screenshot-ocr`,
/// `--screenshot`, `--screenshot-daemon`, `--hotkey-action`, `--upload <png>`,
/// `--upload-keep <listfile>` and `--screenshot-toggle`. Returns `true` if a flag fired
/// (caller should return).
unsafe fn dispatch_screenshot_modes(hinst: HINSTANCE, args: &[String]) -> bool {
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
    // swallows this longer flag.
    if let Some(pos) = args.iter().position(|a| a == "--upload-keep") {
        if let Some(listfile) = args.get(pos + 1) {
            crate::screenshot::run_upload_keep(listfile);
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

/// `--files-to-folder <listfile>` and `--tags-to-folders <listfile>` (both spawned by DLL
/// verbs over a multi-file selection). Returns `true` if a flag fired (caller should
/// return).
unsafe fn dispatch_folder_modes(hinst: HINSTANCE, args: &[String]) -> bool {
    if let Some(pos) = args.iter().position(|a| a == "--files-to-folder") {
        if let Some(listfile) = args.get(pos + 1) {
            run_files_to_folder_dialog(hinst, listfile);
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
unsafe fn dispatch_heal_modes(args: &[String]) -> bool {
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
fn detach_rebuild_thumbnail_cache() {
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
unsafe fn offer_thumbnail_refresh(ver: &str) {
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
fn queue_cache_rebuild() {
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
unsafe fn dispatch_user_state_modes(args: &[String]) -> bool {
    if args.iter().any(|a| a == "--sync-user-shell") {
        if let Err(e) = sagethumbs2k_core::register::sync_user_shell() {
            sagethumbs2k_core::safety::log(&format!("--sync-user-shell failed: {e}"));
        }
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
        let ok = args.get(pos + 1).is_some_and(|path| {
            std::fs::write(path, crate::settings_io::export_settings()).is_ok()
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
unsafe fn remove_user_state() {
    let _ = windows_registry::CURRENT_USER.remove_tree(sagethumbs2k_core::settings::ROOT);
    let _ = sagethumbs2k_core::settings::set_string("Tombstone", env!("CARGO_PKG_VERSION"));

    const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const RUN_NAME: &str = "SageThumbs2KScreenshot";
    if let Ok(k) = windows_registry::CURRENT_USER.open(RUN_KEY) {
        let _ = k.remove_value(RUN_NAME);
    }
}

/// If another instance is already up (or mid-boot), activate ITS window instead of
/// opening a second one, forwarding `want_tab` — the same WM_COMMAND its own nav item
/// sends, so the two paths cannot diverge. Retries briefly since the window may not exist
/// yet if the other instance is still initializing. Returns `true` in every case (the
/// caller must not build a new window): whether it raised the existing window, or gave up
/// because the other instance likely exited between the mutex check and now.
unsafe fn activate_existing_instance(want_tab: Option<usize>) -> bool {
    for _ in 0..50 {
        if let Ok(existing) = FindWindowW(w!("SageThumbs2KOptions"), None) {
            if IsIconic(existing).as_bool() {
                let _ = ShowWindow(existing, SW_RESTORE);
            }
            // If the foreground grab is refused the window stays hidden behind whatever
            // is in front, and the menu item reads as dead.
            crate::win::force_foreground(existing);
            if let Some(tab) = want_tab {
                let _ = PostMessageW(
                    Some(existing),
                    WM_COMMAND,
                    WPARAM(
                        ((STN_CLICKED as usize) << 16) | (settings_dlg::NAV_ID_BASE as usize + tab),
                    ),
                    LPARAM(0),
                );
            }
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    true
}

/// Single instance, TOCTOU-safe: same pattern as the screenshot daemon
/// (`screenshot::daemon::run_daemon`) — claim a named mutex FIRST, since the FindWindow
/// check alone races (two Start Menu double-clicks can both pass it before either has
/// created a window). The mutex handle is intentionally leaked (never closed) for the
/// life of the process; dropping it early would let a third launch in, and `HANDLE` has no
/// `Drop` impl to fight, so returning from this function changes nothing about that.
/// Returns `true` if another instance already held the mutex (caller should return
/// without creating a window).
unsafe fn handle_single_instance(want_tab: Option<usize>) -> bool {
    // Owner-only DACL: another account's process on the same machine cannot open the mutex
    // (the default descriptor is used only when this process's token cannot be read). The
    // helper also captures `GetLastError` right after the create, before anything clobbers it.
    let (mutex, last_err) = win::create_mutex_user_only(true, w!("SageThumbs2K.App.Single"));
    match mutex {
        Ok(_) if last_err == ERROR_ALREADY_EXISTS => {
            activate_existing_instance(want_tab);
            true
        }
        // We now hold the mutex ourselves: genuinely the first (and only) instance.
        Ok(_) => false,
        // CreateMutexW itself failed — we CANNOT establish whether another instance holds
        // it. The old code treated `single_instance.is_ok()` being false the exact same way
        // as "definitely no other instance" (silently falling through to open a second
        // window), which is the bug: a failure here means "unknown", not "no". Log it (this
        // used to be silent) and proceed best-effort rather than either claim.
        Err(e) => {
            sagethumbs2k_core::safety::log(&format!(
                "single-instance mutex could not be created/opened ({e}) — cannot verify \
                 whether another instance is already running"
            ));
            false
        }
    }
}

/// Registers the window class, sizes and positions it for the monitor under the cursor,
/// creates it, applies dark mode, lands on the requested tab, and shows it. Isolated out
/// of `main` because it is the one contiguous "build the real window" concern, distinct
/// from the CLI-mode dispatch above it and the message loop after it.
unsafe fn create_and_show_settings_window(
    hinst: HINSTANCE,
    dark: bool,
    want_tab: Option<usize>,
) -> HWND {
    let class = w!("SageThumbs2KOptions");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(settings_dlg::wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        // Dark window background when the system is dark; otherwise the
        // classic button-face system color ((COLOR_BTNFACE + 1) as HBRUSH).
        hbrBackground: if dark {
            dark_bg_brush()
        } else {
            HBRUSH(16isize as *mut c_void)
        },
        ..Default::default()
    };
    RegisterClassW(&wc);

    // HISTORICAL (v2, before the nav-rail): WS_THICKFRAME let the user drag the window
    // TALLER; that machinery still exists in `settings_dlg::mod::on_resize` (harmless — it
    // just never runs without WS_THICKFRAME to trigger it) but is no longer wired to a
    // resizable frame. v3 layout is a fixed-size nav-rail + content-pane shell (no
    // scrolling column), so the window is NOT user-resizable — drop WS_THICKFRAME.
    //
    // WS_CLIPCHILDREN: the left options are real child controls that the scroll path
    // slides with SetWindowPos + a full-band invalidate each tick. Without it, the
    // parent's background erase paints INTO the child rects before they repaint, which
    // flashes on a fast scroll.
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
    // The control layout is in 96-DPI design pixels, scaled per control by `ctl()`
    // (`GetDpiForWindow`); the window frame must scale to the SAME DPI or the fixed-size
    // v3 shell clips its controls. Size AND position the window for the monitor under the
    // cursor — the one it opens on — so the frame DPI matches the controls' DPI on
    // mixed-DPI multi-monitor setups and after a post-login scale change.
    let (mon_dpi, work) = win::cursor_monitor_metrics();
    // Count this opening of the Settings window and let the sign-in engine decide whether
    // this is one of the rare moments it speaks up. It has to happen BEFORE the window is
    // created, because the answer changes how tall the window is: the banner sits in a
    // strip between the pane and the footer, and the page layout below it runs once and
    // cannot be re-run. See `settings_dlg::nudge`.
    nudge::start_session();
    // Licence posture, decided once as Settings opens (the natural "the user is looking at
    // us" moment). `current_posture()` is called first for its own sake — it logs the
    // decision for support threads, same as before — then `snapshot()` reads the same local
    // state again for the extra fields the notices below need (the key prefix); both are
    // cheap local-only reads (no network), so a second one costs nothing and keeps
    // `current_posture`'s documented logging behaviour intact rather than duplicating it.
    let _ = license::current_posture();
    let license_snap = license::snapshot();
    let nudge_strip = if settings_dlg::decide_sign_in_nudge() {
        settings_dlg::sign_in_nudge_height()
    } else {
        0
    };
    // The Business-nag reminder strip reserves its own room the same way, next to (not
    // instead of) the sign-in one — see `settings_dlg::decide_business_nag`'s doc comment.
    let biznag_strip = if settings_dlg::decide_business_nag() {
        settings_dlg::business_nag_height()
    } else {
        0
    };

    // v3 nav-rail + content-pane shell: fixed 772×588 (96-dpi design), DPI-scaled.
    let win_w = win::dpi_scale_dpi(772, mon_dpi);
    let win_h = win::dpi_scale_dpi(588 + nudge_strip + biznag_strip, mon_dpi);
    let x = work.left + ((work.right - work.left) - win_w).max(0) / 2;
    let y = work.top + ((work.bottom - work.top) - win_h).max(0) / 2;
    let hwnd = CreateWindowExW(
        WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
        class,
        w!("SageThumbs 2K — Settings"),
        style,
        x,
        y,
        win_w,
        win_h,
        None,
        None,
        Some(hinst),
        None,
    )
    .expect("create window");

    if dark {
        dark_control(hwnd, w!("DarkMode_Explorer"));
        dark_titlebar(hwnd);
    }

    // The one-shot licensing notices: the persistent nag banner (Business, unlicensed) is
    // page chrome the window already carries; these are the notices that speak up ONCE, the
    // moment the window is about to appear. `Silent` (Personal, or a licensed Business
    // install) says nothing, ever — see `license.rs`'s standing "fail toward Personal/free"
    // rule.
    match license_snap.posture {
        license::Posture::Silent => {}
        license::Posture::BusinessNag => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if license::nag_due(now) {
                // The toast pumps its own message loop for as long as it lingers; on its
                // own thread it cannot hold up the Settings window being built here.
                std::thread::spawn(|| {
                    crate::win::notify_toast(
                        "SageThumbs 2K",
                        t("licence_nag_toast_body"),
                        std::time::Duration::from_secs(6),
                    );
                });
                license::note_nag_shown(now);
            }
        }
        license::Posture::DowngradeNoticeOnce => {
            crate::win::message_box(
                hwnd,
                &t("licence_downgrade_notice").replace("{key}", &license_snap.key_prefix),
                "SageThumbs 2K",
            );
            license::acknowledge_downgrade();
        }
        license::Posture::DeauthorizedLoud => {
            // Every Settings open until re-licensed — this one has no acknowledgement to
            // record, unlike the downgrade notice above.
            crate::win::message_box(
                hwnd,
                &t("licence_deauthorized_notice").replace("{key}", &license_snap.key_prefix),
                "SageThumbs 2K",
            );
        }
    }

    // Land on the requested page before the window is shown, so it never flashes General
    // first. The layout builder ends with `switch_category(hwnd, 0)`; this re-selects.
    if let Some(tab) = want_tab {
        settings_dlg::show_category(hwnd, tab);
    }

    let _ = ShowWindow(hwnd, SW_SHOW);
    hwnd
}

/// The classic Win32 message pump, run until `WM_QUIT`.
unsafe fn run_message_loop(hwnd: HWND) {
    let mut msg = MSG::default();
    loop {
        // GetMessageW returns -1 on error, 0 on WM_QUIT, >0 otherwise.
        // as_bool() (`!= 0`) would treat -1 as "keep going" and then spin on
        // a MSG it never populated — branch on the raw value instead.
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 || r == -1 {
            break;
        }
        if !IsDialogMessageW(hwnd, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// The Settings page `--tab N` asks for, or `None` when the flag is absent, malformed, or names
/// a page that does not exist. Out-of-range is deliberately `None` rather than clamped: a number
/// past the end means the caller's idea of the page list disagrees with this build's, and
/// silently landing on the last page would hide that.
fn wanted_tab(args: &[String]) -> Option<usize> {
    args.iter()
        .position(|a| a == "--tab")
        .and_then(|p| args.get(p + 1))
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|&t| t < settings_dlg::NAV_CATEGORY_COUNT)
}

#[cfg(test)]
mod tests {
    use super::{update_piggyback_wanted, wanted_tab};

    fn argv(rest: &[&str]) -> Vec<String> {
        std::iter::once("SageThumbs2K.exe".to_string())
            .chain(rest.iter().map(|s| (*s).to_string()))
            .collect()
    }

    #[test]
    fn piggyback_covers_ordinary_launches_and_spares_the_headless_ones() {
        // The whole point of the piggyback: an install where the resident helper was never
        // enabled still gets update checks, from whatever the user actually opens.
        for ordinary in [
            vec![],
            vec!["--convert", "list.txt"],
            vec!["--preview", "a.png"],
            vec!["--eyedropper"],
            vec!["--files-to-folder", "list.txt"],
        ] {
            assert!(
                update_piggyback_wanted(&argv(&ordinary)),
                "{ordinary:?} should fire the update check"
            );
        }

        // Headless captures must stay deterministic and side-effect free, the automation
        // route has a synthetic-pixels-only contract, the daemon owns its own timer, and the
        // one-shot must never spawn itself.
        for excluded in [
            vec!["--shot", "out.png"],
            vec!["--shot", "out.png", "--window", "preview"],
            vec!["--shot-gif", "out.gif"],
            vec!["--screenshot-automation"],
            vec!["--screenshot-daemon"],
            vec!["--update-check"],
            vec!["--update-task"],
            vec!["--update-task", "remove"],
            vec!["--update-selftest", "setup.exe"],
            vec!["--updated", "1.7.0"],
            vec!["--heal-hotkeys"],
            vec!["--rebuild-thumbnail-cache"],
            vec!["--rebuild-thumbnail-cache-now"],
            vec!["--bench-preview", "C:\\pics"],
            vec!["--bench-nav", "C:\\pics", "20"],
            vec!["--bench-mash", "C:\\pics", "20"],
            vec!["--explorer-selection"],
            vec!["--explorer-selection", "--after-ms", "300"],
            vec!["--sync-user-shell"],
            vec!["--remove-user-shell"],
            vec!["--remove-user-state"],
            vec!["--export-settings", "out.json"],
            vec!["--import-settings", "in.json"],
        ] {
            assert!(
                !update_piggyback_wanted(&argv(&excluded)),
                "{excluded:?} must NOT fire the update check"
            );
        }
    }

    /// `--tab N` backs the Quick preview caption's Settings gear, so a launch that carries it
    /// has to land on that page. It was parsed only inside `--shot` until 2026-08-24, which
    /// meant a normal launch silently opened page 0 and a live probe reported a clean pass
    /// against a control that did not exist.
    #[test]
    fn tab_flag_selects_a_real_settings_page() {
        let quick = crate::settings_dlg::quick_preview_page();
        assert_eq!(
            wanted_tab(&argv(&["--tab", &quick.to_string()])),
            Some(quick)
        );
        assert_eq!(wanted_tab(&argv(&["--tab", "0"])), Some(0));
        // Absent, malformed, or with nothing after it: open normally, never panic.
        assert_eq!(wanted_tab(&argv(&[])), None);
        assert_eq!(wanted_tab(&argv(&["--tab"])), None);
        assert_eq!(wanted_tab(&argv(&["--tab", "not-a-number"])), None);
        assert_eq!(wanted_tab(&argv(&["--tab", "-1"])), None);
        // Past the end is REFUSED rather than clamped: it means the caller's page list and
        // this build's disagree, and quietly opening the last page would hide that.
        let past_end = crate::settings_dlg::NAV_CATEGORY_COUNT;
        assert_eq!(wanted_tab(&argv(&["--tab", &past_end.to_string()])), None);
    }
}
