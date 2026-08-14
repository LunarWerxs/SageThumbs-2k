//! `--prebuild <folder>`: the window behind the folder right-click entry.
//!
//! The engine is [`sagethumbs2k_core::prebuild`]; this is only a face for it, because the
//! CLI's console output is no use to someone who right-clicked a folder in Explorer.
//!
//! It runs the walk on a worker thread and posts progress back, rather than pumping messages
//! from inside the work: `prebuild::run` blocks for as long as the folder takes, and doing
//! that on the UI thread would give Explorer a white unresponsive rectangle for minutes.
//!
//! Cancel sets a flag the engine checks per file. That is a real stop, not a kill: the pool
//! keeps visiting the remaining items and skipping them, which costs microseconds and avoids
//! tearing down COM apartments mid-call.

use core::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::{dark_ctlcolor, dark_ctlcolor_dim};
use crate::win::{ctl, dpi_scale, run_dialog, t, wm_dpichanged, BUTTON, STATIC};

const ID_PATH: i32 = 200;
const ID_BAR: i32 = 201;
const ID_STATUS: i32 = 202;
const ID_ACTION: i32 = 203;

/// `WM_APP + n`. The worker owns no window state; it only posts these.
const WM_PB_PROGRESS: u32 = WM_APP + 1;
const WM_PB_DONE: u32 = WM_APP + 2;

/// Progress-bar messages. Spelled out rather than pulled from `Win32_UI_Controls` so this
/// module adds no crate feature for two integers.
const PBM_SETPOS: u32 = 0x0402;
const PBM_SETRANGE32: u32 = 0x0406;
const PROGRESS_CLASS: PCWSTR = w!("msctls_progress32");

const DLG_W: i32 = 420;
const DLG_H: i32 = 190;

/// Set by Cancel, read by the engine between files.
static CANCEL: AtomicBool = AtomicBool::new(false);
/// The run is over and the button now means "close".
///
/// Tracked separately rather than inferred from [`RESULT`], because `WM_PB_DONE` TAKES the
/// summary out to render it — so a "is RESULT populated?" test reads false immediately after
/// the run finishes, and the button silently stopped closing the window. That is exactly how
/// a finished dialog ended up stuck on screen with a Close button that did nothing.
static FINISHED: AtomicBool = AtomicBool::new(false);
/// Finished counts, handed from the worker to `WM_PB_DONE`.
static RESULT: Mutex<Option<Summary>> = Mutex::new(None);
/// Last posted (done, total), so a repaint can restate them without asking the worker.
static SEEN: (AtomicUsize, AtomicUsize) = (AtomicUsize::new(0), AtomicUsize::new(0));
/// The folder to work on, handed from [`run_prebuild`] to `WM_CREATE`.
///
/// `run_dialog` runs the message pump ITSELF and does not return until the window closes, so
/// there is no "after the window exists" moment on the calling side to build controls or start
/// the worker in — both have to happen inside the wndproc, and this is how the path gets there.
static FOLDER: Mutex<String> = Mutex::new(String::new());

struct Summary {
    built: usize,
    already: usize,
    failed: usize,
    offline: usize,
    cancelled: bool,
}

/// Compose "12 built · 30 already there · 2 failed" from the three translated nouns.
///
/// Deliberately number-then-noun with a separator rather than a sentence with placeholders:
/// the i18n layer has no substitution, and inventing one for a status line would put a
/// format-string parser between 36 translators and a window that must never fail to render.
fn summary_text(s: &Summary) -> String {
    let mut parts = vec![
        format!("{} {}", s.built, t("pb_built")),
        format!("{} {}", s.already, t("pb_cached")),
    ];
    if s.failed > 0 {
        parts.push(format!("{} {}", s.failed, t("pb_failed")));
    }
    if s.offline > 0 {
        parts.push(format!("{} {}", s.offline, t("pb_offline")));
    }
    let body = parts.join("  ·  ");
    if s.cancelled {
        format!("{}  ({})", body, t("pb_stopped"))
    } else {
        body
    }
}

/// Shorten a path from the LEFT for display: the tail names the folder, which is the part
/// that identifies it, so a long path loses its middle rather than its meaning.
fn elide(path: &str, max: usize) -> String {
    let n = path.chars().count();
    if n <= max {
        return path.to_string();
    }
    let tail: String = path.chars().skip(n.saturating_sub(max - 1)).collect();
    format!("…{tail}")
}

unsafe fn build(hwnd: HWND, hinst: HINSTANCE, folder: &str) {
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    let unit = dpi_scale(hwnd, 100).max(1);
    let cw = (rc.right - rc.left) * 100 / unit;
    let ch = (rc.bottom - rc.top) * 100 / unit;
    let m = 20;
    let w = cw - m * 2;

    ctl(
        hwnd,
        STATIC,
        &elide(folder, 58),
        WINDOW_STYLE(0),
        m,
        18,
        w,
        20,
        ID_PATH,
        hinst,
    );

    // The bar is created here but left at 0..0; the first progress post gives it a real
    // range. A folder's file count is not known until the walk finishes, and inventing one
    // would make the bar jump backwards when the truth arrived.
    let bar = ctl(
        hwnd,
        PROGRESS_CLASS,
        "",
        WINDOW_STYLE(0),
        m,
        50,
        w,
        18,
        ID_BAR,
        hinst,
    );
    let _ = SendMessageW(bar, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(1)));

    ctl(
        hwnd,
        STATIC,
        t("pb_scanning"),
        WINDOW_STYLE(0),
        m,
        80,
        w,
        36,
        ID_STATUS,
        hinst,
    );

    let (bw, bh) = (110, 30);
    ctl(
        hwnd,
        BUTTON,
        t("pb_cancel"),
        WINDOW_STYLE(BS_PUSHBUTTON as u32) | WS_TABSTOP,
        cw - m - bw,
        ch - bh - 16,
        bw,
        bh,
        ID_ACTION,
        hinst,
    );
}

unsafe fn set_text(hwnd: HWND, id: i32, text: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        let t = crate::win::wide(text);
        let _ = SetWindowTextW(h, PCWSTR(t.as_ptr()));
    }
}

/// Kick the engine off on its own thread. `hwnd` is only ever used with `PostMessageW`,
/// which is the one window API safe to call across threads.
fn spawn_worker(hwnd: HWND, folder: String) {
    let raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let opts = sagethumbs2k_core::prebuild::Options {
            recurse: true,
            ..Default::default()
        };
        // The progress callback runs on the POOL's worker threads, so it must be Sync — and
        // `HWND` wraps a raw pointer, which is not. Carry the handle as an integer and rebuild
        // it per call; `PostMessageW` is the one window API that is safe across threads anyway.
        let rep = sagethumbs2k_core::prebuild::run(
            &[folder],
            &opts,
            Some(&CANCEL),
            |done, total| unsafe {
                let _ = PostMessageW(
                    Some(HWND(raw as *mut c_void)),
                    WM_PB_PROGRESS,
                    WPARAM(done),
                    LPARAM(total as isize),
                );
            },
        );
        let hwnd = HWND(raw as *mut c_void);
        *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = Some(Summary {
            built: rep.built,
            already: rep.already,
            failed: rep.failed,
            offline: rep.skipped_offline,
            cancelled: rep.cancelled,
        });
        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_PB_DONE, WPARAM(0), LPARAM(0));
        }
    });
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_CREATE => {
            let hinst: HINSTANCE = match GetModuleHandleW(None) {
                Ok(h) => h.into(),
                Err(_) => return LRESULT(-1),
            };
            let folder = FOLDER
                .lock()
                .map(|f| f.clone())
                .unwrap_or_else(|e| e.into_inner().clone());
            build(hwnd, hinst, &folder);
            // Start the work only once the controls exist, or the first progress post would
            // arrive before there is a bar to move.
            spawn_worker(hwnd, folder);
            LRESULT(0)
        }
        WM_PB_PROGRESS => {
            let (done, total) = (wp.0, lp.0 as usize);
            SEEN.0.store(done, Ordering::Relaxed);
            SEEN.1.store(total, Ordering::Relaxed);
            if let Ok(bar) = GetDlgItem(Some(hwnd), ID_BAR) {
                let _ = SendMessageW(
                    bar,
                    PBM_SETRANGE32,
                    Some(WPARAM(0)),
                    Some(LPARAM(total.max(1) as isize)),
                );
                let _ = SendMessageW(bar, PBM_SETPOS, Some(WPARAM(done)), None);
            }
            set_text(hwnd, ID_STATUS, &format!("{done} / {total}"));
            LRESULT(0)
        }
        WM_PB_DONE => {
            FINISHED.store(true, Ordering::Relaxed);
            let s = RESULT.lock().unwrap_or_else(|e| e.into_inner()).take();
            let text = s.map(|s| summary_text(&s)).unwrap_or_default();
            set_text(hwnd, ID_STATUS, &text);
            // The bar must READ as finished. A cancelled run stops part-way, and leaving it
            // half-filled next to "stopped" is the honest picture; a completed one is filled.
            if !CANCEL.load(Ordering::Relaxed) {
                if let Ok(bar) = GetDlgItem(Some(hwnd), ID_BAR) {
                    let total = SEEN.1.load(Ordering::Relaxed).max(1);
                    let _ = SendMessageW(bar, PBM_SETPOS, Some(WPARAM(total)), None);
                }
            }
            set_text(hwnd, ID_ACTION, t("pb_close"));
            LRESULT(0)
        }
        WM_COMMAND => {
            if (wp.0 & 0xFFFF) as i32 == ID_ACTION {
                // One button, two jobs: while work is running it cancels, and once the
                // summary is up it closes. Two buttons would leave a dead one on screen for
                // the whole run.
                if FINISHED.load(Ordering::Relaxed) {
                    // The run is over and the button reads "Close" — so close.
                    let _ = DestroyWindow(hwnd);
                } else if SEEN.1.load(Ordering::Relaxed) == 0 {
                    // Still scanning, so there is no progress to abandon: shut the flag and go.
                    CANCEL.store(true, Ordering::Relaxed);
                    let _ = DestroyWindow(hwnd);
                } else {
                    // Mid-run: ask the engine to stop and wait for its WM_PB_DONE, which brings
                    // the real counts. Closing here instead would throw away the summary of the
                    // work that WAS done.
                    CANCEL.store(true, Ordering::Relaxed);
                    set_text(hwnd, ID_ACTION, t("pb_close"));
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            // The path and the counts are secondary to the bar; dim them both.
            dark_ctlcolor_dim(wp)
        }
        WM_CTLCOLORBTN => {
            dark_ctlcolor(msg, wp).unwrap_or_else(|| DefWindowProcW(hwnd, msg, wp, lp))
        }
        WM_DPICHANGED => {
            wm_dpichanged(hwnd, lp);
            LRESULT(0)
        }
        WM_CLOSE => {
            CANCEL.store(true, Ordering::Relaxed);
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            // The worker may still be finishing its skip-loop; it only posts to a window that
            // is going away, which is harmless, and the process exits when the pump returns.
            CANCEL.store(true, Ordering::Relaxed);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

/// Entry point for `--prebuild <folder>`.
pub(crate) unsafe fn run_prebuild(folder: &str) {
    // Elevation would fill the ADMINISTRATOR's thumbnail cache and change nothing the user can
    // see, so refuse loudly rather than reporting a successful run that did nothing. Explorer
    // launches us unelevated, so this only fires if someone wired it up differently.
    if sagethumbs2k_core::prebuild::is_elevated() {
        let msg = crate::win::wide(t("pb_elevated"));
        let title = crate::win::wide(t("pb_title"));
        MessageBoxW(
            None,
            PCWSTR(msg.as_ptr()),
            PCWSTR(title.as_ptr()),
            MB_ICONWARNING | MB_OK,
        );
        return;
    }

    CANCEL.store(false, Ordering::Relaxed);
    FINISHED.store(false, Ordering::Relaxed);
    *RESULT.lock().unwrap_or_else(|e| e.into_inner()) = None;
    SEEN.0.store(0, Ordering::Relaxed);
    SEEN.1.store(0, Ordering::Relaxed);

    *FOLDER.lock().unwrap_or_else(|e| e.into_inner()) = folder.to_string();

    // `run_dialog` shows the window, pumps until it closes, and only then returns — so
    // everything this dialog needs to do is wired from `WM_CREATE`, not from here.
    run_dialog(
        w!("SageThumbs2KPrebuild"),
        Some(wndproc),
        t("pb_title"),
        DLG_W,
        DLG_H,
        None,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A long path must keep its TAIL, because that is the folder the user picked; losing the
    /// end would leave a dialog that cannot say what it is working on.
    #[test]
    fn elide_keeps_the_end_of_a_long_path() {
        let long = r"D:\media\archive\2019\reprocessed\finals\delivery\batch-07";
        let got = elide(long, 20);
        assert!(got.starts_with('…'), "elided path must be marked: {got}");
        assert!(
            got.ends_with("batch-07"),
            "the folder name must survive: {got}"
        );
        assert_eq!(got.chars().count(), 20);
    }

    #[test]
    fn elide_leaves_a_short_path_alone() {
        assert_eq!(elide(r"D:\pics", 20), r"D:\pics");
    }
}
