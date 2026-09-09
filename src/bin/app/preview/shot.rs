//! Headless --shot capture harness.

use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::window::create_viewer;

// ===== Headless shot =====

/// Build the viewer off-screen on `input` (or a synthetic gradient), render it to `out` via
/// `PrintWindow`, tear it down. Returns whether the PNG was written.
pub(super) unsafe fn run_shot(
    hinst: HINSTANCE,
    dark: bool,
    out: &str,
    opts: &super::ShotOpts,
) -> bool {
    if let Some(dpi) = opts.dpi {
        crate::win::set_dpi_override(dpi); // headless high-DPI capture (no physical high-DPI monitor needed)
    }
    let tmp = if opts.file.is_none() {
        write_synthetic_png()
    } else {
        None
    };
    let path = opts.file.clone().or_else(|| tmp.clone());
    let Some(hwnd) = create_viewer(hinst, dark, path, Some(opts)) else {
        if let Some(t) = &tmp {
            let _ = std::fs::remove_file(t);
        }
        return false;
    };
    // The deferred first-show timer must NOT fire mid-capture — an off-screen shot that
    // suddenly shows/resizes the window mid-PrintWindow produces torn frames (white bands).
    let _ = KillTimer(Some(hwnd), super::window::SHOW_TIMER_ID);
    if opts.play {
        // Give Media Foundation time to reach first-frame so the strip has a real duration.
        for _ in 0..200 {
            crate::win::pump_msgs(8);
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    } else {
        crate::win::pump_msgs(8);
    }
    apply_wait_ms(hwnd, opts.wait_ms);
    apply_size(hwnd, opts.size);
    apply_wheel(hwnd, opts.wheel, opts.wheel_ctrl, opts.wheel_shift);
    apply_scroll(hwnd, opts.scroll);
    if opts.toggle_source {
        // Drive the REAL toolbar-click path (`do_action(Btn::Source)`), not the `--source`
        // preset, so the click-driven reload is headlessly verifiable. This is the only way to
        // exercise the toggle's reload from a shot: `--source` starts the window ALREADY in
        // source mode and never runs `toggle_source`, which is exactly how an edition-2021
        // RefCell borrow bug in it survived a green test run once. Pump afterwards so the
        // re-load's paint lands before the capture.
        super::window::do_action(hwnd, super::window::Btn::Source);
        crate::win::pump_msgs(8);
    }
    if opts.toggle_theme {
        // Same reasoning as `--toggle-source`: press the REAL button so the capture proves the
        // whole path - the palette override, the re-applied window frame, and the reload that
        // rebuilds the letterboxed bitmap - rather than a window that merely opened in the
        // other skin.
        super::window::do_action(hwnd, super::window::Btn::Theme);
        crate::win::pump_msgs(8);
    }
    apply_focus(hwnd, opts.focus, opts.focus_transport);
    apply_sel(hwnd, opts.sel);
    apply_find(hwnd, opts.find.as_deref());
    // Not on `ShotOpts` (that struct is threaded through `main.rs`'s CLI parser, owned
    // elsewhere): read straight off argv instead. Exists so `tests/preview_wndproc_drive.rs`
    // can drive the mouse-button and keyboard-dispatch paths a plain build cannot prove —
    // see each helper's doc comment for exactly which real message path it posts.
    apply_click(hwnd, env_arg("--click").and_then(|s| s.parse().ok()));
    apply_drag(hwnd, env_arg("--drag").as_deref().and_then(parse_drag));
    apply_press(hwnd, env_arg("--press").as_deref());
    bench_repaint_if_requested(hwnd);
    let ok = crate::win::capture_and_destroy(hwnd, out);
    if let Some(t) = &tmp {
        let _ = std::fs::remove_file(t);
    }
    ok
}

/// `--wait-ms N`: prime one direct paint (the window is invisible, so `WM_PAINT` never
/// fires on its own; this is what spawns async work like a remote-image fetch), then
/// pump so the posted results install before the capture.
unsafe fn apply_wait_ms(hwnd: HWND, wait_ms: Option<u64>) {
    let Some(ms) = wait_ms else {
        return;
    };
    let dc = GetDC(Some(hwnd));
    if !dc.is_invalid() {
        super::paint::paint_into(hwnd, dc);
        ReleaseDC(Some(hwnd), dc);
    }
    let ticks = ms.div_ceil(10);
    for _ in 0..ticks {
        crate::win::pump_msgs(8);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// `--size W,H`: drive the REAL resize path (SetWindowPos -> WM_SIZE -> invalidate ->
/// repaint), so a shot proves the content actually re-flows at the new width rather than
/// just that it laid out correctly once.
unsafe fn apply_size(hwnd: HWND, size: Option<(i32, i32)>) {
    let Some((w, h)) = size else {
        return;
    };
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        w.max(120),
        h.max(120),
        SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
    );
    crate::win::pump_msgs(8);
}

/// `--wheel N [--ctrl|--shift]`: the REAL message path, `SendMessageW` straight into the
/// viewer's own wndproc with the modifier keys genuinely held down, so this exercises the
/// routing a user's wheel goes through rather than the function it eventually calls. That
/// distinction is not academic - 2.3.1 shipped a PDF whose wheel was inert, and it passed
/// every test because they all called the scroll function directly.
unsafe fn apply_wheel(hwnd: HWND, wheel: Option<i32>, ctrl: bool, shift: bool) {
    let Some(notches) = wheel else {
        return;
    };
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYEVENTF_KEYUP, VK_CONTROL, VK_SHIFT,
    };
    let mods: &[u8] = match (ctrl, shift) {
        (true, true) => &[VK_CONTROL.0 as u8, VK_SHIFT.0 as u8],
        (true, false) => &[VK_CONTROL.0 as u8],
        (false, true) => &[VK_SHIFT.0 as u8],
        (false, false) => &[],
    };
    for &vk in mods {
        keybd_event(vk, 0, Default::default(), 0);
    }
    let step = if notches < 0 { -120 } else { 120 };
    for _ in 0..notches.abs() {
        SendMessageW(
            hwnd,
            WM_MOUSEWHEEL,
            Some(WPARAM(((step as i16 as u16 as usize) << 16) & 0xFFFF_0000)),
            Some(LPARAM(0)),
        );
    }
    for &vk in mods {
        keybd_event(vk, 0, KEYEVENTF_KEYUP, 0);
    }
    // Zooming DROPS every tile and re-renders at the new width, and those arrive on worker
    // threads. One pump captures the moment where the pages are still blank placeholders,
    // which reads as "zoom broke the view" when it only means the shot was too quick.
    for _ in 0..150 {
        crate::win::pump_msgs(8);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// `--scroll N`: a continuously scrolled PDF takes it through its REAL scroll path, clamp
/// and all, so a shot proves the layout at that position rather than just that page one
/// drew (needs the session to have landed, i.e. `--wait-ms` first); the text/Markdown pane
/// gets its scroll offset poked directly (an overshoot just shows the bottom - the wheel
/// handler's clamp doesn't run here, but paint clips like any big scroll).
unsafe fn apply_scroll(hwnd: HWND, scroll: Option<i32>) {
    let Some(scroll) = scroll else {
        return;
    };
    if super::pdfview::active(hwnd) {
        super::pdfview::scroll_by(hwnd, scroll);
        crate::win::pump_msgs(8);
    }
    let stp = super::window::state(hwnd);
    if !stp.is_null() {
        (*stp).text_scroll.set(scroll.max(0));
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
        crate::win::pump_msgs(8);
    }
}

/// `--focus N` / `--focus-transport N`: set the toolbar's keyboard focus onto a specific
/// button after loading, exactly as pressing Tab/arrow keys would land on it, so
/// `paint::draw_toolbar_focus_ring` is shot-verifiable without driving real key input.
///
/// `--focus N` takes a caption-toolbar `BTNS` index — the SAME numbering `--hot N` uses — not
/// the position `toolbar::FocusTarget::Caption` actually stores (an index into the CURRENT
/// visible, right-to-left `button_rects`, which shrinks/reorders per document); this
/// translates one into the other by locating `BTNS[N]` in `toolbar::button_rects(hwnd)`. A
/// hidden button (see `window::btn_visible`) is simply not found and focus is left untouched,
/// same as `--hot` on a hidden index does nothing visible.
///
/// `--focus-transport N` indexes `transport::TBTNS` directly (0..7) — the strip carries no
/// per-document visibility filter, so it needs no translation.
unsafe fn apply_focus(hwnd: HWND, focus: Option<usize>, focus_transport: Option<usize>) {
    if focus.is_none() && focus_transport.is_none() {
        return;
    }
    let stp = super::window::state(hwnd);
    if stp.is_null() {
        return;
    }
    let st = &*stp;
    if let Some(n) = focus {
        if let Some(&btn) = super::window::BTNS.get(n) {
            let rects = super::toolbar::button_rects(hwnd);
            if let Some(i) = rects.iter().position(|(b, _)| *b == btn) {
                super::toolbar::set_focus(hwnd, st, Some(super::toolbar::FocusTarget::Caption(i)));
            }
        }
    }
    if let Some(n) = focus_transport {
        if n < super::transport::TBTNS.len() {
            super::toolbar::set_focus(hwnd, st, Some(super::toolbar::FocusTarget::Transport(n)));
        }
    }
    crate::win::pump_msgs(8);
}

/// `--sel A,B`: force a text-pane selection before capture (verifies the highlight
/// headlessly).
unsafe fn apply_sel(hwnd: HWND, sel: Option<(usize, usize)>) {
    let Some(sel) = sel else {
        return;
    };
    let stp = super::window::state(hwnd);
    if !stp.is_null() {
        (*stp).sel.set(Some(sel));
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
        crate::win::pump_msgs(8);
    }
}

/// `--find TEXT`: open the find bar on `q` and land on its first match. Driven through the
/// REAL key path (Ctrl+F then a `WM_CHAR` per character) rather than by poking the state,
/// so the shot proves the whole chain: the bar opening, the pane shrinking around it, and
/// the match highlight.
unsafe fn apply_find(hwnd: HWND, find: Option<&str>) {
    let Some(q) = find else {
        return;
    };
    super::find::toggle(hwnd);
    for c in q.chars() {
        super::find::on_char(hwnd, c as u32);
    }
    // A PDF's text is READ off its rendered pages on worker threads, so a search over one is
    // not finished when the last character is typed - it has barely started. Without this,
    // every shot of a PDF search would capture the same "no results yet" frame and prove
    // nothing at all about whether the search works.
    if super::pdfview::active(hwnd) {
        for _ in 0..600 {
            crate::win::pump_msgs(8);
            if let Some(ix) = super::pdfview::index_snapshot(hwnd) {
                if ix.total > 0 && ix.done >= ix.total {
                    break;
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }
    let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
    crate::win::pump_msgs(8);
}

/// Read one flag's value straight off argv, independent of `ShotOpts` (which `main.rs` owns).
/// Used only by the driving flags below, which exist purely for
/// `tests/preview_wndproc_drive.rs` and have no reason to widen a struct threaded through code
/// this file doesn't own.
fn env_arg(flag: &str) -> Option<String> {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == flag)
        .and_then(|p| args.get(p + 1))
        .cloned()
}

/// Pack a client-space point into the `LPARAM` encoding a mouse message carries it in — the
/// same bit layout `tests/screenshot_automation.rs`'s external driver uses for the same reason.
fn point_lparam(x: i32, y: i32) -> windows::Win32::Foundation::LPARAM {
    let packed = u32::from(x as u16) | (u32::from(y as u16) << 16);
    windows::Win32::Foundation::LPARAM(packed as isize)
}

/// `--click N`: press-AND-release the same toolbar button `--hot`/`--focus` index into
/// (`BTNS[N]`), via a real `WM_LBUTTONDOWN` + `WM_LBUTTONUP` posted at its actual on-screen
/// rect. Unlike `--toggle-source`/`--toggle-theme`, which call `do_action` directly, this
/// proves the MOUSE HIT-TEST too (`on_lbuttondown` → `hit_button` → `button_rects`) — the
/// path a real click actually takes, and the one a hit-test regression could break while
/// every direct-`do_action` test stayed green.
unsafe fn apply_click(hwnd: HWND, click: Option<usize>) {
    use windows::Win32::Foundation::WPARAM;
    let Some(n) = click else {
        return;
    };
    let Some(&btn) = super::window::BTNS.get(n) else {
        return;
    };
    let rects = super::toolbar::button_rects(hwnd);
    let Some((_, r)) = rects.iter().find(|(b, _)| *b == btn) else {
        return;
    };
    let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
    let _ = PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(1), point_lparam(cx, cy));
    crate::win::pump_msgs(8);
    let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), point_lparam(cx, cy));
    crate::win::pump_msgs(8);
}

/// The fixed shape `--drag` parses: two legs of a mouse drag (`(x1,y1)` → `(x2,y2)` →
/// `(x3,y3)`) plus whether a `WM_CAPTURECHANGED` interrupts it between the legs.
type DragSpec = (i32, i32, i32, i32, i32, i32, bool);

/// Parse `--drag X1,Y1,X2,Y2,X3,Y3[,interrupt]` for [`apply_drag`].
fn parse_drag(spec: &str) -> Option<DragSpec> {
    let parts: Vec<&str> = spec.split(',').collect();
    if parts.len() != 6 && parts.len() != 7 {
        return None;
    }
    let n = |i: usize| parts[i].trim().parse::<i32>().ok();
    let (x1, y1, x2, y2, x3, y3) = (n(0)?, n(1)?, n(2)?, n(3)?, n(4)?, n(5)?);
    let interrupt = parts
        .get(6)
        .map(|s| s.trim() == "interrupt")
        .unwrap_or(false);
    Some((x1, y1, x2, y2, x3, y3, interrupt))
}

/// `--drag X1,Y1,X2,Y2,X3,Y3[,interrupt]`: a real content-pane selection drag over TWO legs —
/// `WM_LBUTTONDOWN` at `(X1,Y1)` starts it, `WM_MOUSEMOVE` to `(X2,Y2)` extends it — then,
/// with `interrupt`, a `WM_CAPTURECHANGED` (capture stolen mid-drag, e.g. alt-tab) BEFORE the
/// second `WM_MOUSEMOVE` to `(X3,Y3)`; without it, the second move runs straight through. No
/// `WM_LBUTTONUP` is ever posted, so the only thing that can explain a difference between the
/// two runs is `on_capturechanged`: does a SECOND move still extend the selection (the bug it
/// exists to prevent — a buttonless move left dragging forever) or does it correctly do
/// nothing once capture is gone.
unsafe fn apply_drag(hwnd: HWND, drag: Option<DragSpec>) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    let Some((x1, y1, x2, y2, x3, y3, interrupt)) = drag else {
        return;
    };
    let _ = PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(1), point_lparam(x1, y1));
    crate::win::pump_msgs(8);
    let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(1), point_lparam(x2, y2));
    crate::win::pump_msgs(8);
    if interrupt {
        let _ = PostMessageW(Some(hwnd), WM_CAPTURECHANGED, WPARAM(0), LPARAM(0));
        crate::win::pump_msgs(8);
    }
    let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(1), point_lparam(x3, y3));
    crate::win::pump_msgs(8);
}

/// `--press SPEC[,SPEC...]`: post each token as a real key through the exact `WM_KEYDOWN`
/// dispatch a live window receives. `ctrl+X` genuinely holds `VK_CONTROL` down first (the same
/// `keybd_event` technique `--wheel --ctrl` already relies on, since `on_keydown`'s modifier
/// reads come from `GetKeyState`, not the message itself), so a toolbar SHORTCUT is proven
/// through the same dispatch cluster a plain arrow key is, not merely called directly.
/// Recognised tokens: `left`/`right`/`up`/`down`/`home`/`end`/`escape`, plus `ctrl+<letter>`
/// (`u` = view source, `f` = find, `c` = copy, `a` = select all, `s` = save, `p` = print).
/// Unrecognised tokens are silently skipped.
unsafe fn apply_press(hwnd: HWND, press: Option<&str>) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        keybd_event, KEYEVENTF_KEYUP, VK_CONTROL, VK_DOWN, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT,
        VK_RIGHT, VK_UP,
    };
    let Some(spec) = press else {
        return;
    };
    let named_vk = |name: &str| -> Option<u16> {
        Some(match name {
            "left" => VK_LEFT.0,
            "right" => VK_RIGHT.0,
            "up" => VK_UP.0,
            "down" => VK_DOWN.0,
            "home" => VK_HOME.0,
            "end" => VK_END.0,
            "escape" | "esc" => VK_ESCAPE.0,
            s if s.len() == 1 && s.as_bytes()[0].is_ascii_alphabetic() => {
                s.as_bytes()[0].to_ascii_uppercase() as u16
            }
            _ => return None,
        })
    };
    for raw in spec.split(',') {
        let token = raw.trim().to_ascii_lowercase();
        if token.is_empty() {
            continue;
        }
        let (ctrl, key) = match token.split_once('+') {
            Some(("ctrl", k)) => (true, k),
            _ => (false, token.as_str()),
        };
        let Some(vk) = named_vk(key) else {
            continue;
        };
        if ctrl {
            keybd_event(VK_CONTROL.0 as u8, 0, Default::default(), 0);
        }
        let _ = PostMessageW(Some(hwnd), WM_KEYDOWN, WPARAM(vk as usize), LPARAM(1));
        crate::win::pump_msgs(8);
        if ctrl {
            keybd_event(VK_CONTROL.0 as u8, 0, KEYEVENTF_KEYUP, 0);
        }
        crate::win::pump_msgs(8);
    }
}

/// `ST2K_MD_BENCH`: repaint several times so the Markdown layout cache's cold(1st)-vs-
/// warm(rest) timings print - each `paint_into` re-runs `markdown::render`, which
/// self-times under the same env var. This is the only way to measure the SCROLL speedup,
/// since one PrintWindow capture is a single (cold) paint.
unsafe fn bench_repaint_if_requested(hwnd: HWND) {
    if std::env::var_os("ST2K_MD_BENCH").is_none() {
        return;
    }
    let dc = GetDC(Some(hwnd));
    if !dc.is_invalid() {
        for _ in 0..6 {
            super::paint::paint_into(hwnd, dc);
        }
        ReleaseDC(Some(hwnd), dc);
    }
}

/// Write a small synthetic gradient PNG to a temp file (fallback input for `--shot`).
pub(super) fn write_synthetic_png() -> Option<String> {
    let (w, h) = (640u32, 400u32);
    let mut img = image::RgbaImage::new(w, h);
    for (x, y, px) in img.enumerate_pixels_mut() {
        *px = image::Rgba([(x * 255 / w) as u8, (y * 255 / h) as u8, 160, 255]);
    }
    let path = std::env::temp_dir().join(format!("st2k_preview_shot_{}.png", std::process::id()));
    img.save(&path).ok()?;
    Some(path.to_string_lossy().into_owned())
}
