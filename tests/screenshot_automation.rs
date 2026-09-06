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
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
    COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationElement, IUIAutomationElementArray,
    IUIAutomationInvokePattern, IUIAutomationSelectionItemPattern, TreeScope_Children,
    UIA_ButtonControlTypeId, UIA_InvokePatternId, UIA_RadioButtonControlTypeId,
    UIA_SelectionItemPatternId,
};
use windows::Win32::UI::HiDpi::{
    SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_TAB;
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

// ---------------------------------------------------------------------------------------
// The UI Automation provider, exercised the way Narrator or Accessibility Insights would:
// as a real UIA CLIENT in a separate process, driving the editor through the tree.
// ---------------------------------------------------------------------------------------

/// Launch the synthetic overlay and hand back its window once it has painted once.
///
/// The discoverability test above keeps its own inline copy of this dance because it asserts
/// the window styles step by step as it goes. The client test below needs only a painted
/// window, and the first paint matters to it for a different reason: the toolbar, and so
/// every element the provider can describe, is laid out from the committed selection.
unsafe fn launch_painted_overlay() -> (TestChild, windows::Win32::Foundation::HWND) {
    assert!(
        automation_window().is_none(),
        "a screenshot automation overlay is already running; close it before this test"
    );
    assert!(
        normal_capture_window().is_none(),
        "a normal screenshot overlay is already running; close it before this test"
    );

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
        if let Some(hwnd) = automation_window() {
            // The bare prefix is set at CreateWindowEx time, so only the full telemetry
            // title proves the first real WM_PAINT completed.
            if IsWindowVisible(hwnd).as_bool() && window_title(hwnd) == INITIAL_PAINTED_TITLE {
                break hwnd;
            }
        }
        assert!(
            Instant::now() < deadline,
            "automation overlay did not become visible within 10 seconds"
        );
        std::thread::sleep(Duration::from_millis(25));
    };

    let mut window_pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut window_pid));
    assert_eq!(
        window_pid, child_id,
        "the discovered automation window must belong to this test's exact child"
    );
    (child, hwnd)
}

/// Drag out the capture region, which is what brings the toolbar into existence.
///
/// Not incidental setup: with no committed selection `uia::children` returns an empty list by
/// design, so every element assertion below would be measuring the wrong state entirely.
unsafe fn commit_capture_region(hwnd: windows::Win32::Foundation::HWND, width: i32, height: i32) {
    PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(1), point_lparam(40, 40))
        .expect("start automation selection");
    PostMessageW(
        Some(hwnd),
        WM_MOUSEMOVE,
        WPARAM(1),
        point_lparam(width - 40, height - 40),
    )
    .expect("drag automation selection");
    PostMessageW(
        Some(hwnd),
        WM_LBUTTONUP,
        WPARAM(0),
        point_lparam(width - 40, height - 40),
    )
    .expect("finish automation selection");
}

/// Draw one annotation with whatever tool is currently active, then wait for the overlay to
/// publish `tool=<expected>` in its title.
///
/// This is the only way the ACTIVE tool becomes observable from another process:
/// `automation_title` appends `tool=` from the last COMMITTED drag, not from `Shot::tool`, so
/// a drag has to happen before the title can report which tool performed it.
unsafe fn draw_and_expect_tool(
    hwnd: windows::Win32::Foundation::HWND,
    anchor: (i32, i32),
    expected: &str,
) {
    let end = (anchor.0 + 150, anchor.1 + 80);
    PostMessageW(
        Some(hwnd),
        WM_LBUTTONDOWN,
        WPARAM(1),
        point_lparam(anchor.0, anchor.1),
    )
    .expect("start annotation drag");
    PostMessageW(
        Some(hwnd),
        WM_MOUSEMOVE,
        WPARAM(1),
        point_lparam(end.0, end.1),
    )
    .expect("preview annotation drag");
    PostMessageW(
        Some(hwnd),
        WM_LBUTTONUP,
        WPARAM(0),
        point_lparam(end.0, end.1),
    )
    .expect("commit annotation drag");

    let needle = format!("| tool={expected} |");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let title = window_title(hwnd);
        if title.contains(&needle) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the overlay never published {needle:?}; last title was {title:?}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// Everything the toolbar walk hands back to the test that owns it.
///
/// `kids` and `child_count` travel with the names because the focus proof re-reads the very same
/// collection, and `automation_ids` because a focused element is only proven to be one of OURS by
/// matching an id this walk collected.
struct EditorChildren {
    kids: IUIAutomationElementArray,
    child_count: i32,
    names: Vec<String>,
    automation_ids: Vec<String>,
    line_element: Option<IUIAutomationElement>,
}

/// Walk the editor's children as a client would, proving there are enough of them and that every
/// one is a named control of a type a screen reader can announce.
unsafe fn enumerate_named_controls(
    automation: &IUIAutomation,
    root: &IUIAutomationElement,
) -> EditorChildren {
    let condition = automation
        .CreateTrueCondition()
        .expect("create a match-everything condition");
    let kids = root
        .FindAll(TreeScope_Children, &condition)
        .expect("enumerate the editor's children");
    let child_count = kids.Length().expect("count the editor's children");
    assert!(
        child_count > 0,
        "the editor reported no children at all, so its toolbar is invisible to a screen reader"
    );
    // The bar carries twelve tools plus colour, undo, redo, copy, OCR, save, upload and
    // close, so anything at or below ten means the walk found the window chrome rather
    // than the editor's own controls. THIS is the assertion that fails outright against a
    // build with no provider: an ownerless popup with no child windows has zero children.
    assert!(
        child_count > 10,
        "the toolbar alone has more than ten buttons, but only {child_count} children were found"
    );

    let mut names: Vec<String> = Vec::new();
    let mut automation_ids: Vec<String> = Vec::new();
    let mut line_element: Option<IUIAutomationElement> = None;
    for i in 0..child_count {
        let el = kids.GetElement(i).expect("read one child element");
        let name = el
            .CurrentName()
            .expect("read a child element's Name")
            .to_string();
        // A control a screen reader cannot name is announced as a bare "button", which is
        // indistinguishable from the nineteen others beside it.
        assert!(
            !name.is_empty(),
            "child {i} of the editor has an empty Name"
        );
        let control_type = el
            .CurrentControlType()
            .expect("read a child element's ControlType");
        assert!(
            control_type.0 == UIA_ButtonControlTypeId.0
                || control_type.0 == UIA_RadioButtonControlTypeId.0,
            "child {i} ({name:?}) reported control type {}, but every element of this \
             chrome is either a button or, for the mutually exclusive tools, a radio button",
            control_type.0
        );
        automation_ids.push(
            el.CurrentAutomationId()
                .expect("read a child element's AutomationId")
                .to_string(),
        );
        if name == "Line" {
            line_element = Some(el);
        }
        names.push(name);
    }

    EditorChildren {
        kids,
        child_count,
        names,
        automation_ids,
        line_element,
    }
}

/// Press the Line tool the way a screen reader user would, then wait for the provider to agree
/// that Line is now the selected tool.
///
/// Reading a tree is half an accessible control; a screen reader user presses the button too.
unsafe fn prove_invoke_selects_the_line_tool(line: &IUIAutomationElement) {
    let invoke: IUIAutomationInvokePattern = line
        .GetCurrentPatternAs(UIA_InvokePatternId)
        .expect("the Line tool must expose the Invoke pattern");
    invoke
        .Invoke()
        .expect("invoking the Line tool through UI Automation must succeed");

    // `Invoke` is contractually asynchronous and the provider POSTS the work, so this
    // waits on the outcome rather than assuming the message has been handled. The tools
    // are one selection set, so IsSelected is the provider's own answer to "which tool is
    // active", read back through the client.
    let selected: IUIAutomationSelectionItemPattern = line
        .GetCurrentPatternAs(UIA_SelectionItemPatternId)
        .expect("a tool must expose the SelectionItem pattern");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if selected
            .CurrentIsSelected()
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the Line tool never reported itself selected after Invoke"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// The first of the two routes a client has to keyboard focus: ask the automation object outright,
/// and accept the answer only when it belongs to this editor.
unsafe fn focused_automation_id_within(
    automation: &IUIAutomation,
    automation_ids: &[String],
) -> Option<String> {
    let Ok(el) = automation.GetFocusedElement() else {
        return None;
    };
    let id = el
        .CurrentAutomationId()
        .map(|s| s.to_string())
        .unwrap_or_default();
    // The ids are this provider's own ("toolbar.N"), so matching one proves
    // the focused element is inside THIS window and not merely somewhere.
    if automation_ids.contains(&id) {
        return Some(id);
    }
    None
}

/// The second route: ask each child of the editor whether it holds the keyboard, which is the
/// property a reader polls rather than the focus event it subscribes to.
unsafe fn child_name_with_keyboard_focus(
    kids: &IUIAutomationElementArray,
    child_count: i32,
) -> Option<String> {
    for i in 0..child_count {
        let Ok(el) = kids.GetElement(i) else { continue };
        if el
            .CurrentHasKeyboardFocus()
            .map(|b| b.as_bool())
            .unwrap_or(false)
        {
            return el.CurrentName().ok().map(|n| n.to_string());
        }
    }
    None
}

/// Post Tab and wait for the editor to report keyboard focus by either client route, returning the
/// line the test logs about what each route saw.
///
/// A reader follows the keyboard, so an element that never claims focus is one the user is never
/// told they have arrived at.
unsafe fn prove_tab_reports_keyboard_focus(
    hwnd: windows::Win32::Foundation::HWND,
    automation: &IUIAutomation,
    kids: &IUIAutomationElementArray,
    child_count: i32,
    automation_ids: &[String],
) -> String {
    PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(VK_TAB.0 as usize), LPARAM(1))
        .expect("post Tab to move keyboard focus into the toolbar");

    let mut by_get_focus: Option<String> = None;
    let mut by_property: Option<String> = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if by_get_focus.is_none() {
            by_get_focus = focused_automation_id_within(automation, automation_ids);
        }
        if by_property.is_none() {
            by_property = child_name_with_keyboard_focus(kids, child_count);
        }
        if by_get_focus.is_some() && by_property.is_some() {
            break;
        }
        if Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        by_get_focus.is_some() || by_property.is_some(),
        "after Tab, neither GetFocusedElement nor HasKeyboardFocus placed keyboard focus \
         on any element of this editor, so a screen reader would never announce it"
    );
    format!("GetFocusedElement -> {by_get_focus:?}, HasKeyboardFocus -> {by_property:?}")
}

/// The provider must survive contact with a real assistive-technology client, not merely
/// expose a window that automation enumerators can see.
///
/// The neighbour above proves DISCOVERABILITY: the popup is not a tool window, not cloaked,
/// not owned, so an enumerator finds it. That says nothing about what is inside it, and for
/// most of this editor's life the honest answer was "one blank rectangle": a single
/// owner-drawn popup with no child windows, so every button was invisible to Narrator. This
/// test is the other half. It becomes a UIA client, walks the tree, and then OPERATES it,
/// because a tree a client can read but not drive is still an editor a screen reader user
/// cannot use.
///
/// Kept `#[ignore]` for the same reason as its neighbour: it opens an opaque, topmost window
/// over the whole virtual desktop.
#[test]
#[ignore = "opens the synthetic full-screen screenshot automation overlay and drives it over UI Automation"]
fn a_screen_reader_can_find_name_and_operate_the_editor() {
    if !unsafe { running_on_interactive_desktop() } {
        eprintln!(
            "skipping: no interactive window station (Session 0 service, or a CI runner \
             with no logged-on user) - UI Automation cannot reach a window that has no \
             visible desktop to live on"
        );
        return;
    }

    // Same reason as the neighbour: match the PMv2-aware app before reading virtual-screen
    // metrics, or Windows DPI-virtualises the caller and the drag coordinates land elsewhere.
    unsafe {
        let _ = SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let (mut child, hwnd) = unsafe { launch_painted_overlay() };

    let mut bounds = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut bounds) }.expect("query automation window bounds");
    let width = bounds.right - bounds.left;
    let height = bounds.bottom - bounds.top;
    assert!(
        width >= 500 && height >= 400,
        "this test needs a virtual desktop of at least 500x400; got {width}x{height}"
    );

    unsafe {
        commit_capture_region(hwnd, width, height);

        // Put the editor into a KNOWN tool that is NOT the one the Invoke below selects. The
        // starting tool comes from the user's settings, so without this the "tool=Line" proof
        // at the end could just as well be reporting a machine that already started on Line.
        PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(b'R' as usize), LPARAM(1))
            .expect("select the Rectangle tool");
        draw_and_expect_tool(hwnd, (width / 3, height / 2), "Rect");
    }

    // COM is initialised on THIS thread only, and torn down before the function returns, so
    // the rest of the test binary is unaffected. Apartment-threaded because that is what a
    // desktop assistive technology uses, and a UIA client that only makes outgoing calls is
    // well behaved in an STA.
    let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    assert!(
        hr.is_ok(),
        "CoInitializeEx(COINIT_APARTMENTTHREADED) failed: {hr:?}"
    );

    let (child_count, names, focus_report) = unsafe {
        let automation: IUIAutomation =
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .expect("create the CUIAutomation client object");

        // The first thing any client does with a window. A failure here means WM_GETOBJECT
        // never answered UiaRootObjectId, which is the state the editor was in before the
        // provider existed: findable window, no accessible content at all.
        let root = automation
            .ElementFromHandle(hwnd)
            .expect("UI Automation must resolve the overlay HWND to an element");
        let root_name = root
            .CurrentName()
            .expect("read the editor root's Name")
            .to_string();
        assert!(
            !root_name.is_empty(),
            "the editor root must name itself; an unnamed root is announced as nothing at all"
        );

        let children = enumerate_named_controls(&automation, &root);
        let names = children.names;

        // Names come from `uia::spoken_name`, which keeps the head of the tooltip and drops
        // the keyboard hint, so "Line (L) - drag to draw" is spoken as "Line" and
        // "Copy to the clipboard (Ctrl+C / Enter)" as "Copy to the clipboard". Asserting the
        // reduced forms is what pins that reduction from the outside.
        assert!(
            names.iter().any(|n| n == "Line"),
            "the Line tool must be reachable by name; saw {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("Copy")),
            "the clipboard buttons must be reachable by name; saw {names:?}"
        );

        // ---------------------------------------------------------------------------
        // The operation proof. Reading a tree is half an accessible control; a screen
        // reader user presses the button too.
        // ---------------------------------------------------------------------------
        let line = children
            .line_element
            .expect("the toolbar must expose an element named \"Line\"");
        prove_invoke_selects_the_line_tool(&line);

        // The independent proof, out of band from UIA entirely: draw again and let the
        // overlay say through its own window title which tool drew it. The drag posts no
        // tool change of its own, so "tool=Line" here can only have come from the Invoke,
        // and the earlier "tool=Rect" is what makes that a change rather than a coincidence.
        draw_and_expect_tool(hwnd, (width / 2, height / 2), "Line");

        // ---------------------------------------------------------------------------
        // Focus reporting. A reader follows the keyboard, so an element that never claims
        // focus is one the user is never told they have arrived at.
        // ---------------------------------------------------------------------------
        let focus_report = prove_tab_reports_keyboard_focus(
            hwnd,
            &automation,
            &children.kids,
            children.child_count,
            &children.automation_ids,
        );

        (children.child_count, names, focus_report)
    };

    // Every interface above went out of scope with the block, so nothing is still holding a
    // provider when the apartment is torn down.
    unsafe { CoUninitialize() };

    eprintln!("UIA children: {child_count}");
    eprintln!("UIA names: {names:?}");
    eprintln!("UIA focus: {focus_report}");

    child.close_and_wait(hwnd);
}
