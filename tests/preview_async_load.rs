//! F10 (2026-09-05 audit): the Quick preview window must show and stay responsive on a slow
//! file read, instead of blocking its own message pump before it can even appear.
//!
//! Drives the REAL `--preview <path>` live viewer (not `--shot`, which decodes synchronously
//! on purpose and is untouched by this fix) as a subprocess, with `ST2K_PREVIEW_SLOW_READ_MS`
//! as the deterministic slow-read seam (`preview::content::read_capped`, shared by the
//! unknown-extension sniff and the text/markdown read, see `docs/CHANGELOG.md` and
//! `src/bin/app/preview/content.rs`). No real network share or removable drive is touched.
//!
//! Pre-fix, `load()` ran the archive/DB/mail/text read synchronously on the same thread that
//! was about to become the window's message pump, so a slow read left the window either
//! invisible or unresponsive for its whole duration. Post-fix, `load()` shows the window in its
//! Loading state and hands the read to a worker thread, so the window appears almost
//! immediately and keeps answering `WM_COPYDATA` throughout the slow read.
//!
//! The companion unit test for the OTHER half of the acceptance bar (a stale, superseded load's
//! completion must never paint over the newest one) lives in `src/bin/app/preview/loader.rs`
//! (`is_load_current`), since that decision is a pure function extracted for exactly this.
#![cfg(windows)]

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, IsWindowVisible, SendMessageTimeoutW, SMTO_ABORTIFHUNG, WM_COPYDATA,
};

/// Quick preview viewer's window class (`preview/mod.rs::VIEWER_CLASS`).
const VIEWER_CLASS: &str = "SageThumbs2KViewer";
/// `preview/mod.rs::CMD_CLOSE`: close the viewer (honored only after its open-settle window).
const CMD_CLOSE: usize = 3;
/// `preview/window.rs::SETTLE_CLOSE_MS`: a close is ignored before this, so a key-repeat right
/// after open can't close a window that just appeared.
const SETTLE_CLOSE_MS: u64 = 400;

fn scratch_dir(case: &str) -> PathBuf {
    std::env::temp_dir().join(format!("st2k_preview_async_{}_{case}", std::process::id()))
}

fn write_sample(case: &str, name: &str, body: &str) -> PathBuf {
    let dir = scratch_dir(case);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join(name);
    std::fs::write(&path, body).expect("write sample");
    path
}

fn cleanup(case: &str) {
    let _ = std::fs::remove_dir_all(scratch_dir(case));
}

fn find_viewer() -> Option<HWND> {
    let wide: Vec<u16> = VIEWER_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe { FindWindowW(PCWSTR(wide.as_ptr()), PCWSTR::null()).ok() }
}

/// Poll for the viewer window, up to `timeout`. Requires `IsWindowVisible` as well as
/// `FindWindowW` finding a handle: `FindWindowW` matches a hidden window too (the class exists
/// from `CreateWindowExW` on, before `ensure_shown`'s `SW_SHOWNOACTIVATE` ever runs), so a
/// find-only check would prove creation, not the "appears" this test is named for. The
/// close-while-blocked assertion right after this is still the real proof the message pump is
/// alive; this addition only tightens what "appears" means for the first wait.
fn wait_for_viewer(timeout: Duration) -> Option<HWND> {
    let start = Instant::now();
    loop {
        if let Some(h) = find_viewer() {
            if unsafe { IsWindowVisible(h) }.as_bool() {
                return Some(h);
            }
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Send `WM_COPYDATA` `CMD_CLOSE`, bounded by `SendMessageTimeoutW`'s own timeout and
/// `SMTO_ABORTIFHUNG`, the documented way to detect a receiver whose message pump is not
/// turning, which is exactly what a synchronous slow read on the UI thread looks like from the
/// outside. Returns whether the call itself completed (a hung pump makes it fail/timeout).
fn send_close(hwnd: HWND, timeout_ms: u32) -> bool {
    let cds = COPYDATASTRUCT {
        dwData: CMD_CLOSE,
        cbData: 0,
        lpData: std::ptr::null_mut(),
    };
    let mut result = 0usize;
    let r = unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_COPYDATA,
            WPARAM(0),
            LPARAM(std::ptr::addr_of!(cds) as isize),
            SMTO_ABORTIFHUNG,
            timeout_ms,
            Some(&mut result),
        )
    };
    r.0 != 0
}

/// Poll `child` for exit, up to `timeout`.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> Option<std::process::ExitStatus> {
    let start = Instant::now();
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        if start.elapsed() > timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// A slow read must not stop the window from appearing, and must not stop it from answering a
/// close request while that read is still in flight.
///
/// Pre-fix this fails two different ways depending on timing: the `wait_for_viewer` poll times
/// out (the window cannot even be created-and-shown until `load()`'s synchronous read returns,
/// so under a 4 s slow read it simply isn't there yet at the 1.5 s mark), or, if it is found,
/// `send_close` fails/times out under `SMTO_ABORTIFHUNG`, because the thread that would pump
/// `WM_COPYDATA` is the same one still blocked inside the slow read.
#[test]
fn slow_file_read_never_blocks_the_message_pump() {
    let case = "responsive";
    let doc = write_sample(case, "slow.txt", "hello from the F10 slow-read seam test\n");

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"));
    cmd.env("ST2K_PREVIEW_SLOW_READ_MS", "4000")
        .arg("--preview")
        .arg(&doc);
    let mut child = cmd.spawn().expect("spawn SageThumbs2K --preview");

    // The window must appear within the documented responsiveness contract
    // (`safety::PREVIEW_APPEARANCE_BUDGET`), long before the 4 s slow read finishes.
    let appearance_budget = sagethumbs2k_core::safety::PREVIEW_APPEARANCE_BUDGET;
    let hwnd = match wait_for_viewer(appearance_budget) {
        Some(h) => h,
        None => {
            let _ = child.kill();
            cleanup(case);
            panic!(
                "preview window did not appear within {}ms of a 4s slow read starting \
                 (pre-fix: `load()` reads the file synchronously before the window can show)",
                appearance_budget.as_millis()
            );
        }
    };

    // Past the open-settle window but still well inside the 4 s slow read: ask the viewer to
    // close and confirm it actually can, right now, while the read is still running.
    std::thread::sleep(Duration::from_millis(SETTLE_CLOSE_MS + 200));
    let closed_promptly = send_close(hwnd, 1500);
    let exited = wait_for_exit(&mut child, Duration::from_millis(2000));

    if !closed_promptly || exited.is_none() {
        let _ = child.kill();
        cleanup(case);
        panic!(
            "viewer did not respond to a close request while its slow read was still in \
             flight (message send succeeded={closed_promptly}, process exited={}): pre-fix, \
             the UI thread is blocked inside the read and cannot pump WM_COPYDATA",
            exited.is_some()
        );
    }
    assert!(
        exited.unwrap().success(),
        "viewer exited with a failure status after CMD_CLOSE"
    );
    cleanup(case);
}
