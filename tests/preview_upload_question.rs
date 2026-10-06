//! The Quick preview's Upload button asks its question, in both of its states, and the viewer
//! outlives the answer.
//!
//! 3.6.0 made the button opt-in: off, a click says why nothing happened and offers Settings; on,
//! it names the file and asks first. Neither question ever appeared. Both go through
//! `appkit::win::confirm_verbs`, whose task-dialog icon was `-3isize` (a 64-bit pointer) where
//! comctl32 wants `MAKEINTRESOURCE(-3)` (the 16-bit 0xFFFD), so comctl32 read it as a string and
//! the viewer died of an access violation: a user saw a busy cursor for a few seconds, then the
//! window was gone. Nothing had ever clicked the button; this does.
//!
//! The click is the documented `--shot --click N` harness (a real `WM_LBUTTONDOWN` at the
//! button's own rect, see `crates/preview/src/preview/shot.rs`). The viewer runs on a private
//! desktop nobody sees, and this test answers the question from outside the way a person
//! dismisses it (closing it is "no"). Settings go to a scratch `ST2K_SETTINGS_ROOT`.
#![cfg(windows)]

use std::path::Path;
use std::time::{Duration, Instant};

use windows::core::{BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, GENERIC_ALL, HWND, LPARAM, WAIT_OBJECT_0, WPARAM};
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, CreateDesktopW, EnumDesktopWindows, DESKTOP_CONTROL_FLAGS,
};
use windows::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, TerminateProcess, WaitForSingleObject,
    CREATE_UNICODE_ENVIRONMENT, PROCESS_INFORMATION, STARTUPINFOW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW, WM_CLOSE,
};
use windows_registry::CURRENT_USER;

/// `Btn::Upload`'s index in the toolbar's `BTNS` (`crates/preview/src/preview/window.rs`).
const UPLOAD: &str = "11";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// This process's environment with `extra` set, as a `CREATE_UNICODE_ENVIRONMENT` block.
fn env_block(extra: &[(&str, &str)]) -> Vec<u16> {
    let mut vars: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !extra.iter().any(|(e, _)| e.eq_ignore_ascii_case(k)))
        .collect();
    vars.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
    vars.sort_by_key(|(k, _)| k.to_uppercase());
    let mut block: Vec<u16> = vars
        .iter()
        .flat_map(|(k, v)| wide(&format!("{k}={v}")))
        .collect();
    block.push(0);
    block
}

/// `EnumDesktopWindows` callback: the visible task dialogs (class `#32770`) of the process whose
/// id `lp` points at, pushed onto the list beside it.
unsafe extern "system" fn collect(hwnd: HWND, lp: LPARAM) -> BOOL {
    let (pid, found) = &mut *(lp.0 as *mut (u32, Vec<HWND>));
    let mut owner = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut owner));
    let mut class = [0u16; 64];
    let n = GetClassNameW(hwnd, &mut class) as usize;
    if owner == *pid
        && IsWindowVisible(hwnd).as_bool()
        && String::from_utf16_lossy(&class[..n]) == "#32770"
    {
        found.push(hwnd);
    }
    true.into()
}

/// What one Upload click came to.
struct Outcome {
    /// The viewer's process id (it names the uploader's hand-off file).
    pid: u32,
    asked: bool,
    exit: u32,
}

/// Run the viewer over `doc` on a private desktop, click Upload, close whatever it asks, and
/// report whether it asked and how the viewer ended.
unsafe fn click_upload(case: &str, doc: &Path, out: &Path, settings_root: &str) -> Outcome {
    let desk_name = wide(&format!("st2k-upload-test-{}-{case}", std::process::id()));
    let desk = CreateDesktopW(
        PCWSTR(desk_name.as_ptr()),
        PCWSTR::null(),
        None,
        DESKTOP_CONTROL_FLAGS(0),
        GENERIC_ALL.0,
        None,
    )
    .expect("CreateDesktopW");
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        lpDesktop: PWSTR(desk_name.as_ptr() as *mut u16),
        ..Default::default()
    };
    let mut cmd = wide(&format!(
        "\"{}\" --shot \"{}\" --window preview --file \"{}\" --click {UPLOAD}",
        env!("CARGO_BIN_EXE_SageThumbs2K"),
        out.display(),
        doc.display()
    ));
    let env = env_block(&[
        ("ST2K_SETTINGS_ROOT", settings_root),
        ("ST2K_THEME", "dark"),
    ]);
    let mut pi = PROCESS_INFORMATION::default();
    CreateProcessW(
        PCWSTR::null(),
        Some(PWSTR(cmd.as_mut_ptr())),
        None,
        None,
        false,
        CREATE_UNICODE_ENVIRONMENT,
        Some(env.as_ptr().cast()),
        PCWSTR::null(),
        &si,
        &mut pi,
    )
    .expect("start SageThumbs2K --shot");
    let mut asked = false;
    let mut exit = None;
    let deadline = Instant::now() + Duration::from_secs(60);
    while exit.is_none() && Instant::now() < deadline {
        let mut seen = (pi.dwProcessId, Vec::new());
        let _ = EnumDesktopWindows(Some(desk), Some(collect), LPARAM(&raw mut seen as isize));
        for dlg in seen.1 {
            asked = true;
            let _ = PostMessageW(Some(dlg), WM_CLOSE, WPARAM(0), LPARAM(0));
        }
        if WaitForSingleObject(pi.hProcess, 100) == WAIT_OBJECT_0 {
            let mut code = 0u32;
            let _ = GetExitCodeProcess(pi.hProcess, &mut code);
            exit = Some(code);
        }
    }
    if exit.is_none() {
        let _ = TerminateProcess(pi.hProcess, 1);
    }
    let _ = CloseHandle(pi.hProcess);
    let _ = CloseHandle(pi.hThread);
    let _ = CloseDesktop(desk);
    Outcome {
        pid: pi.dwProcessId,
        asked,
        exit: exit.expect("the viewer was still running a minute after the click"),
    }
}

fn check(case: &str, upload_on: bool) {
    let dir = std::env::temp_dir().join(format!("st2k_upload_q_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let doc = dir.join("picture.png");
    image::RgbaImage::from_pixel(64, 48, image::Rgba([200, 60, 40, 255]))
        .save(&doc)
        .expect("write the picture");
    let out = dir.join("shot.png");
    let root = format!(
        r"Software\SageThumbs2K-test-upload-{}-{case}",
        std::process::id()
    );
    CURRENT_USER
        .create(&root)
        .and_then(|k| k.set_u32("PreviewUpload", u32::from(upload_on)))
        .expect("scratch settings key");

    let got = unsafe { click_upload(case, &doc, &out, &root) };
    let _ = CURRENT_USER.remove_tree(&root);
    // `command.rs::upload` hands the file over through this list: a "no" must never write it.
    let handoff = std::env::temp_dir().join(format!("st2k_preview_upload_{}.lst", got.pid));
    let uploaded = handoff.exists();
    let _ = std::fs::remove_file(&handoff);
    let shot = std::fs::metadata(&out).map_or(0, |m| m.len());
    let _ = std::fs::remove_dir_all(&dir);

    assert_eq!(
        got.exit, 0,
        "{case}: the viewer ended with exit 0x{:08X} after the Upload click (0xC0000005 is the \
         crash 3.6.0 shipped)",
        got.exit
    );
    assert!(got.asked, "{case}: the Upload click asked nothing");
    assert!(
        shot > 0,
        "{case}: the viewer did not live past the question to capture a frame"
    );
    assert!(
        !uploaded,
        "{case}: closing the question still handed the file to the uploader"
    );
}

#[test]
fn upload_off_says_why_and_survives() {
    check("off", false);
}

#[test]
fn upload_on_asks_first_and_survives() {
    check("on", true);
}
