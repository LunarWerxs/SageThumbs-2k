//! A human click-through of the preview window, replaced: drives the REAL preview wndproc
//! (`src/bin/app/preview/window.rs` + its `window/mouse.rs`/`window/keys.rs` split) through the
//! actual OS message loop over exactly the paths that split touched and that a green
//! `cargo build`/`cargo test`/`clippy` cannot prove on their own — the mouse handlers (hover
//! already covered by `--hot`, so this file adds press/release/capture-changed), the keyboard
//! dispatch (a navigation key each direction, Escape, and a Ctrl+ toolbar shortcut), and a real
//! resize. Standing repo rule (CLAUDE.md §6.1.1, "STOP DEFERRING CHECKS TO A MOUSE"): where a
//! check seems to need a human, drive the real surface instead.
//!
//! Two drivers, both real message-loop dispatch, chosen per test for what each proves best:
//!
//! - Most tests extend the documented headless `--shot --window preview` harness (CLAUDE.md
//!   §6), which already drives real message paths for hover/find/scroll/resize. Three NEW
//!   driving flags were added to `src/bin/app/preview/shot.rs` for this file — `--click N`
//!   (a real `WM_LBUTTONDOWN`+`WM_LBUTTONUP` at toolbar button N's own on-screen rect, not a
//!   direct `do_action` call), `--drag X1,Y1,X2,Y2,X3,Y3[,interrupt]` (a two-leg mouse drag,
//!   optionally interrupted by a real `WM_CAPTURECHANGED` between the legs), and
//!   `--press SPEC[,SPEC...]` (real `WM_KEYDOWN`s, `ctrl+X` genuinely holding `VK_CONTROL` via
//!   `keybd_event` the same way `--wheel --ctrl` already does). See each flag's doc comment in
//!   `shot.rs` for exactly what it posts. These need a real window station (GDI + `PrintWindow`)
//!   like every other headless shot test in this suite (`preview_db_shot.rs`,
//!   `preview_theme_toggle.rs`) — same requirement, same lack of a skip check: if the box has no
//!   window station these fail the same way the neighbours they extend already do.
//! - The navigation+Escape test drives a LIVE `--preview <path>` window cross-process
//!   (`tests/preview_async_load.rs`'s pattern): `FindWindowW` the real viewer, `PostMessageW`
//!   real `WM_KEYDOWN`s into it, and read back the window TITLE (`window::set_title` publishes
//!   the current file's leaf name) as the observable — the same "automation channel" idea
//!   `tests/screenshot_automation.rs` uses, without needing that test's foreground/UIA
//!   machinery since nothing here asserts real OS focus.
//!
//! Settings isolation: the live test redirects `ST2K_SETTINGS_ROOT` to a scratch, per-run HKCU
//! subkey (never the developer's real `HKCU\Software\SageThumbs2K`) — see
//! `settings::hkcu_root`'s doc comment and `scripts/make-shots.ps1`'s identical redirect. The
//! `--shot`-based tests don't need it: like `preview_theme_toggle.rs`/`preview_db_shot.rs`,
//! nothing they drive persists a setting, and `ST2K_THEME=dark` (forced below, same as
//! `preview_theme_toggle.rs`) already pins the one thing that would otherwise vary by machine.
//!
//! `SETTLE_CLOSE_MS` (see `tests/preview_async_load.rs`) gates only the WM_ACTIVATE-driven
//! close-on-focus-loss path (`window.rs::on_activate`) — Escape's own arm
//! (`window/keys.rs::keydown_lifecycle`) calls `request_close` unconditionally once `st.manual`
//! is true, with no settle timer of its own. The live test still waits for the window to
//! publish its loaded title (a real, polled effect) before pressing Escape, rather than assuming
//! a blind sleep proves creation finished — "wait for the effect, do not sleep and hope."
//!
//! What this covers, and what still needs a human: see the six tests below for the exact
//! wndproc paths driven. NOT covered by this file (still needs a human, or a future headless
//! extension): tooltip popups (comctl-timed), the PDF-strip thumbnail click, and multi-tab
//! Explorer selection — same three exceptions CLAUDE.md §6 already documents as the harness's
//! limits.
#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_LEFT, VK_RIGHT};
use windows::Win32::UI::WindowsAndMessaging::{
    FindWindowW, GetWindowTextW, IsWindowVisible, PostMessageW, WM_KEYDOWN,
};
use windows_registry::CURRENT_USER;

// ===== Shared scratch-dir plumbing (same convention as the other preview shot tests) =====

fn scratch(case: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("st2k_wndproc_drive_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn cleanup(case: &str) {
    let _ = std::fs::remove_dir_all(
        std::env::temp_dir().join(format!("st2k_wndproc_drive_{}_{case}", std::process::id())),
    );
}

/// One headless `--shot --window preview` capture of `doc` with `extra` driving flags appended.
/// `ST2K_THEME=dark` is forced (same reasoning as `preview_theme_toggle.rs`): several tests below
/// flip the theme, and without pinning the baseline the outcome would depend on the machine's own
/// dark/light setting.
fn shot(dir: &Path, doc: &Path, tag: &str, extra: &[&str]) -> Vec<u8> {
    let out = dir.join(format!("{tag}.png"));
    let status = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .env("ST2K_THEME", "dark")
        .arg("--shot")
        .arg(&out)
        .args(["--window", "preview", "--file"])
        .arg(doc)
        .args(extra)
        .status()
        .expect("spawn SageThumbs2K --shot");
    assert!(
        status.success(),
        "{tag} shot of {doc:?} (flags {extra:?}) failed: exit {:?}",
        status.code(),
    );
    let bytes = std::fs::read(&out).unwrap_or_else(|e| panic!("{tag} shot wrote no PNG: {e}"));
    assert!(!bytes.is_empty(), "{tag} shot wrote an empty PNG");
    bytes
}

// ===== Mouse: a real press+release drives the toolbar hit-test, not just do_action =====

/// `on_lbuttondown` (`window/mouse.rs`) hit-tests the click point against `button_rects` and
/// only THEN calls `do_action` — `--toggle-source`/`--toggle-theme` skip straight to the second
/// half. This proves the first half too: a real `WM_LBUTTONDOWN`+`WM_LBUTTONUP` posted at the
/// Theme button's own on-screen rect (`BTNS[5]`, the same numbering `--hot`/`--focus` use — see
/// CLAUDE.md §6) must land on exactly that button and produce the identical result
/// `--toggle-theme`'s direct call does.
#[test]
fn mouse_click_drives_the_real_toolbar_hit_test() {
    let case = "click_theme";
    let dir = scratch(case);
    let doc = dir.join("note.md");
    std::fs::write(
        &doc,
        "# Click drive\n\nBody text for the theme button click test.\n",
    )
    .expect("write sample");

    let baseline = shot(&dir, &doc, "baseline", &[]);
    let via_toggle = shot(&dir, &doc, "toggle", &["--toggle-theme"]);
    let via_click = shot(&dir, &doc, "click", &["--click", "5"]);

    assert_ne!(
        baseline, via_click,
        "a real WM_LBUTTONDOWN/WM_LBUTTONUP posted at the Theme button's own rect changed \
         nothing — either hit_button missed the button or on_lbuttondown never reached \
         do_action",
    );
    assert_eq!(
        via_toggle, via_click,
        "clicking the Theme button through its real on-screen rect (on_lbuttondown's hit-test) \
         produced a DIFFERENT capture than calling do_action(Btn::Theme) directly — the mouse \
         dispatch and the action it is supposed to trigger have come apart",
    );
    cleanup(case);
}

// ===== Keyboard: a real Ctrl+ toolbar shortcut, through the modifier-aware dispatch =====

/// Ctrl+U (`window/keys.rs::keydown_edit_actions`) is the keyboard twin of the `{ }` view-source
/// toolbar button, and reads its modifier from `GetKeyState`, not the message itself — exactly
/// the class of bug `pdf_wheel_action`'s doc comment describes (a routing decision only a real
/// message can reach). `--press ctrl+u` genuinely holds `VK_CONTROL` via `keybd_event` (the same
/// technique `--wheel --ctrl` already relies on) before posting `WM_KEYDOWN 'U'`, so this proves
/// the shortcut through the dispatcher itself rather than by calling `toggle_source` directly.
#[test]
fn ctrl_u_shortcut_matches_the_source_preset_through_the_real_key_path() {
    let case = "ctrl_u";
    let dir = scratch(case);
    let doc = dir.join("note.md");
    std::fs::write(
        &doc,
        "# Ctrl+U drive\n\nBody text, a `code span`, and a list:\n\n- one\n- two\n",
    )
    .expect("write sample");

    let rendered = shot(&dir, &doc, "rendered", &[]);
    let preset_source = shot(&dir, &doc, "preset", &["--source"]);
    let pressed_source = shot(&dir, &doc, "pressed", &["--press", "ctrl+u"]);

    assert_ne!(
        rendered, pressed_source,
        "a real Ctrl-held + 'U' WM_KEYDOWN changed nothing — the toolbar shortcut never reached \
         keydown_edit_actions/toggle_source",
    );
    assert_eq!(
        preset_source, pressed_source,
        "the REAL Ctrl+U key path rendered something different from the --source preset, though \
         both are documented to land the viewer in the exact same source-view state",
    );
    cleanup(case);
}

// ===== Keyboard: Escape reaches the FIND cluster ahead of window lifecycle =====

/// `find::on_key`'s Escape arm is checked inside `keydown_find_and_extend`, well ahead of
/// `keydown_lifecycle` in `on_keydown`'s dispatch order — so this is a genuinely different
/// Escape path from the one the live cross-process test below exercises (that one only fires
/// when the find bar is closed). A real Escape `WM_KEYDOWN` posted while the bar is open must
/// close it.
#[test]
fn escape_closes_the_find_bar_through_the_real_key_path() {
    let case = "escape_find";
    let dir = scratch(case);
    let doc = dir.join("note.txt");
    std::fs::write(&doc, "needle in a haystack of ordinary lines\n".repeat(10))
        .expect("write sample");

    let bar_open = shot(&dir, &doc, "open", &["--find", "needle"]);
    let bar_closed = shot(
        &dir,
        &doc,
        "closed",
        &["--find", "needle", "--press", "escape"],
    );

    assert_ne!(
        bar_open, bar_closed,
        "a real Escape WM_KEYDOWN posted while the find bar was open changed nothing — \
         find::on_key's Escape arm did not close it",
    );
    cleanup(case);
}

// ===== Mouse: WM_CAPTURECHANGED must actually end a drag, not just get posted =====

/// `on_capturechanged`'s own doc comment says why it exists: "end every drag so a buttonless
/// mouse-move can't keep seeking/panning/selecting". This drives that exact scenario over a
/// text-selection drag and proves the SECOND move's effect, not merely that a capture-changed
/// message can be posted without crashing:
///
/// - `stop2`: down at leg 1, move to leg 2, then a redundant move to the SAME point (leg 2
///   again) — the selection this produces is "as if the drag had stopped exactly at leg 2".
/// - `interrupted`: down at leg 1, move to leg 2, a real `WM_CAPTURECHANGED`, THEN move to a
///   leg 3 far down the document. If `on_capturechanged` correctly cleared `sel_drag`, this
///   third move is hover tracking, not a drag extension, so the result must equal `stop2`.
/// - `uninterrupted`: the same two legs to leg 3, WITHOUT the capture-changed — the drag is
///   still active, so the selection must have grown past `stop2`.
///
/// No `WM_LBUTTONUP` is ever posted in any of the three, so the only thing that can explain a
/// difference is whether the capture was still live for that final move.
#[test]
fn capture_changed_stops_a_selection_drag_from_extending_further() {
    let case = "capture_changed";
    let dir = scratch(case);
    let doc = dir.join("lines.txt");
    let mut body = String::new();
    for n in 1..=60 {
        body.push_str(&format!("line {n:02} of the drag-fixture body text\n"));
    }
    std::fs::write(&doc, &body).expect("write sample");

    // Fixed window-client coordinates: safely inside the default 720x480 off-screen shot
    // window's content pane (below CAPTION_H=36 either way), and the 60-line fixture reaches
    // well past y=400 regardless of exact font metrics.
    let stopped_at_leg2 = shot(&dir, &doc, "stop2", &["--drag", "20,50,20,90,20,90"]);
    let interrupted = shot(
        &dir,
        &doc,
        "interrupted",
        &["--drag", "20,50,20,90,20,400,interrupt"],
    );
    let uninterrupted = shot(
        &dir,
        &doc,
        "uninterrupted",
        &["--drag", "20,50,20,90,20,400"],
    );

    assert_eq!(
        stopped_at_leg2, interrupted,
        "a WM_CAPTURECHANGED between the two drag legs did NOT stop the second WM_MOUSEMOVE \
         from still extending the selection — on_capturechanged is not clearing sel_drag, so a \
         buttonless mouse-move (capture stolen mid-drag, e.g. alt-tab) would keep dragging \
         forever, exactly the bug its own doc comment says it exists to prevent",
    );
    assert_ne!(
        stopped_at_leg2, uninterrupted,
        "the same second move, WITHOUT a capture-changed first, produced the same capture as \
         stopping at leg 2 — the drag-fixture coordinates never reached different text, so this \
         test proves nothing either way; widen the leg-3 offset or the fixture's line count",
    );
    cleanup(case);
}

// ===== Resize: a real SetWindowPos -> WM_SIZE -> repaint actually re-flows content =====

/// `--size` already drives the real resize path (`shot.rs::apply_size`'s own doc comment), but
/// nothing in this suite compares two widths yet. A long unbroken paragraph must visibly re-wrap
/// between a narrow and a wide window, which only happens if `on_size` (`window.rs`) actually
/// invalidates and the next paint re-measures at the new client width.
#[test]
fn resize_reflows_the_document_through_a_real_wm_size() {
    let case = "resize";
    let dir = scratch(case);
    let doc = dir.join("note.md");
    std::fs::write(&doc, format!("# Resize drive\n\n{}\n", "word ".repeat(120)))
        .expect("write sample");

    let narrow = shot(&dir, &doc, "narrow", &["--size", "420x500"]);
    let wide = shot(&dir, &doc, "wide", &["--size", "900x500"]);

    assert_ne!(
        narrow, wide,
        "a real SetWindowPos -> WM_SIZE -> repaint at two different widths produced the same \
         capture — the paragraph never re-flowed, so on_size's behaviour is unverified here",
    );
    cleanup(case);
}

// ===== Live cross-process: navigation keys + Escape, through the real OS message loop =====

/// Quick preview viewer's window class (`preview/mod.rs::VIEWER_CLASS`).
const VIEWER_CLASS: &str = "SageThumbs2KViewer";

fn find_viewer() -> Option<HWND> {
    let wide: Vec<u16> = VIEWER_CLASS
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    unsafe { FindWindowW(PCWSTR(wide.as_ptr()), PCWSTR::null()).ok() }
}

/// Poll for the viewer window, up to `timeout`. `FindWindowW` alone matches a hidden window too
/// (the class exists from `CreateWindowExW` on) — same reasoning as
/// `tests/preview_async_load.rs::wait_for_viewer`.
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

unsafe fn window_title(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf);
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn post_key(hwnd: HWND, vk: u16) {
    let _ = unsafe { PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(vk as usize), LPARAM(1)) };
}

/// Poll the window title for `expected`, up to `timeout` — `window::set_title` publishes the
/// current file's leaf name, so this is the observable channel a real navigation key's effect
/// shows up on. Never a fixed sleep: the load a key triggers is not synchronous with the key
/// being dispatched (F10's async load applies to the initial open; navigation reads + repaints
/// on the same worker-hand-off shape).
fn wait_for_title(hwnd: HWND, expected: &str, timeout: Duration) -> bool {
    let start = Instant::now();
    loop {
        if unsafe { window_title(hwnd) } == expected {
            return true;
        }
        if start.elapsed() > timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

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

/// Drives a LIVE `--preview <path>` window (not `--shot`, which is decoded synchronously and
/// never runs the lifecycle/navigation clusters at all) across TWO real files, then closes it —
/// all through genuine `WM_KEYDOWN`s posted cross-process into the real wndproc's real message
/// loop, exactly as a physical key press would arrive.
///
/// Covers: `keydown_page_nav` -> `nav_key_action` -> `nav_sibling` in both directions (Right
/// THEN Left, so a regression that only breaks one direction cannot hide behind the other), and
/// `keydown_lifecycle`'s Escape -> `request_close` -> `DestroyWindow` -> `WM_QUIT`.
///
/// Needs a real window station: `--preview` shows an actual window (`ensure_shown`), and
/// `FindWindowW`/`PostMessageW` need it to exist. Same requirement as the `--shot` tests above,
/// same lack of a skip check — see this file's header.
#[test]
fn navigation_keys_and_escape_drive_the_live_window_through_the_real_os_message_loop() {
    let case = "nav_escape";
    let dir = scratch(case);
    let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
    std::fs::write(&a, "file a\n").expect("write a.txt");
    std::fs::write(&b, "file b\n").expect("write b.txt");

    // A scratch HKCU subkey — never the developer's real `HKCU\Software\SageThumbs2K` — so this
    // test's outcome never depends on whether Quick preview or close-on-focus-loss happen to
    // already be on. See settings::hkcu_root's `ST2K_SETTINGS_ROOT` redirect and
    // `scripts/make-shots.ps1`'s identical use of it. Left EMPTY on purpose: an absent
    // `PreviewEnabled` reads as false (the documented default), which is what makes
    // `window::create_viewer`'s `manual` flag true for a `--preview` launch (`manual =
    // shot.is_none() && !preview_enabled()`) — Escape only closes the window when `manual` is
    // true.
    let settings_root = format!(
        r"Software\SageThumbs2K\__test_wndproc_drive_{}_{case}",
        std::process::id()
    );
    CURRENT_USER
        .create(&settings_root)
        .expect("create scratch HKCU key");

    let mut child = Command::new(env!("CARGO_BIN_EXE_SageThumbs2K"))
        .env("ST2K_SETTINGS_ROOT", &settings_root)
        .arg("--preview")
        .arg(&a)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn SageThumbs2K --preview");

    let hwnd = match wait_for_viewer(Duration::from_secs(10)) {
        Some(h) => h,
        None => {
            let _ = child.kill();
            let _ = CURRENT_USER.remove_tree(&settings_root);
            cleanup(case);
            panic!("preview window never became visible");
        }
    };

    assert!(
        wait_for_title(hwnd, "a.txt", Duration::from_secs(5)),
        "the viewer never published its initial file's name in its window title"
    );

    // The REAL key path: WM_KEYDOWN VK_RIGHT into the live window's own wndproc, driving
    // on_keydown -> keydown_page_nav -> nav_key_action -> nav_sibling.
    post_key(hwnd, VK_RIGHT.0);
    if !wait_for_title(hwnd, "b.txt", Duration::from_secs(5)) {
        let _ = child.kill();
        let _ = CURRENT_USER.remove_tree(&settings_root);
        cleanup(case);
        panic!(
            "a real WM_KEYDOWN VK_RIGHT never navigated the live viewer to the next sibling file"
        );
    }

    post_key(hwnd, VK_LEFT.0);
    if !wait_for_title(hwnd, "a.txt", Duration::from_secs(5)) {
        let _ = child.kill();
        let _ = CURRENT_USER.remove_tree(&settings_root);
        cleanup(case);
        panic!(
            "a real WM_KEYDOWN VK_LEFT never navigated the live viewer back to the previous file"
        );
    }

    // Escape: keydown_lifecycle's request_close. Unlike the WM_ACTIVATE close-on-focus-loss
    // path, which SETTLE_CLOSE_MS gates behind an open grace window (tests/preview_async_load.rs),
    // Escape's own arm carries no settle timer — it fires the moment `st.manual` is true, which
    // the title wait above already proves the window has had ample time to become. Waited for by
    // polling the PROCESS EXIT, never a fixed sleep.
    post_key(hwnd, VK_ESCAPE.0);
    let exited = wait_for_exit(&mut child, Duration::from_secs(5));
    let _ = CURRENT_USER.remove_tree(&settings_root);
    match exited {
        Some(status) => assert!(status.success(), "viewer exited non-zero after Escape"),
        None => {
            let _ = child.kill();
            cleanup(case);
            panic!(
                "a real WM_KEYDOWN VK_ESCAPE never closed the live viewer window \
                 (keydown_lifecycle's request_close did not fire, or DestroyWindow never posted \
                 WM_QUIT)"
            );
        }
    }
    cleanup(case);
}
