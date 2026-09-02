//! Full-screen screenshot-editor automation contract.
//!
//! This test is deliberately ignored by default: it opens an opaque, topmost window over
//! the complete virtual desktop. Run it explicitly when changing screenshot window creation:
//!
//! `cargo test --test screenshot_automation -- --ignored --test-threads=1`
//!
//! The hidden route must remain synthetic and side-effect-free. This test checks only the
//! externally observable window contract; focused unit tests in the screenshot module cover
//! the mode/style decisions without opening UI.
//!
//! Also needs a genuinely INTERACTIVE desktop: it asserts the overlay takes the foreground
//! (`GetForegroundWindow`), which is a session with no logged-on user (a Windows service's
//! Session 0, many CI runners) can never grant no matter what the app does. The test detects
//! that case itself (`running_on_interactive_desktop`) and skips with a message rather than
//! failing on a box that was never a candidate to pass.
#![cfg(windows)]

use std::ffi::c_void;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetForegroundWindow, GetSystemMetrics, GetWindow, GetWindowLongPtrW,
    GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, PostMessageW,
    GWL_EXSTYLE, GW_OWNER, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, WM_CLOSE, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

const TITLE_PREFIX: &str = "SageThumbs 2K Screenshot Automation";
const INITIAL_PAINTED_TITLE: &str =
    "SageThumbs 2K Screenshot Automation | snap=0 | commit=0 | painted=0 | status=ready";

struct TestChild(Child);

impl TestChild {
    fn close_and_wait(&mut self, hwnd: windows::Win32::Foundation::HWND) {
        unsafe {
            PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0))
                .expect("post WM_CLOSE to automation overlay");
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.0.try_wait().expect("query automation child").is_some() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("automation overlay did not exit after WM_CLOSE");
    }
}

impl Drop for TestChild {
    fn drop(&mut self) {
        // Scoped to the exact child this test launched. This also cleans up after an
        // assertion panic without touching a user's normal screenshot/daemon process.
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Whether this process is attached to an INTERACTIVE window station — false in a
/// non-interactive session (a Windows service's Session 0, some CI runners with no
/// logged-on user). `SetForegroundWindow`/foreground-focus checks are unwinnable there
/// regardless of anything the app does, which is exactly the assertion further down that
/// needs this test skipped rather than failed on such a box. Needs the
/// `Win32_System_StationsAndDesktops` `windows` crate feature.
unsafe fn running_on_interactive_desktop() -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::StationsAndDesktops::{
        GetProcessWindowStation, GetUserObjectInformationW, UOI_FLAGS, USEROBJECTFLAGS,
    };
    use windows::Win32::UI::WindowsAndMessaging::WSF_VISIBLE;

    let Ok(hwinsta) = (unsafe { GetProcessWindowStation() }) else {
        return false;
    };
    let mut flags = USEROBJECTFLAGS::default();
    let mut needed = 0u32;
    let queried = unsafe {
        GetUserObjectInformationW(
            HANDLE(hwinsta.0),
            UOI_FLAGS,
            Some(&mut flags as *mut _ as *mut c_void),
            std::mem::size_of::<USEROBJECTFLAGS>() as u32,
            Some(&mut needed),
        )
    };
    queried.is_ok() && (flags.dwFlags as i32 & WSF_VISIBLE) != 0
}

unsafe fn automation_window() -> Option<windows::Win32::Foundation::HWND> {
    FindWindowW(w!("SageThumbs2KShotAutomation"), PCWSTR::null()).ok()
}

unsafe fn normal_capture_window() -> Option<windows::Win32::Foundation::HWND> {
    FindWindowW(w!("SageThumbs2KShot"), PCWSTR::null()).ok()
}

unsafe fn window_title(hwnd: windows::Win32::Foundation::HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn point_lparam(x: i32, y: i32) -> LPARAM {
    let packed = u32::from(x as u16) | (u32::from(y as u16) << 16);
    LPARAM(packed as isize)
}

#[test]
#[ignore = "opens the synthetic full-screen screenshot automation overlay"]
fn synthetic_overlay_is_discoverable_by_windows_automation() {
    if !unsafe { running_on_interactive_desktop() } {
        eprintln!(
            "skipping: no interactive window station (Session 0 service, or a CI runner \
             with no logged-on user) — SetForegroundWindow can never succeed here"
        );
        return;
    }

    // Match the PMv2-aware app before comparing virtual-screen metrics/window
    // bounds. Without this, Windows may DPI-virtualize the test caller and make an
    // exact full-screen window look smaller on mixed-DPI desktops.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    unsafe {
        assert!(
            automation_window().is_none(),
            "a screenshot automation overlay is already running; close it before this test"
        );
        assert!(
            normal_capture_window().is_none(),
            "a normal screenshot overlay is already running; close it before this test"
        );
    }

    let child = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .arg("--screenshot-automation")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("launch synthetic screenshot automation mode");
    let child_id = child.id();
    let mut child = TestChild(child);

    let deadline = Instant::now() + Duration::from_secs(10);
    let hwnd = loop {
        if let Some(status) = child.0.try_wait().expect("query automation child") {
            panic!("automation child exited before creating its window: {status}");
        }
        let found = unsafe { automation_window() };
        if let Some(hwnd) = found {
            // The bare prefix is used at CreateWindowEx time. Waiting for the full
            // telemetry title proves the first real WM_PAINT completed, rather than
            // accepting a merely-visible but unpainted popup.
            if unsafe {
                IsWindowVisible(hwnd).as_bool() && window_title(hwnd) == INITIAL_PAINTED_TITLE
            } {
                break hwnd;
            }
        }
        assert!(
            Instant::now() < deadline,
            "automation overlay did not become visible within 10 seconds"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    unsafe {
        let mut window_pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
        assert_eq!(
            window_pid, child_id,
            "the discovered automation window must belong to this test's exact child"
        );

        let title = window_title(hwnd);
        assert!(
            title.starts_with(TITLE_PREFIX),
            "unexpected automation window title: {title:?}"
        );
        assert_eq!(
            title, INITIAL_PAINTED_TITLE,
            "automation window must publish its post-paint initial state"
        );

        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        assert_eq!(
            ex_style & WS_EX_TOOLWINDOW.0,
            0,
            "WS_EX_TOOLWINDOW makes the editor invisible to Windows UI automation"
        );
        assert_ne!(
            ex_style & WS_EX_TOPMOST.0,
            0,
            "the capture editor must remain topmost"
        );
        assert_ne!(
            ex_style & WS_EX_NOACTIVATE.0,
            0,
            "WS_EX_NOACTIVATE keeps this ownerless popup out of the taskbar by default"
        );
        assert!(
            GetWindow(hwnd, GW_OWNER).is_err(),
            "the automation window must be ownerless so discovery accepts it"
        );

        let mut cloaked = 1u32;
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut _ as *mut c_void,
            std::mem::size_of::<u32>() as u32,
        )
        .expect("query DWM cloak state");
        assert_eq!(cloaked, 0, "automation window must not be DWM-cloaked");

        // The overlay carries WS_EX_NOACTIVATE, so activation is entirely explicit —
        // and Windows' foreground lock routinely REFUSES SetForegroundWindow from a
        // process spawned by a background hotkey daemon. When that happens the window
        // still shows and still takes mouse clicks, but never receives a keystroke, so
        // Esc does not close the capture and the user is stuck with a full-screen
        // overlay. Owner-reported, 2026-07-31. `activate_overlay` now falls back to
        // attaching to the foreground thread's input queue; this asserts the outcome
        // rather than the mechanism.
        let mut focused = std::ptr::null_mut();
        for _ in 0..40 {
            focused = GetForegroundWindow().0;
            if focused == hwnd.0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert_eq!(
            focused, hwnd.0,
            "the capture overlay never took the foreground, so it can receive no keys              (Esc will not cancel)"
        );

        let mut actual = RECT::default();
        GetWindowRect(hwnd, &mut actual).expect("query automation window bounds");
        let expected = RECT {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN),
            top: GetSystemMetrics(SM_YVIRTUALSCREEN),
            right: GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN),
            bottom: GetSystemMetrics(SM_YVIRTUALSCREEN) + GetSystemMetrics(SM_CYVIRTUALSCREEN),
        };
        assert_eq!(
            actual, expected,
            "automation canvas must cover the complete virtual desktop"
        );

        // Exercise the real editor message path after proving the window is exposed:
        // select a region, choose Line, latch the automation-only Shift surrogate,
        // and drag at a non-45-degree angle. The post-paint title reports both raw
        // and committed geometry, so this checks the visible preview/commit pipeline
        // rather than only the pure snap helper.
        let width = actual.right - actual.left;
        let height = actual.bottom - actual.top;
        assert!(
            width >= 500 && height >= 400,
            "automation snap test needs a virtual desktop of at least 500x400; got {width}x{height}"
        );

        let selection_start = (40, 40);
        let selection_end = (width - 40, height - 40);
        PostMessageW(
            Some(hwnd),
            WM_LBUTTONDOWN,
            WPARAM(1),
            point_lparam(selection_start.0, selection_start.1),
        )
        .expect("start automation selection");
        PostMessageW(
            Some(hwnd),
            WM_MOUSEMOVE,
            WPARAM(1),
            point_lparam(selection_end.0, selection_end.1),
        )
        .expect("drag automation selection");
        PostMessageW(
            Some(hwnd),
            WM_LBUTTONUP,
            WPARAM(0),
            point_lparam(selection_end.0, selection_end.1),
        )
        .expect("finish automation selection");

        PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(b'L' as usize), LPARAM(1))
            .expect("select Line tool");
        PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(0x77), LPARAM(1))
            .expect("latch synthetic Shift with F8");

        let anchor = (width / 3, height / 2);
        // sqrt(150^2 + 80^2) is exactly 170; nearest 45 degrees therefore
        // commits a rounded (120,120) delta while preserving drag length.
        let raw = (anchor.0 + 150, anchor.1 + 80);
        let final_point = (anchor.0 + 120, anchor.1 + 120);
        PostMessageW(
            Some(hwnd),
            WM_LBUTTONDOWN,
            WPARAM(1),
            point_lparam(anchor.0, anchor.1),
        )
        .expect("start snapped line");
        PostMessageW(
            Some(hwnd),
            WM_MOUSEMOVE,
            WPARAM(1),
            point_lparam(raw.0, raw.1),
        )
        .expect("preview snapped line");
        PostMessageW(
            Some(hwnd),
            WM_LBUTTONUP,
            WPARAM(0),
            point_lparam(raw.0, raw.1),
        )
        .expect("commit snapped line");

        let expected_title = format!(
            "{TITLE_PREFIX} | snap=1 | commit=1 | painted=1 | status=ready | \
             tool=Line | anchor={},{} | raw={},{} | final={},{} | shifted=1",
            anchor.0, anchor.1, raw.0, raw.1, final_point.0, final_point.1
        );
        let paint_deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let title = window_title(hwnd);
            if title == expected_title {
                break;
            }
            assert!(
                Instant::now() < paint_deadline,
                "snapped line was not committed and painted; expected {expected_title:?}, got {title:?}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    child.close_and_wait(hwnd);
}
