//! Quick preview viewer — the QuickLook-style "press Space, see the file" popup
//! window, dispatched from `main.rs` for `--preview [path]`.
//!
//! This is Phase 1 of the Quick preview plan (`to-do/quick-preview-plan.md`): the
//! standalone viewer, launched by hand. There is NO keyboard hook yet (Phase 2) — the
//! window is driven by `--preview <path>` on the command line and by `WM_COPYDATA`
//! commands (which the Phase 2 daemon hook, and the single-instance forwarder here,
//! use to switch/close a running viewer).
//!
//! Process model (plan §3): a SEPARATE single-instance process, not a window inside the
//! daemon — matches the codebase's "daemon spawns single-purpose helpers" pattern
//! (`--convert`, `--screenshot`), keeps decode crashes away from the hotkey owner (the
//! release profile is `panic=abort`, so a hostile-file decode panic aborts THIS throwaway
//! viewer only; Explorer + the daemon are untouched), and keeps the hook thread free.
//!
//! Submodules: [`window`] owns the window/chrome/toolbar/wndproc; [`content`] owns the
//! budgeted decode worker + the image DIB paint; [`infocard`] is the fallback card.

mod anim;
mod content;
mod dbdoc;
mod docconv;
mod find;
mod font;
mod highlight;
mod infocard;
mod loader;
mod markdown;
mod mdhtml;
mod paint;
mod pdfview;
mod selection;
mod shot;
mod toolbar;
mod transport;
mod video;
#[cfg(feature = "html-preview")]
mod webview;
mod window;
mod woff;

use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_ALREADY_EXISTS, HINSTANCE, HWND, LPARAM, WPARAM,
};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, GetMessageW, SendMessageW, TranslateMessage, MSG, WM_COPYDATA,
};

/// Last time we spawned a `--preview` in response to a Space press (ms tick), or 0. Serializes
/// the FindWindow-then-spawn race: we won't spawn a second viewer while one is still coming up.
static SPAWN_TICK: AtomicU64 = AtomicU64::new(0);

/// Window class of the viewer (NOTE: `"SageThumbs2KPreview"` is TAKEN by
/// `previewhandler.rs`; this is a distinct class — see the plan §7).
pub(super) const VIEWER_CLASS: PCWSTR = w!("SageThumbs2KViewer");

/// Single-instance mutex — mirrors the daemon's mutex-first startup so two `--preview`
/// launches can't both create a window (the loser forwards its path and exits).
const VIEWER_MUTEX: PCWSTR = w!("SageThumbs2K.Viewer.Single");

// WM_COPYDATA command tags (the `dwData` field). The payload, when present, is the
// target file path as UTF-16 (no NUL required; `cbData` bounds it).
/// Switch the viewer to preview the payload path (reuse the window). Always honored.
pub(super) const CMD_SET_PATH: usize = 1;
/// Toggle: if the viewer is showing the payload path, close; else switch to it. Honored
/// only after the open grace window (plan §3) so a key-repeat can't close a fresh window.
pub(super) const CMD_TOGGLE: usize = 2;
/// Close the viewer. Honored only after the open grace window.
pub(super) const CMD_CLOSE: usize = 3;

/// `--preview [path]` entry. Claims the single-instance mutex; if another viewer is
/// already up, forwards the path to it via `WM_COPYDATA` and returns. Otherwise creates
/// the viewer window, kicks off the initial decode, and runs the message loop until close.
pub(crate) unsafe fn run_preview(hinst: HINSTANCE, initial_path: Option<&str>) {
    // Mutex-first single-instance (TOCTOU-safe; mirrors `daemon::run_daemon` +
    // `main`'s `SageThumbs2K.App.Single`). `GetLastError` must be read immediately after
    // `CreateMutexW`, before any other Win32 call clobbers it.
    let mutex = CreateMutexW(None, true, VIEWER_MUTEX);
    if mutex.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS {
        // Another viewer owns the window — hand it the new selection and exit. Retry the
        // FindWindow briefly in case the winner is still mid-create.
        for _ in 0..25 {
            if let Ok(existing) = FindWindowW(VIEWER_CLASS, PCWSTR::null()) {
                if let Some(p) = initial_path {
                    send_command(existing, CMD_SET_PATH, Some(p));
                }
                crate::win::force_foreground(existing);
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(40));
        }
        return; // winner vanished between the mutex check and now — nothing to forward to
    }
    // Held (leaked) for the life of the viewer on purpose — dropping it early would let a
    // third launch create a second window. `_mutex` binds it so it isn't dropped now.
    let _mutex = mutex;

    // Resolve the target: an explicit `--preview <path>` (manual/test), else the foreground
    // Explorer selection (the authoritative hot path — the daemon spawns `--preview` with NO
    // path because COM is forbidden in the hook, so the viewer resolves the selection itself).
    // Both resolutions run on a BUDGETED worker: the IShellWindows automation marshals into
    // explorer.exe (and a `.lnk` resolve can touch a dead network target), so a hung shell
    // would otherwise park this process forever before any window exists.
    let path = match initial_path {
        Some(p) => {
            let raw = p.to_string();
            let for_resolve = raw.clone();
            // On timeout, fall back to the raw path (an unresolved .lnk previews as its card).
            budgeted(move || unsafe { crate::explorer_selection::resolve_explicit(&for_resolve) })
                .unwrap_or(raw)
        }
        None => {
            match budgeted(|| unsafe { crate::explorer_selection::preview_target() }).flatten() {
                Some(p) => p,
                None => return, // nothing selected (or shell hung) → nothing to preview
            }
        }
    };

    let dark = crate::dark::is_dark();
    let Some(_hwnd) = window::create_viewer(hinst, dark, Some(path), None) else {
        return;
    };

    // Standard modal-less pump; `WM_DESTROY` posts `WM_QUIT` which ends this.
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 || r == -1 {
            break;
        }
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
    // `_mutex` drops here, releasing single-instance ownership as the process exits.
}

/// Send a `WM_COPYDATA` command (+ optional path payload) to a viewer window. Uses
/// `SendMessageW` (blocking) because `WM_COPYDATA`'s buffer is only valid for the call.
pub(super) unsafe fn send_command(hwnd: HWND, cmd: usize, path: Option<&str>) {
    let wide = path.map(crate::win::wide).unwrap_or_default();
    let cds = COPYDATASTRUCT {
        dwData: cmd,
        cbData: (wide.len() * 2) as u32,
        lpData: if wide.is_empty() {
            core::ptr::null_mut()
        } else {
            wide.as_ptr() as *mut c_void
        },
    };
    SendMessageW(
        hwnd,
        WM_COPYDATA,
        Some(WPARAM(0)),
        Some(LPARAM(&cds as *const _ as isize)),
    );
}

/// Parse a `WM_COPYDATA` payload back into `(command, path)`. `path` is `None` when the
/// message carried no buffer.
pub(super) unsafe fn parse_command(lparam: LPARAM) -> Option<(usize, Option<String>)> {
    // WM_COPYDATA is receivable from ANY local same-desktop process that knows the viewer's
    // window class, so treat the payload as untrusted: cap the claimed size (a real path is at
    // most ~32K chars) so a bogus/huge `cbData` can't drive a giant allocation. Windows copies
    // the buffer into our address space for the call, so within the cap the read is bounded.
    const MAX_PAYLOAD_BYTES: u32 = 0x10000; // 64 KB = 32K UTF-16 units, > any real path
    let cds = (lparam.0 as *const COPYDATASTRUCT).as_ref()?;
    let path = if (2..=MAX_PAYLOAD_BYTES).contains(&cds.cbData) && !cds.lpData.is_null() {
        let n = (cds.cbData / 2) as usize;
        let slice = core::slice::from_raw_parts(cds.lpData as *const u16, n);
        Some(
            String::from_utf16_lossy(slice)
                .trim_end_matches('\0')
                .to_string(),
        )
    } else {
        None
    };
    Some((cds.dwData, path))
}

/// Run `f` on a detached worker with a 3 s wall-clock budget; `None` on timeout (the worker is
/// abandoned — it sends into a dropped channel and exits, or dies with the process). Guards the
/// startup selection/.lnk resolution against a hung explorer.exe (see `run_preview`).
fn budgeted<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(std::time::Duration::from_secs(3)).ok()
}

/// `--bench-preview <dir>`: measure what a ←/→ step actually costs.
///
/// Runs the viewer's REAL decode entry point (`content::bench_decode_uncached`, the same
/// `read_and_decode` the worker calls) over every previewable image in `dir`, twice:
///
///   * COLD — nothing cached, which is what every arrow-key press used to cost;
///   * WARM — served from the decode cache, which is what a revisit costs now.
///
/// Prints per-file and totals. Deliberately single-threaded and in-process: it is measuring
/// decode + cache, not process startup or window creation, and mixing those in was how
/// "it feels faster" replaced an actual number.
/// Results go to a FILE, not stdout: this EXE is a windows-subsystem binary with no console
/// of its own, so a `println!` here writes to a closed handle. Attaching the parent console
/// would need a new `windows` crate feature on a binary whose size is release-gated — not
/// worth it for a dev tool. Second argument overrides the path.
pub(crate) fn run_bench(dir: &str) {
    let mut out = String::new();
    let dest = std::env::args().nth(3).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("st2k-bench.txt")
            .to_string_lossy()
            .into_owned()
    });
    let flush = |text: &str| {
        let _ = std::fs::write(&dest, text);
    };
    if dir.is_empty() {
        flush("usage: SageThumbs2K.exe --bench-preview <folder> [out.txt]\n");
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        flush(&format!("bench: cannot read folder {dir}\n"));
        return;
    };
    let mut files: Vec<std::path::PathBuf> = rd
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|x| x.to_str())
                .map(|x| sagethumbs2k_core::formats::is_known(&x.to_ascii_lowercase()))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    if files.is_empty() {
        flush(&format!("bench: no decodable files in {dir}\n"));
        return;
    }

    let _ = writeln!(out, "bench: {} files from {dir}", files.len());
    let _ = writeln!(out, "{}\n", load_snapshot());
    let _ = writeln!(
        out,
        "{:<34} {:>10} {:>10} {:>10} {:>11}",
        "file", "cold", "warm", "to-DIB", "wic-scaled"
    );
    let _ = writeln!(out, "{}", "-".repeat(82));

    use std::fmt::Write as _;
    let (mut cold_total, mut warm_total, mut counted) = (0u128, 0u128, 0usize);
    for p in &files {
        let path = p.to_string_lossy().into_owned();
        let name: String = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        let t0 = std::time::Instant::now();
        let cold = content::bench_decode_uncached(&path);
        let cold_us = t0.elapsed().as_micros();
        if cold.is_none() {
            let _ = writeln!(out, "{name:<34} {:>10} {:>10}  (no decode)", "-", "-");
            continue;
        }

        let t1 = std::time::Instant::now();
        let warm = content::bench_decode_cached(&path);
        let warm_us = t1.elapsed().as_micros();
        if warm.is_none() {
            let _ = writeln!(
                out,
                "{name:<34} {:>10} {:>10}  CACHE MISS",
                fmt_us(cold_us),
                "-"
            );
            continue;
        }

        cold_total += cold_us;
        warm_total += warm_us;
        counted += 1;
        let speedup = if warm_us > 0 {
            format!("{:.0}x", cold_us as f64 / warm_us as f64)
        } else {
            ">1000x".to_string()
        };
        let dib = content::bench_make_render(&path)
            .map(fmt_us)
            .unwrap_or_else(|| "-".to_string());
        let scaled = content::bench_scaled_decode(&path)
            .map(fmt_us)
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            out,
            "{name:<34} {:>10} {:>10} {:>10} {:>11}   {speedup}",
            fmt_us(cold_us),
            fmt_us(warm_us),
            dib,
            scaled
        );
    }

    if counted == 0 {
        out.push_str("\nbench: nothing decoded\n");
        flush(&out);
        return;
    }
    let _ = writeln!(out, "{}", "-".repeat(70));
    let _ = writeln!(
        out,
        "{:<34} {:>10} {:>10}  {:.0}x",
        format!("TOTAL ({counted} files)"),
        fmt_us(cold_total),
        fmt_us(warm_total),
        cold_total as f64 / warm_total.max(1) as f64
    );
    let _ = writeln!(
        out,
        "{:<34} {:>10} {:>10}",
        "MEAN per step",
        fmt_us(cold_total / counted as u128),
        fmt_us(warm_total / counted as u128)
    );
    flush(&out);
}

/// `--bench-nav <dir> <steps> [out.txt]`: what a RIGHT-ARROW press actually costs, end to end.
///
/// Unlike [`run_bench`], which times the decode in isolation, this drives the real thing: it
/// builds the real viewer window, posts real `WM_KEYDOWN VK_RIGHT` messages into its real
/// wndproc, and pumps the real message loop until the next file is genuinely installed and
/// painted. That covers the folder listing, the sort, the prefetch, the cache, the worker
/// hand-off and the repaint — everything between the key going down and the picture being up,
/// which is the only number a user experiences.
///
/// The window is created off-screen and hidden (same construction the headless `--shot` uses),
/// `--bench-mash <dir> <keys>`: hold the arrow key down, then time the catch-up.
///
/// `--bench-nav` waits for each step to paint before pressing again, so it never has two
/// decodes in flight and cannot see the cost of work the user has already scrolled past. This
/// posts every keypress at a real key-repeat cadence WITHOUT waiting, then measures from the
/// first press to the moment the LAST file is on screen. That is the number a user feels when
/// they lean on the key, and the only one that shows whether abandoning superseded work helps.
///
/// Set `ST2K_NO_CANCEL=1` to measure the same binary with abandonment switched off.
pub(crate) fn run_mash_bench(hinst: HINSTANCE, dir: &str, keys: usize) {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_RIGHT;
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN};

    let dest = std::env::args().nth(4).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("st2k-mashbench.txt")
            .to_string_lossy()
            .into_owned()
    });
    let flush = |text: &str| {
        let _ = std::fs::write(&dest, text);
    };

    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|x| x.to_str())
                    .map(|x| window::navigate::is_previewable_ext(&x.to_ascii_lowercase()))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            flush(&format!("bench-mash: cannot read {dir}: {e}\n"));
            return;
        }
    };
    if files.len() < 2 {
        flush("bench-mash: need at least 2 previewable files\n");
        return;
    }
    files = window::navigate::sort_paths_like_explorer(files);
    let first = files[0].to_string_lossy().into_owned();
    let hwnd = match unsafe { window::create_viewer(hinst, false, Some(first.clone()), None) } {
        Some(h) => h,
        None => {
            flush("bench-mash: could not create the viewer window\n");
            return;
        }
    };
    let _ = wait_until_loaded(hwnd, &first, 15_000);

    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "bench-mash: {} files in {dir}", files.len());
    let _ = writeln!(
        out,
        "cancellation: {}",
        if std::env::var_os("ST2K_NO_CANCEL").is_some() {
            "OFF (ST2K_NO_CANCEL)"
        } else {
            "on"
        }
    );
    let _ = writeln!(out, "{}\n", load_snapshot());

    // ~30 ms between presses is a normal Windows key-repeat rate once the initial delay has
    // elapsed. Faster than any decode, which is the whole point.
    const REPEAT_MS: u64 = 30;
    let target = files[keys % files.len()].to_string_lossy().into_owned();
    let name = files[keys % files.len()]
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    let t0 = std::time::Instant::now();
    let mut last_press = t0;
    for _ in 0..keys {
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_KEYDOWN,
                windows::Win32::Foundation::WPARAM(VK_RIGHT.0 as usize),
                windows::Win32::Foundation::LPARAM(0),
            )
        };
        // Pump so the window actually processes the press, the way a real key repeat arrives
        // into a running message loop rather than all at once into a full queue.
        last_press = std::time::Instant::now();
        let deadline = last_press + std::time::Duration::from_millis(REPEAT_MS);
        while std::time::Instant::now() < deadline {
            pump_once(hwnd);
        }
    }
    let landed = wait_until_loaded(hwnd, &target, 30_000);
    let us = t0.elapsed().as_micros();
    // The CATCH-UP is the number that matters. Total time includes `keys * REPEAT_MS` of
    // pressing, a fixed floor that swamps everything; what abandoning superseded work can
    // change is only how long the viewer takes to settle AFTER the user stops.
    let tail_us = last_press.elapsed().as_micros();

    let _ = writeln!(out, "{keys} keypresses at {REPEAT_MS} ms, target {name}");
    let _ = writeln!(
        out,
        "first keypress -> final file painted: {}",
        if landed {
            fmt_us(us)
        } else {
            "TIMEOUT".to_string()
        }
    );
    let _ = writeln!(
        out,
        "workers abandoned: {}   <- work not done for files already scrolled past",
        content::bench_abandoned_count()
    );
    let _ = writeln!(
        out,
        "LAST keypress -> final file painted: {}   <- the catch-up",
        if landed {
            fmt_us(tail_us)
        } else {
            "TIMEOUT".to_string()
        }
    );
    flush(&out);
}

/// so this needs no desktop, steals no focus, and runs unattended.
pub(crate) fn run_nav_bench(hinst: HINSTANCE, dir: &str, steps: usize) {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_RIGHT;
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_KEYDOWN};

    let mut out = String::new();
    let dest = std::env::args().nth(4).unwrap_or_else(|| {
        std::env::temp_dir()
            .join("st2k-navbench.txt")
            .to_string_lossy()
            .into_owned()
    });
    let flush = |text: &str| {
        let _ = std::fs::write(&dest, text);
    };

    let mut files: Vec<std::path::PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
            .map(|e| e.path())
            .filter(|p| {
                p.extension()
                    .and_then(|x| x.to_str())
                    .map(|x| window::navigate::is_previewable_ext(&x.to_ascii_lowercase()))
                    .unwrap_or(false)
            })
            .collect(),
        Err(e) => {
            flush(&format!("bench-nav: cannot read {dir}: {e}\n"));
            return;
        }
    };
    if files.len() < 2 {
        flush(&format!(
            "bench-nav: need at least 2 previewable files in {dir}\n"
        ));
        return;
    }
    // The viewer walks the folder in Explorer's logical order, so expectations must too —
    // otherwise "where did it land" would compare against the wrong sequence.
    files = window::navigate::sort_paths_like_explorer(files);
    let first = files[0].to_string_lossy().into_owned();

    let hwnd = match unsafe { window::create_viewer(hinst, false, Some(first.clone()), None) } {
        Some(h) => h,
        None => {
            flush("bench-nav: could not create the viewer window\n");
            return;
        }
    };
    // Settle the initial load so the first measured step starts from a painted window rather
    // than from whatever the constructor left mid-flight.
    let _ = wait_until_loaded(hwnd, &first, 15_000);

    use std::fmt::Write as _;
    let _ = writeln!(out, "bench-nav: {} files in {dir}", files.len());
    let _ = writeln!(out, "{}\n", load_snapshot());
    let _ = writeln!(
        out,
        "{:<6} {:<34} {:>12}",
        "step", "landed on", "keypress->painted"
    );
    let _ = writeln!(out, "{}", "-".repeat(60));

    let (mut total, mut worst, mut counted) = (0u128, 0u128, 0usize);
    let mut timeouts = 0usize;
    for step in 1..=steps {
        let expected = files[step % files.len()].to_string_lossy().into_owned();
        let name = files[step % files.len()]
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let t0 = std::time::Instant::now();
        // The REAL key path: the wndproc's WM_KEYDOWN arm decides this is navigation.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_KEYDOWN,
                windows::Win32::Foundation::WPARAM(VK_RIGHT.0 as usize),
                windows::Win32::Foundation::LPARAM(0),
            )
        };
        let landed = wait_until_loaded(hwnd, &expected, 15_000);
        let us = t0.elapsed().as_micros();
        if landed {
            total += us;
            worst = worst.max(us);
            counted += 1;
            let _ = writeln!(out, "{step:<6} {name:<34} {:>12}", fmt_us(us));
        } else {
            timeouts += 1;
            let _ = writeln!(out, "{step:<6} {name:<34} {:>12}", "TIMEOUT");
        }
    }
    unsafe { window::request_close(hwnd) };

    let _ = writeln!(out, "{}", "-".repeat(60));
    if counted > 0 {
        let _ = writeln!(
            out,
            "{:<41} {:>12}",
            format!("MEAN of {counted} steps"),
            fmt_us(total / counted as u128)
        );
        let _ = writeln!(out, "{:<41} {:>12}", "WORST step", fmt_us(worst));
    }
    let _ = writeln!(out, "{:<41} {:>12}", "timeouts", timeouts);
    flush(&out);
}

/// Pump the viewer's message loop until it has fully loaded `expected`, or `budget_ms` passes.
///
/// "Loaded" is deliberately BOTH conditions: the state's path is the file we asked for AND the
/// kind has left `Loading`. Checking only the path would stop the clock when navigation began
/// rather than when the picture arrived, which would measure nothing worth measuring.
/// Drain whatever is in the queue right now, once. Shared by the mash bench, which has to keep
/// the window alive BETWEEN keypresses rather than only after the last one.
fn pump_once(_hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    unsafe {
        let mut msg = MSG::default();
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn wait_until_loaded(hwnd: HWND, expected: &str, budget_ms: u64) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(budget_ms);
    loop {
        // Drain the queue DIRECTLY rather than via `win::pump_msgs`, which sleeps 16 ms per
        // "frame" — it exists to let a headless screenshot settle, not to time anything. Using
        // it here put a 128 ms floor under every measured step and made a 400x300 JPEG look
        // exactly as slow as a 12 MP one, which is what gave the game away.
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        // The window can die under us (a decode abort, a close, anything), and `state()` then
        // hands back a NULL pointer that `&*` would happily dereference -- an access
        // violation, which is exactly how this harness crashed at 14 steps while passing at 8.
        // A dead window is a legitimate outcome to report, not to fault on.
        if !unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(hwnd)) }.as_bool() {
            return false;
        }
        let sp = unsafe { window::state(hwnd) };
        if sp.is_null() {
            return false;
        }
        let st = unsafe { &*sp };
        let here = st.path.borrow().clone().unwrap_or_default();
        if here.eq_ignore_ascii_case(expected) && st.kind.get() != window::ContentKind::Loading {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        // Sub-millisecond, so the poll granularity is noise next to a real step.
        std::thread::sleep(std::time::Duration::from_micros(200));
    }
}

/// System load at the moment of measuring, stamped into every bench report.
///
/// A timing without this is not a measurement, it is an anecdote: the same bench on this
/// machine swung from 315 ms to 604 ms per step purely because something else was using the
/// CPU. Sampling it here means a stored report can be trusted (or discarded) later without
/// anyone having to remember what the box was doing.
fn load_snapshot() -> String {
    // Deliberately NOT a CPU-percentage reading: that needs another `windows` crate feature
    // on a binary whose size is release-gated, and a percentage is the wrong number anyway.
    // What a benchmark cares about is how much CPU this process can actually GET, so time a
    // FIXED unit of work on the spot. It degrades exactly when the machine is busy, which is
    // what makes two reports comparable — or visibly not — without anyone having to remember
    // what else was running an hour ago. (This session saw the same bench swing 315 ms to
    // 604 ms per step on a box that was pegged at 100% by unrelated work.)
    let t0 = std::time::Instant::now();
    let mut acc = 0u64;
    for i in 0..40_000_000u64 {
        acc = acc.wrapping_add(i ^ (acc >> 7));
    }
    let us = t0.elapsed().as_micros();
    std::hint::black_box(acc);
    format!(
        "calibration: a fixed CPU workload took {} here (lower = quieter machine; \
         compare runs only when these agree)",
        fmt_us(us)
    )
}

fn fmt_us(us: u128) -> String {
    if us >= 1_000_000 {
        format!("{:.2} s", us as f64 / 1_000_000.0)
    } else if us >= 1_000 {
        format!("{:.1} ms", us as f64 / 1_000.0)
    } else {
        format!("{us} us")
    }
}

/// Spawn a fresh detached instance of ourselves with `args` (launches the viewer + the Info
/// dialog).
pub(super) fn spawn_self(args: &[&str]) {
    if let Ok(exe) = std::env::current_exe() {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new(exe)
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
            .spawn();
    }
}

/// Daemon-side "Space pressed" handler (posted from the hook): if a viewer is up, close it
/// (toggle off); otherwise spawn one (toggle on) — serialized so a fast double-press can't open
/// two windows. Called on the DAEMON thread, off the LL-hook callback (FindWindow / spawn /
/// WM_COPYDATA must not run inside the hook).
pub(crate) unsafe fn request_toggle() {
    if let Ok(hwnd) = FindWindowW(VIEWER_CLASS, PCWSTR::null()) {
        SPAWN_TICK.store(0, Ordering::Relaxed);
        send_command(hwnd, CMD_TOGGLE, None);
        return;
    }
    let now = GetTickCount64();
    let last = SPAWN_TICK.load(Ordering::Relaxed);
    if last != 0 && now.saturating_sub(last) < 2000 {
        return; // a spawn is already in flight; the viewer's mutex-first startup guards any race
    }
    SPAWN_TICK.store(now, Ordering::Relaxed);
    spawn_self(&["--preview"]);
}

/// Daemon-side "Esc / Enter / hold-to-peek" handler: close the viewer if it's up.
pub(crate) unsafe fn request_close() {
    if let Ok(hwnd) = FindWindowW(VIEWER_CLASS, PCWSTR::null()) {
        send_command(hwnd, CMD_CLOSE, None);
    }
}

/// Headless `--shot --window preview` options. Lets the off-screen capture force the runtime-only
/// states that a plain still can't show — so verifying the toolbar hover, a pinned window, a PDF's
/// page pager, an animation frame, or the video transport strip never needs to drive the desktop.
#[derive(Default)]
pub(crate) struct ShotOpts {
    /// File to preview (`--file`); a synthetic gradient is used when `None`.
    pub file: Option<String>,
    /// Force toolbar button index `N` hovered (`--hot N`).
    pub hot: Option<usize>,
    /// Open pinned — filled pin glyph + topmost (`--pinned`).
    pub pinned: bool,
    /// For a PDF, render page `N` (0-based) and populate the pager/count (`--pdf-page N`).
    pub pdf_page: Option<u32>,
    /// For an animated GIF/APNG/WebP, show frame `N` (`--frame N`).
    pub frame: Option<usize>,
    /// For a video, load the engine + pump to first-frame so the transport strip renders
    /// (`--play`). The video surface itself is a swap chain PrintWindow can't read, so it stays
    /// black; the strip (GDI) captures fine.
    pub play: bool,
    /// Force the layout/font DPI (`--dpi N`, e.g. 192 for 200%) so a high-DPI render can be
    /// captured off-screen without a physical high-DPI monitor. `None`/0 uses the real DPI.
    pub dpi: Option<i32>,
    /// Scroll the text/Markdown pane down `N` device px before capturing (`--scroll N`) —
    /// lets a long document's middle/bottom be shot-verified headlessly.
    pub scroll: Option<i32>,
    /// `--wheel N [--ctrl|--shift]`: post N REAL `WM_MOUSEWHEEL` notches into the viewer's own
    /// window procedure before capturing. Distinct from `--scroll`, which calls the scroll
    /// function directly: 2.3.1 shipped a PDF whose wheel did nothing precisely because every
    /// test bypassed the message that a user actually sends. Negative scrolls down, matching
    /// the sign Windows uses.
    pub wheel: Option<i32>,
    pub wheel_ctrl: bool,
    pub wheel_shift: bool,
    /// Force a text-pane selection over the raw byte range `A..B` before capturing
    /// (`--sel A,B`) — lets the selection highlight be shot-verified headlessly.
    pub sel: Option<(usize, usize)>,
    /// Open the find bar on this query and jump to its first match (`--find <text>`), so the bar,
    /// the pane shrinking around it and the match highlight are all shot-verifiable.
    pub find: Option<String>,
    /// Pump the message loop for `N` ms before capturing (`--wait-ms N`) — lets async work
    /// (e.g. an opt-in remote markdown image fetch) land in the frame.
    pub wait_ms: Option<u64>,
    /// Open in "view source" mode (`--source`) — the raw text of a file that would normally
    /// RENDER (Markdown, CSV/TSV/notebook, SVG), as the caption's `{ }` toggle shows it.
    pub source: bool,
    /// PRESS the view-source toolbar button once after loading (`--toggle-source`), driving the
    /// real click path (`do_action` → `toggle_source` → reload) rather than presetting the mode
    /// like `--source` does. Combine with `--source` to verify toggling back to rendered.
    pub toggle_source: bool,
    /// RESIZE the window to `W`x`H` after loading (`--size WxH`), exactly as dragging the frame
    /// does (a real `SetWindowPos` → `WM_SIZE`), then capture. This is how "does the content
    /// re-flow when the window gets wider" is verified headlessly instead of on the desktop.
    pub size: Option<(i32, i32)>,
}

/// The app's `--shot --window preview` mode: build the viewer OFF-SCREEN per `opts`, render it to
/// a PNG at `out` via `PrintWindow`, then tear it down. Returns whether the PNG was written.
pub(crate) unsafe fn run_shot_preview(
    hinst: HINSTANCE,
    dark: bool,
    out: &str,
    opts: &ShotOpts,
) -> bool {
    shot::run_shot(hinst, dark, out, opts)
}
