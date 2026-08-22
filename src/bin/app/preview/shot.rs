//! Headless --shot capture harness.

use windows::Win32::Foundation::HINSTANCE;
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
    if let Some(ms) = opts.wait_ms {
        // The window is invisible, so WM_PAINT never fires on its own — prime ONE direct paint
        // pass first (it's what spawns async work like remote image fetches), then pump so the
        // posted results install before the capture.
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
    if let Some((w, h)) = opts.size {
        // Drive the REAL resize path (SetWindowPos → WM_SIZE → invalidate → repaint), so a
        // shot proves the content actually re-flows at the new width rather than just that
        // it laid out correctly once.
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
    if let Some(notches) = opts.wheel {
        // The REAL message path: SendMessageW straight into the viewer's wndproc, with the
        // modifier keys genuinely held down, so this exercises the routing a user's wheel goes
        // through rather than the function it eventually calls. That distinction is not
        // academic - 2.3.1 shipped a PDF whose wheel was inert, and it passed every test
        // because they all called the scroll function directly.
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            keybd_event, KEYEVENTF_KEYUP, VK_CONTROL, VK_SHIFT,
        };
        let mods: &[u8] = match (opts.wheel_ctrl, opts.wheel_shift) {
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
    if let Some(scroll) = opts.scroll {
        // A continuously scrolled PDF takes `--scroll` through its REAL scroll path, clamp and
        // all, so a shot proves the layout at that position rather than just that page one
        // drew. This is the only headless way to see the scrolling view: it needs the session
        // to have landed, which means `--wait-ms` first.
        if super::pdfview::active(hwnd) {
            super::pdfview::scroll_by(hwnd, scroll);
            crate::win::pump_msgs(8);
        }
        // Scroll the text/Markdown pane before capture (an overshoot just shows the bottom —
        // the wheel handler's clamp doesn't run here, but paint clips like any big scroll).
        let stp = super::window::state(hwnd);
        if !stp.is_null() {
            (*stp).text_scroll.set(scroll.max(0));
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
            crate::win::pump_msgs(8);
        }
    }
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
    if let Some(sel) = opts.sel {
        // Force a text-pane selection before capture (verifies the highlight headlessly).
        let stp = super::window::state(hwnd);
        if !stp.is_null() {
            (*stp).sel.set(Some(sel));
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
            crate::win::pump_msgs(8);
        }
    }
    if let Some(q) = opts.find.as_deref() {
        // Open the find bar on `q` and land on its first match. Driven through the REAL key path
        // (Ctrl+F then a WM_CHAR per character) rather than by poking the state, so the shot proves
        // the whole chain: the bar opening, the pane shrinking around it, and the match highlight.
        super::find::toggle(hwnd);
        for c in q.chars() {
            super::find::on_char(hwnd, c as u32);
        }
        let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(hwnd), None, false);
        crate::win::pump_msgs(8);
    }
    if std::env::var_os("ST2K_MD_BENCH").is_some() {
        // Bench: repaint several times so the Markdown layout cache's cold(1st)-vs-warm(rest)
        // timings print — each paint_into re-runs markdown::render, which self-times under the
        // same env var. This is the only way to measure the SCROLL speedup, since one PrintWindow
        // capture is a single (cold) paint.
        let dc = GetDC(Some(hwnd));
        if !dc.is_invalid() {
            for _ in 0..6 {
                super::paint::paint_into(hwnd, dc);
            }
            ReleaseDC(Some(hwnd), dc);
        }
    }
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    if let Some(t) = &tmp {
        let _ = std::fs::remove_file(t);
    }
    ok
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
