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
mod sponsors;
mod convert;
mod cred_store;
mod dark;
mod explorer_selection;
mod eyedropper;
mod files_to_folder;
mod gdip;
mod hotkey;
mod http;
mod image_info;
mod oauth;
mod preview;
mod screenshot;
mod sync_client;
mod upload_result;
mod settings_dlg;
mod settings_io;
mod tags_to_folders;
mod update;
mod win;

use core::ffi::c_void;

use windows::core::w;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX, ICC_LINK_CLASS,
    ICC_STANDARD_CLASSES, ICC_BAR_CLASSES, ICC_PROGRESS_CLASS,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use sagethumbs2k_core::i18n;

use crate::convert::run_convert_dialog;
use crate::dark::{dark_bg_brush, dark_control, dark_titlebar, init_dark_app, is_dark};
use crate::eyedropper::run_eyedropper;
use crate::files_to_folder::run_files_to_folder_dialog;
use crate::tags_to_folders::run_tags_to_folders_dialog;
use crate::win::app_icon;

/// Is this process running with an ELEVATED (admin) token? The installer's post-install
/// [Run] steps carry `runasoriginaluser`, but when Setup itself was launched pre-elevated
/// — which is exactly how the SELF-UPDATE launches it (`ShellExecuteW("runas")`) — Inno
/// has no original non-elevated token and falls back to running them ELEVATED. A hotkey
/// daemon spawned from that context inherits the elevation and is then UIPI-deaf to the
/// non-elevated Settings window's `WM_RELOAD` forever (hotkey changes silently stop
/// applying), and every capture helper it spawns runs as admin too.
unsafe fn is_elevated() -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    let mut tok = windows::Win32::Foundation::HANDLE::default();
    if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok).is_err() {
        return false;
    }
    let mut e = TOKEN_ELEVATION::default();
    let mut len = 0u32;
    let ok = GetTokenInformation(
        tok,
        TokenElevation,
        Some(&mut e as *mut _ as *mut core::ffi::c_void),
        core::mem::size_of::<TOKEN_ELEVATION>() as u32,
        &mut len,
    )
    .is_ok();
    let _ = CloseHandle(tok);
    ok && e.TokenIsElevated != 0
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
        if let Some(pos) = args.iter().position(|a| a == "--convert") {
            if let Some(listfile) = args.get(pos + 1) {
                run_convert_dialog(hinst, listfile);
            }
            return;
        }
        // Headless GIF asset: `--shot-gif <out.gif>` walks every Settings tab and encodes an
        // animated (infinite-loop) GIF — the regenerable README/site walkthrough of the
        // Settings window. Invisible (off-screen); exits 0/1. Checked before `--shot` (exact
        // match, so the shorter flag never swallows it).
        if let Some(pos) = args.iter().position(|a| a == "--shot-gif") {
            let ok = args.get(pos + 1).is_some_and(|out| settings_dlg::run_shot_gif(hinst, dark, out));
            std::process::exit(i32::from(!ok));
        }
        // Headless self-capture (verification + README/site assets):
        //   `--shot <out.png> [--tab N] [--window settings|convert|eyedropper]`
        // builds the chosen window INVISIBLY (off-screen), renders it to a PNG, exits 0/1.
        // `--tab N` (0-based) selects the Settings category (default 0); ignored for the
        // other windows. No window ever appears and the desktop is never driven.
        if let Some(pos) = args.iter().position(|a| a == "--shot") {
            let window = args
                .iter()
                .position(|a| a == "--window")
                .and_then(|p| args.get(p + 1))
                .map(String::as_str)
                .unwrap_or("settings");
            let ok = if let Some(out) = args.get(pos + 1) {
                match window {
                    "convert" => crate::convert::run_shot_convert(out),
                    "eyedropper" => crate::eyedropper::run_shot_eyedropper(out),
                    "preview" => {
                        // `--file <path>` input (synthetic gradient if absent), plus optional
                        // headless state forcing: `--hot N` (button N hovered), `--pinned`,
                        // `--pdf-page N`, `--frame N` (animation frame), `--play` (video strip).
                        let val = |name: &str| {
                            args.iter().position(|a| a == name).and_then(|p| args.get(p + 1))
                        };
                        let opts = crate::preview::ShotOpts {
                            file: val("--file").cloned(),
                            hot: val("--hot").and_then(|s| s.parse().ok()),
                            pinned: args.iter().any(|a| a == "--pinned"),
                            pdf_page: val("--pdf-page").and_then(|s| s.parse().ok()),
                            frame: val("--frame").and_then(|s| s.parse().ok()),
                            play: args.iter().any(|a| a == "--play"),
                            dpi: val("--dpi").and_then(|s| s.parse().ok()),
                            scroll: val("--scroll").and_then(|s| s.parse().ok()),
                            sel: val("--sel").and_then(|s| {
                                let (a, b) = s.split_once(',')?;
                                Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
                            }),
                            wait_ms: val("--wait-ms").and_then(|s| s.parse().ok()),
                        };
                        crate::preview::run_shot_preview(hinst, dark, out, &opts)
                    }
                    _ => {
                        let tab = args
                            .iter()
                            .position(|a| a == "--tab")
                            .and_then(|p| args.get(p + 1))
                            .and_then(|s| s.parse::<usize>().ok())
                            .unwrap_or(0);
                        settings_dlg::run_shot(hinst, dark, out, tab)
                    }
                }
            } else {
                false
            };
            std::process::exit(i32::from(!ok));
        }
        // Eyedropper mode: `--eyedropper` (spawned by the DLL verb) opens the
        // system-wide screen color picker.
        if args.iter().any(|a| a == "--eyedropper") {
            run_eyedropper(hinst);
            return;
        }
        // Image info: `--image-info <path>` (spawned by the DLL's Image info verb) shows
        // a verbose, copyable metadata dump for the file.
        if let Some(pos) = args.iter().position(|a| a == "--image-info") {
            if let Some(path) = args.get(pos + 1) {
                image_info::run_image_info(path);
            }
            return;
        }
        // Quick preview: `--preview [path]` launches the single-instance QuickLook-style
        // viewer. With no daemon hook yet (Phase 2), it's driven by the path arg + WM_COPYDATA
        // commands. A second launch forwards its path to the running viewer and exits.
        if let Some(pos) = args.iter().position(|a| a == "--preview") {
            let path = args.get(pos + 1).filter(|p| !p.starts_with("--")).map(String::as_str);
            crate::preview::run_preview(hinst, path);
            return;
        }
        // Instant capture: `--screenshot-instant` grabs the whole screen straight to
        // the clipboard + a PNG, no overlay — the optional "quick-save" hotkey's
        // action. Checked before `--screenshot` (exact match, so they don't overlap).
        if args.iter().any(|a| a == "--screenshot-instant") {
            crate::screenshot::capture_instant();
            return;
        }
        // Screenshot mode: `--screenshot` opens the Flameshot-style capture +
        // annotation overlay (region → draw → copy/save). Wired to a hotkey by the
        // opt-in tray daemon; runnable directly for testing.
        if args.iter().any(|a| a == "--screenshot") {
            crate::screenshot::run_capture(hinst);
            return;
        }
        // Screenshot daemon: `--screenshot-daemon` runs the opt-in tray helper that
        // registers the global hotkey and spawns captures. Launched at logon (HKCU
        // autostart) only after the user enables it in Settings.
        if args.iter().any(|a| a == "--screenshot-daemon") {
            crate::screenshot::run_daemon(hinst);
            return;
        }
        // Screenshot watchdog: `--screenshot-watchdog` runs the tiny supervisor that
        // restarts the daemon if it ever dies while still wanted. Spawned by the daemon
        // itself; kept a separate mode so it survives a daemon crash.
        if args.iter().any(|a| a == "--screenshot-watchdog") {
            crate::screenshot::run_watchdog(hinst);
            return;
        }
        // Custom action hotkey: `--hotkey-action` (spawned by the daemon when the user's
        // assigned chord fires) runs whichever action they bound in Settings ▸ Screenshots —
        // colour picker, screenshot, or a file verb over the Explorer selection / a picker.
        if args.iter().any(|a| a == "--hotkey-action") {
            crate::hotkey::run_hotkey_action(hinst);
            return;
        }
        // Upload mode: `--upload <png>` POSTs a capture to a keyless host and copies
        // the URL to the clipboard (spawned by the capture overlay's Upload button).
        if let Some(pos) = args.iter().position(|a| a == "--upload") {
            if let Some(path) = args.get(pos + 1) {
                crate::screenshot::run_upload(path);
            }
            return;
        }
        // Upload-keep mode: `--upload-keep <listfile>` uploads the USER files listed
        // (the DLL's right-click "Upload" verb) to the keyless host and copies the
        // link(s) to the clipboard — WITHOUT deleting the originals (only `--upload`
        // deletes, since its file is a throwaway capture). Exact-match above means
        // `--upload` never swallows this longer flag.
        if let Some(pos) = args.iter().position(|a| a == "--upload-keep") {
            if let Some(listfile) = args.get(pos + 1) {
                crate::screenshot::run_upload_keep(listfile);
            }
            return;
        }
        // Toggle the screenshot hotkey on/off (HKCU autostart + the tray daemon).
        // The Settings checkbox will drive this via screenshot::set_enabled; exposed
        // here so it's usable/testable without the UI.
        if args.iter().any(|a| a == "--screenshot-toggle") {
            crate::screenshot::set_enabled(!crate::screenshot::is_enabled());
            return;
        }
        // Files-to-folder mode: `--files-to-folder <listfile>` (spawned by the DLL
        // verb for a multi-file selection) prompts for a folder name, then moves.
        if let Some(pos) = args.iter().position(|a| a == "--files-to-folder") {
            if let Some(listfile) = args.get(pos + 1) {
                run_files_to_folder_dialog(hinst, listfile);
            }
            return;
        }
        // Tags-to-folders mode: `--tags-to-folders <listfile>` (spawned by the DLL
        // verb) sorts audio files into folders by their tags.
        if let Some(pos) = args.iter().position(|a| a == "--tags-to-folders") {
            if let Some(listfile) = args.get(pos + 1) {
                run_tags_to_folders_dialog(hinst, listfile);
            }
            return;
        }
        // Self-update confirmation: `--updated <ver>` is launched by the installer's [Run]
        // step right after a SILENT self-update finishes — pop a NON-BLOCKING tray toast
        // ("you're now on <ver>"), then exit. No modal dialog, so the update stays silent.
        // The installer gates this relaunch on its own /UPDATED marker, so a normal
        // interactive install never triggers it.
        if let Some(pos) = args.iter().position(|a| a == "--updated") {
            // The installer force-closed the resident hotkey daemon (Restart Manager /
            // PrepareToInstall) to replace this very EXE — and NOTHING else restarts it
            // before the next logon. Heal it FIRST, so the hotkeys the toast implicitly
            // claims are working actually are. No-op when the feature is off.
            heal_after_install();
            let ver = args.get(pos + 1).map_or(env!("CARGO_PKG_VERSION"), String::as_str);
            crate::update::show_updated_toast(ver);
            return;
        }
        // Silent self-heal: `--heal-hotkeys` (run by the installer after EVERY install —
        // including manual/silent reinstalls that never pass /UPDATED) restarts the hotkey
        // daemon if it's wanted but not running. No UI; exits immediately.
        if args.iter().any(|a| a == "--heal-hotkeys") {
            heal_after_install();
            return;
        }

        // Opening the Settings window is the natural moment to self-heal the hotkey service:
        // if it's enabled (or a custom hotkey is bound) but not running — e.g. it and its
        // watchdog were both killed, or a prior logon never brought it up — restart it now so
        // the user doesn't have to click "Restart". No-op when it's already running / not wanted.
        crate::screenshot::heal_if_wanted();

        // Single instance, TOCTOU-safe: same pattern as the screenshot daemon
        // (`screenshot::daemon::run_daemon`) — claim a named mutex FIRST, since the
        // FindWindow check alone races (two Start Menu double-clicks can both pass it
        // before either has created a window). Held (leaked) for the life of the
        // Settings window on purpose; dropping it early would let a third launch in.
        let single_instance = CreateMutexW(None, true, w!("SageThumbs2K.App.Single"));
        if single_instance.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS {
            // Another instance is already up (or mid-boot) — activate ITS window
            // instead of opening a second one. The window may not exist yet if the
            // other instance is still initializing, so retry briefly before giving up.
            for _ in 0..50 {
                if let Ok(existing) = FindWindowW(w!("SageThumbs2KOptions"), None) {
                    if IsIconic(existing).as_bool() {
                        let _ = ShowWindow(existing, SW_RESTORE);
                    }
                    let _ = SetForegroundWindow(existing);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            // Never appeared — the other instance likely exited between the mutex
            // check and now. Exit quietly rather than fight over the window class.
            return;
        }

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

        // WS_THICKFRAME lets the user drag the window TALLER; the dialog's
        // WM_GETMINMAXINFO locks the width + a minimum height, and WM_SIZE reflows the
        // bottom-anchored controls (right list / scrollbar / fold-mask / footer) so the
        // left options get a bigger scroll viewport.
        //
        // WS_CLIPCHILDREN: the left options are real child controls that the scroll
        // path slides with SetWindowPos + a full-band invalidate each tick. Without it,
        // the parent's background erase paints INTO the child rects before they repaint,
        // which flashes on a fast scroll. Clipping the children out of the parent's paint
        // — paired with the double-buffered WM_PAINT + WM_ERASEBKGND no-op in
        // `settings_dlg` — makes each scroll frame atomic (no erase-then-paint flicker).
        // v3 layout is a fixed-size nav-rail + content-pane shell (no scrolling
        // column), so the window is NOT user-resizable — drop WS_THICKFRAME.
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN;
        // The control layout is in 96-DPI design pixels, scaled per control by `ctl()`
        // (`GetDpiForWindow`); the window frame must scale to the SAME DPI or the
        // fixed-size v3 shell clips its controls. Size AND position the window for the
        // monitor under the cursor — the one it opens on — so the frame DPI matches the
        // controls' DPI on mixed-DPI multi-monitor setups and after a post-login scale
        // change. (The old `dpi_for_system()` + CW_USEDEFAULT used the LOGIN primary DPI,
        // which mismatched the actual monitor and clipped the toggles / fields / footer.)
        let (mon_dpi, work) = win::cursor_monitor_metrics();
        // v3 nav-rail + content-pane shell: fixed 772×588 (96-dpi design), DPI-scaled.
        let win_w = win::dpi_scale_dpi(772, mon_dpi);
        let win_h = win::dpi_scale_dpi(588, mon_dpi);
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

        let _ = ShowWindow(hwnd, SW_SHOW);

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
}
