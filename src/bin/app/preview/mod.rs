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
mod font;
#[cfg(feature = "html-preview")]
mod webview;
mod highlight;
mod infocard;
mod docconv;
mod markdown;
mod mdhtml;
mod video;
mod window;
mod loader;
mod paint;
mod shot;
mod toolbar;
mod transport;

use core::ffi::c_void;
use core::sync::atomic::{AtomicU64, Ordering};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError, HINSTANCE, HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, FindWindowW, GetMessageW, SendMessageW, SetForegroundWindow, TranslateMessage,
    MSG, WM_COPYDATA,
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
                let _ = SetForegroundWindow(existing);
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
        lpData: if wide.is_empty() { core::ptr::null_mut() } else { wide.as_ptr() as *mut c_void },
    };
    SendMessageW(hwnd, WM_COPYDATA, Some(WPARAM(0)), Some(LPARAM(&cds as *const _ as isize)));
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
        Some(String::from_utf16_lossy(slice).trim_end_matches('\0').to_string())
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
    /// Pump the message loop for `N` ms before capturing (`--wait-ms N`) — lets async work
    /// (e.g. an opt-in remote markdown image fetch) land in the frame.
    pub wait_ms: Option<u64>,
}

/// The app's `--shot --window preview` mode: build the viewer OFF-SCREEN per `opts`, render it to
/// a PNG at `out` via `PrintWindow`, then tear it down. Returns whether the PNG was written.
pub(crate) unsafe fn run_shot_preview(hinst: HINSTANCE, dark: bool, out: &str, opts: &ShotOpts) -> bool {
    shot::run_shot(hinst, dark, out, opts)
}
