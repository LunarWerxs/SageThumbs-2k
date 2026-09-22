//! Keyless screenshot / file upload. POSTs the image to a no-account, no-API-key
//! host and copies the returned URL to the clipboard.
//!
//! **No API key, no shared account.** Hosts like x0.at / catbox.moe accept an
//! anonymous multipart upload and rate-limit per **end-user IP** — so there's no
//! single key/account of ours to get hammered; each user's uploads are on their
//! own connection.
//!
//! **Fallback chain (2026-07):** these keyless hosts keep dying one at a time
//! (0x0.st disabled itself over AI-spam abuse; catbox.moe paused uploads over
//! storage), so a single hardcoded host is a single point of failure. We now try
//! [`builtin_hosts`] IN ORDER until one returns a URL — permanent hosts first, an
//! expiring one last, across THREE independent operators (x0.at, catbox, uguu.se)
//! so no single operator outage can take the whole chain down. Some hosts reply
//! with the bare URL, others embed it in JSON — see [`extract_url`].
//!
//! **User-editable config:** the whole chain is overridable via a plain-text file
//! `%APPDATA%\SageThumbs2K\upload-hosts.conf` (auto-created, self-documenting — the
//! path + template live in `sagethumbs2k_core::upload_config`, shared with the
//! `st2k upload-hosts` CLI) so a user can add / reorder / replace hosts, or point at
//! their own server, with no rebuild. A legacy single-host HKCU override still works
//! too. See [`upload_hosts`] for the precedence.
//!
//! When every host refuses, the failure dialog shows **what each host actually
//! said** (e.g. "catbox.moe — Uploads paused…") so the user can tell a host outage
//! ("just wait") apart from a real connection problem.
//!
//! Runs in its OWN `--upload <png>` / `--upload-keep <list>` process (spawned by the
//! toolbar's Upload button / the DLL verb) so the shell never blocks on the network.

mod hosts;
use hosts::*;
mod wire;
pub(crate) use hosts::open_hosts_config;
use wire::*;

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetSystemMetrics, MessageBoxW, PeekMessageW,
    SendMessageW, TranslateMessage, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MSG, PM_REMOVE,
    SM_CXSCREEN, SM_CYSCREEN, SW_SHOWNORMAL, WINDOW_STYLE, WM_SETFONT, WS_BORDER, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
};

use crate::win::{set_clipboard_text, t, wide, SS_CENTER, SS_CENTERIMAGE};
use sagethumbs2k_core::upload_history::{self, Entry, Expiry};

const MAX_RESP: usize = 64 * 1024; // a URL response is tiny; cap to be safe

/// Caption for the screenshot-upload completion dialogs.
fn shot_caption() -> &'static str {
    t("up_caption_shot")
}
/// Caption for the right-click "Upload" verb's completion dialogs.
fn file_caption() -> &'static str {
    t("up_caption_file")
}

/// A tiny topmost "Uploading…" pill (bottom-center of the primary monitor) shown while
/// `work` runs on a worker thread — the overlay/menu that launched us is already gone by
/// then, so without it the user stares at NOTHING for the seconds (and up to three host
/// retries) an upload takes, and reasonably assumes it silently failed. This thread pumps
/// messages so the pill actually paints; the pill is non-activating and owns no input.
///
/// `pub(crate)` because the OCR helper (`crate::ocr_result`) has the identical problem:
/// it is a fresh process with no window of its own while the WinRT engine spins up.
pub(crate) unsafe fn with_busy_pill<T: Send + 'static>(
    text: &str,
    work: impl FnOnce() -> T + Send + 'static,
) -> T {
    let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    let (w, h) = (300, 40);
    let txt = wide(text);
    let pill = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
        windows::core::w!("STATIC"),
        PCWSTR(txt.as_ptr()),
        WS_POPUP | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(SS_CENTER | SS_CENTERIMAGE),
        (sw - w) / 2,
        sh - h - 90, // above the taskbar area, bottom-center
        w,
        h,
        None,
        None,
        None,
        None,
    )
    .ok();
    if let Some(p) = pill {
        SendMessageW(
            p,
            WM_SETFONT,
            Some(windows::Win32::Foundation::WPARAM(
                crate::win::gui_font().0 as usize,
            )),
            Some(windows::Win32::Foundation::LPARAM(1)),
        );
    }

    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(work());
    });
    let mut msg = MSG::default();
    let result = loop {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        match rx.recv_timeout(std::time::Duration::from_millis(30)) {
            Ok(v) => break v,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            // Unreachable under panic=abort (a worker panic kills the process), but
            // don't hang the pill forever if it somehow happens.
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => std::process::exit(1),
        }
    };
    if let Some(p) = pill {
        let _ = DestroyWindow(p);
    }
    result
}

/// Upload `path` (a throwaway capture PNG): read the temp capture, delete it immediately
/// (so no other process races the path), upload the bytes, copy the resulting URL to the
/// clipboard, and report. Spawned by the capture overlay's Upload button via `--upload <png>`.
pub(crate) unsafe fn run_upload(path: &str) {
    // Resolve (and validate) the endpoint(s) first, so a misconfigured custom host
    // gives a specific message instead of a generic "couldn't upload".
    let hosts = match upload_hosts() {
        Ok(h) => h,
        Err(msg) => {
            let _ = std::fs::remove_file(path);
            notify(&msg, shot_caption(), true);
            return;
        }
    };
    let bytes = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    // The temp file is gone the moment we read it (the pill's worker thread can run
    // long enough — up to three host retries — that leaving it around risks a second
    // process racing the same path). Keep the bytes themselves alive past the upload
    // attempt so a total failure below can still recover the capture instead of
    // reporting it lost with nothing left to show for it.
    let recovery_bytes = bytes.as_ref().ok().cloned();
    let result = with_busy_pill(t("up_busy_one"), move || match bytes {
        // SAFETY: upload_any only touches WinInet handles it creates + closes itself,
        // so running it on the pill's worker thread is fine.
        Ok(b) => unsafe { upload_any(&b, "screenshot.png", &hosts) },
        Err(e) => Err(format!("couldn't read the capture — {e}")),
    });
    match result {
        Ok(done) => {
            let _ = set_clipboard_text(&done.url);
            crate::upload_result::show_upload_result(t("up_done_one"), std::slice::from_ref(&done));
        }
        Err(reasons) => {
            let base = upload_failed_msg(t("up_what_screenshot"), &reasons);
            // Every host failed and the temp capture is already deleted — write the
            // in-memory bytes back out so the shot isn't gone for good, and say where.
            let msg = match recovery_bytes.and_then(|b| save_recovery_copy(&b)) {
                Some(p) => format!("{base}\n\nSaved a copy to:\n{}", p.display()),
                None => base,
            };
            notify(&msg, shot_caption(), true);
        }
    }
}

/// Write `bytes` (a whole PNG) into `dir` under the standard timestamped capture
/// name. Split out from [`save_recovery_copy`] so the write itself is testable
/// without touching the registry-backed save-folder setting.
///
/// Routes through [`super::output::write_reserved`] rather than writing
/// `dir.join(name)` directly: `timestamped_name` only has 1-second resolution, so two
/// failed-upload recoveries (or a recovery landing in the same second as an ordinary
/// Ctrl+S capture) into the same folder would otherwise silently overwrite each other —
/// in exactly the path whose whole purpose is not losing the shot. The name is reserved
/// with `create_new` at pick time, so even two recoveries in the same tick cannot share it.
fn write_recovery_copy(dir: &std::path::Path, bytes: &[u8]) -> Option<std::path::PathBuf> {
    let _ = std::fs::create_dir_all(dir);
    let name = unsafe { super::output::timestamped_name() };
    super::output::write_reserved(dir, &name, bytes)
}

/// Recover a failed upload's bytes to the user's normal capture save location (their
/// configured folder, or Desktop) — the same place a manual Ctrl+S would have gone.
fn save_recovery_copy(bytes: &[u8]) -> Option<std::path::PathBuf> {
    write_recovery_copy(
        &std::path::PathBuf::from(super::effective_save_dir()),
        bytes,
    )
}

/// Upload the USER files listed (one path per line) in `list_path` — the right-click
/// "Upload" verb's path — copy the resulting URL(s) to the clipboard (one per line),
/// and report. Unlike [`run_upload`], these are the user's own files and are **never
/// deleted**; only the temporary list file is removed. Spawned by the DLL verb via
/// `--upload-keep <list>`.
///
/// `url_to`, when set (only `st2k upload` passes it — the DLL verb never does), redirects
/// the whole result path away from the GUI: no MessageBox, no clipboard write. On success
/// the resulting URL(s) are written LF-joined to that path and the process exits `0`; on
/// any failure (including the hosts-config error below) nothing is written there, the
/// reason goes to stderr instead, and the process exits `1`. `st2k`, a console-subsystem
/// binary, still gets stderr/exit-status from this windows-subsystem one because both are
/// plain inherited OS handles — no console window is involved either way.
pub(crate) unsafe fn run_upload_keep(list_path: &str, url_to: Option<&str>) {
    let hosts = match resolve_hosts(list_path, url_to) {
        Some(h) => h,
        None => return,
    };
    let files = match load_file_list(list_path) {
        Ok(files) => files,
        Err(e) => {
            let msg = format!("couldn't read the file list — {e}");
            if url_to.is_some() {
                eprintln!("{msg}");
                std::process::exit(1);
            }
            notify(&msg, file_caption(), true);
            return;
        }
    };
    if files.is_empty() {
        if url_to.is_some() {
            eprintln!("no file to upload");
            std::process::exit(1);
        }
        return;
    }
    let total = files.len();
    // Upload each file under its real name so the host keeps the extension (the
    // returned link then stays viewable in a browser). Remember the last failure
    // reason so an all-fail run can show WHY (host paused vs. no connection). The
    // whole batch runs behind the "Uploading…" pill — multi-file menu uploads can
    // take a while and previously gave zero sign anything was happening.
    let busy = busy_message(total);
    // SAFETY: run_upload_keep is itself unsafe for the same reason (WinInet handles
    // scoped to this call), but a closure doesn't inherit its enclosing fn's unsafety.
    let (done, last_reason) = with_busy_pill(&busy, move || unsafe { upload_all(&files, &hosts) });
    match url_to {
        Some(url_to) => report_to_file(url_to, &done, last_reason),
        None => report_interactively(total, &done, last_reason),
    }
}

/// The DLL writes the selection CRLF-joined; tolerate either ending, drop blanks. Removes
/// `list_path` either way — the list is ours; the images are NOT. Propagates the read error
/// so the caller can tell an unreadable list apart from an empty selection.
fn load_file_list(list_path: &str) -> std::io::Result<Vec<String>> {
    let read = std::fs::read_to_string(list_path);
    let _ = std::fs::remove_file(list_path);
    Ok(read?
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// The "Uploading…" pill's text — singular for one file, a `{n}`-filled count otherwise.
fn busy_message(total: usize) -> String {
    if total == 1 {
        t("up_busy_one").to_string()
    } else {
        t("up_busy_many").replace("{n}", &total.to_string())
    }
}

/// Upload every file, returning the uploads that succeeded plus the last failure reason (if
/// any) so an all-fail run can still show WHY.
///
/// SAFETY: upload_any only touches WinInet handles it creates + closes itself, so running
/// it on the pill's worker thread is fine.
unsafe fn upload_all(files: &[String], hosts: &[UploadHost]) -> (Vec<Entry>, Option<String>) {
    let mut done: Vec<Entry> = Vec::new();
    let mut last_reason: Option<String> = None;
    for f in files {
        let name = std::path::Path::new(f)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("upload");
        match std::fs::read(f) {
            // Already in an unsafe fn body — no nested `unsafe` block needed here (unlike
            // the closure this loop used to run inside).
            Ok(bytes) => match upload_any(&bytes, name, hosts) {
                Ok(u) => done.push(u),
                Err(why) => last_reason = Some(why),
            },
            Err(e) => last_reason = Some(format!("couldn't read {name} — {e}")),
        }
    }
    (done, last_reason)
}

/// CLI path (`url_to` set): no clipboard, no dialog — just the file `st2k` is waiting to
/// read. Success writes the URL(s) LF-joined and exits `0`; any failure writes nothing,
/// puts the reason on stderr, and exits `1`.
fn report_to_file(url_to: &str, done: &[Entry], last_reason: Option<String>) -> ! {
    if done.is_empty() {
        let reasons = last_reason.unwrap_or_else(|| "no readable files".to_string());
        let _ = std::fs::remove_file(url_to);
        eprintln!("{reasons}");
        std::process::exit(1);
    }
    let urls: Vec<&str> = done.iter().map(|e| e.url.as_str()).collect();
    match std::fs::write(url_to, urls.join("\n")) {
        Ok(()) => std::process::exit(0),
        Err(e) => {
            eprintln!("uploaded, but couldn't write the result to {url_to}: {e}");
            std::process::exit(1);
        }
    }
}

/// Interactive path (`url_to` unset): a failure dialog naming every host's own reason, or
/// success — clipboard + the result window with a heading matched to how many of `total`
/// files made it.
unsafe fn report_interactively(total: usize, done: &[Entry], last_reason: Option<String>) {
    if done.is_empty() {
        let reasons = last_reason.unwrap_or_else(|| "no readable files".to_string());
        let what = if total == 1 {
            t("up_what_file")
        } else {
            t("up_what_any_files")
        };
        notify(&upload_failed_msg(what, &reasons), file_caption(), true);
        return;
    }
    let _ = set_clipboard_text(&crate::upload_result::links_of(done));
    let heading = if total == 1 {
        t("up_done_one").to_string()
    } else if done.len() == total {
        t("up_done_all").replace("{total}", &total.to_string())
    } else {
        t("up_done_partial")
            .replace("{ok}", &done.len().to_string())
            .replace("{total}", &total.to_string())
            .replace("{failed}", &(total - done.len()).to_string())
    };
    crate::upload_result::show_upload_result(&heading, done);
}

/// Body for the "couldn't upload" dialog. Includes what each host actually said, so a
/// host outage ("just wait") is distinguishable from a real connection problem.
fn upload_failed_msg(what: &str, reasons: &str) -> String {
    let cfg = sagethumbs2k_core::upload_config::config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "%APPDATA%\\SageThumbs2K\\upload-hosts.conf".to_string());
    t("up_failed")
        .replace("{what}", what)
        .replace("{reasons}", reasons)
        .replace("{cfg}", &cfg)
}

/// A simple completion message (the upload process has no window of its own).
unsafe fn notify(msg: &str, caption: &str, error: bool) {
    let body = wide(msg);
    let cap = wide(caption);
    let icon = if error {
        MB_ICONWARNING
    } else {
        MB_ICONINFORMATION
    };
    MessageBoxW(
        None,
        PCWSTR(body.as_ptr()),
        PCWSTR(cap.as_ptr()),
        MB_OK | icon,
    );
}

/// Try each host in order; return the first upload that works, or — if all fail — a
/// multi-line summary of what each host said (`host — reason`), one per line.
unsafe fn upload_any(bytes: &[u8], filename: &str, hosts: &[UploadHost]) -> Result<Entry, String> {
    let mut reasons: Vec<String> = Vec::new();
    for h in hosts {
        match upload_one(bytes, filename, h) {
            Ok(url) => return Ok(remember(h, url, filename, bytes.len())),
            Err(why) => reasons.push(format!("{} — {}", h.host, why)),
        }
    }
    Err(reasons.join("\n"))
}

/// A finished upload as a history entry: its expiry worked out now, from the policy of the
/// host that actually took it (the fallback chain can land any file on any host), and added
/// to the local "Recent uploads" list. Every path records here - the screenshot button, the
/// right-click verb and `st2k upload` - so the list is complete whichever one was used. A list
/// that cannot be written is not an upload failure; the link is already live.
fn remember(h: &UploadHost, url: String, filename: &str, size: usize) -> Entry {
    let now = upload_history::now_unix();
    let retention = sagethumbs2k_core::upload_config::retention_for(&h.host, &h.extra, size as u64);
    let entry = Entry {
        uploaded: now,
        expires: Expiry::from_retention(retention, now),
        host: h.host.clone(),
        url,
        name: filename.to_string(),
    };
    let _ = upload_history::record(&entry);
    entry
}

#[cfg(test)]
mod tests;
