//! Headless --shot plumbing: an off-screen window, pumped, painted and captured.

use super::*;

/// Drain the message queue `frames` times (tiny sleep between) so async WM_PAINT / timer /
/// layout work settles before a headless PrintWindow capture. Shared by every `--shot` path.
pub(crate) unsafe fn pump_msgs(frames: usize) {
    let mut msg = MSG::default();
    for _ in 0..frames {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        std::thread::sleep(std::time::Duration::from_millis(16));
    }
}

/// Take the foreground and the keyboard focus, even when Windows says no.
///
/// Our full-screen overlays (the screenshot capture, the eyedropper) are
/// `WS_POPUP`/`WS_EX_NOACTIVATE` windows spawned by the background hotkey daemon.
/// A process that did not itself just receive input is NOT allowed to steal the
/// foreground, so `SetForegroundWindow` is routinely refused. The window still
/// appears — it is topmost — and still receives MOUSE messages, so everything
/// looks fine; but it never gets keyboard focus, and every keystroke goes to
/// whatever app was in front. The symptom the user sees is "Esc does not close
/// the screenshot" (owner report, 2026-07-31).
///
/// Attaching our input queue to the current foreground thread makes us share its
/// input state, which is the documented way to be granted the change. We detach
/// immediately: staying attached would couple our message loop to another app's,
/// so its hangs would become ours.
pub(crate) unsafe fn force_foreground(hwnd: HWND) {
    let _ = SetForegroundWindow(hwnd);
    let _ = SetActiveWindow(hwnd);
    let _ = SetFocus(Some(hwnd));
    if GetForegroundWindow() == hwnd {
        return;
    }
    let fg = GetForegroundWindow();
    if fg.0.is_null() {
        return;
    }
    let fg_tid = GetWindowThreadProcessId(fg, None);
    let me = windows::Win32::System::Threading::GetCurrentThreadId();
    if fg_tid == 0 || fg_tid == me {
        return;
    }
    let _ = windows::Win32::System::Threading::AttachThreadInput(fg_tid, me, true);
    let _ = SetForegroundWindow(hwnd);
    let _ = SetActiveWindow(hwnd);
    let _ = SetFocus(Some(hwnd));
    let _ = windows::Win32::System::Threading::AttachThreadInput(fg_tid, me, false);
}

/// Force a SYNCHRONOUS paint of `hwnd` AND every child (RDW_UPDATENOW). Owner-drawn statics
/// (nav rail, pane header, toggle switches) only paint on a real WM_PAINT, so without this a
/// headless capture races them and leaves blank gaps.
pub(crate) unsafe fn force_repaint(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{
        RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
    };
    let _ = RedrawWindow(
        Some(hwnd),
        None,
        None,
        RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
}

/// Create a top-level dialog window ON-SCREEN but fully transparent (WS_EX_LAYERED alpha 0)
/// and non-activated — a real window that is invisible and steals no focus — for headless
/// `PrintWindow` capture. Same class
/// registration + dark styling as [`run_dialog`], but returns the HWND WITHOUT a message
/// loop: the caller pumps ([`pump_msgs`]), captures, and `DestroyWindow`s it. `design_w/h`
/// are 96-dpi design pixels (scaled to the DPI of the monitor under the cursor, or the
/// `--dpi` override, here).
pub(crate) unsafe fn create_shot_window(
    hinst: HINSTANCE,
    dark: bool,
    class: PCWSTR,
    wndproc: WNDPROC,
    title: &str,
    design_w: i32,
    design_h: i32,
) -> Option<HWND> {
    register_app_class(class, wndproc, hinst); // same tone as the real window classes

    // Position it ON-SCREEN (centered on the cursor monitor), NOT off the virtual desktop: an
    // off-screen window's DWM redirection surface can be stale/blank when PrintWindow grabs it
    // (that raced the capture — some frames came out blank or showed the previous tab). DWM keeps
    // an on-screen window's surface current. `WS_EX_LAYERED` + alpha 0 makes it fully transparent
    // → invisible to the user, while PrintWindow still captures the real (opaque) content;
    // SW_SHOWNOACTIVATE + tool-window means it steals no focus and shows no taskbar entry. Sizing
    // to the cursor monitor's DPI also matches the per-control layout DPI (GetDpiForWindow).
    //
    // A `--dpi N` shot override wins over the monitor's real DPI: every `ctl()` in this window
    // lays its children out via `dpi_scale`, which already honors the override (via
    // `effective_dpi`), but until this line the WINDOW FRAME here was sized from the real
    // monitor DPI regardless, so a forced high-DPI capture laid out children for e.g. 192 DPI
    // inside a frame still sized for the dev box's real 96, and every control past the top-left
    // corner rendered outside the captured window (2026-09-05 audit F36's DPI coverage, caught
    // by actually capturing at `--dpi 192`, not by reasoning about it).
    let (mon_dpi, work) = cursor_monitor_metrics();
    let dpi = dpi_override().unwrap_or(mon_dpi);
    let (sw, sh) = (dpi_scale_dpi(design_w, dpi), dpi_scale_dpi(design_h, dpi));
    let x = work.left + ((work.right - work.left) - sw).max(0) / 2;
    let y = work.top + ((work.bottom - work.top) - sh).max(0) / 2;
    let title_w = wide(title);
    let hwnd = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
        class,
        PCWSTR(title_w.as_ptr()),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_CLIPCHILDREN,
        x,
        y,
        sw,
        sh,
        None,
        None,
        Some(hinst),
        None,
    )
    .ok()?;
    // Fully transparent (alpha 0) → composited by DWM but invisible on screen.
    let _ = SetLayeredWindowAttributes(hwnd, windows::Win32::Foundation::COLORREF(0), 0, LWA_ALPHA);
    if dark {
        crate::dark::dark_control(hwnd, w!("DarkMode_Explorer"));
        crate::dark::dark_titlebar(hwnd);
    }
    let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
    Some(hwnd)
}

/// Everything [`capture_shot_window`] needs to build its headless dialog window via
/// [`create_shot_window`], bundled into one value so callers don't hand five loose
/// parameters through every `run_shot_*`.
pub(crate) struct ShotWindowSpec<'a> {
    pub(crate) class: PCWSTR,
    pub(crate) wndproc: WNDPROC,
    pub(crate) title: &'a str,
    pub(crate) design_w: i32,
    pub(crate) design_h: i32,
}

/// The ritual eight `run_shot_*` functions (About, Convert, the Convert failure report,
/// Doctor, Send-feedback, both first-run pages, the OCR result window) used to hand-repeat:
/// resolve this process's own module handle, build `spec`'s window on-screen (transparent) via
/// [`create_shot_window`], run `after_create` for whatever that one dialog needs done to the
/// fresh window before it settles (About grows the frame back to its design client size,
/// first-run page 2 flips itself to page 2, most pass a no-op), settle with [`settle_pump`],
/// capture to `out`, and destroy the window. Returns whether the PNG was written.
///
/// The Settings window (its priming needs a REAL category transition, not a fixed pump count,
/// see `settings_dlg::shot::settle_pane`) and the eyedropper overlay (a fullscreen
/// `WS_POPUP` built off the virtual desktop, not a dialog frame `create_shot_window`
/// produces) build their own window and don't call this, but both still settle and capture
/// through [`settle_pump`]/[`capture_and_destroy`] below, so that half of the ritual still
/// lives in exactly one place.
pub(crate) unsafe fn capture_shot_window(
    out: &str,
    dark: bool,
    spec: ShotWindowSpec<'_>,
    after_create: impl FnOnce(HWND, HINSTANCE),
    pump1: usize,
    pump2: usize,
    skip_final_repaint: bool,
) -> bool {
    let Ok(h) = GetModuleHandleW(None) else {
        return false;
    };
    let hinst: HINSTANCE = h.into();
    let Some(hwnd) = create_shot_window(
        hinst,
        dark,
        spec.class,
        spec.wndproc,
        spec.title,
        spec.design_w,
        spec.design_h,
    ) else {
        return false;
    };
    after_create(hwnd, hinst);
    settle_pump(hwnd, pump1, pump2, skip_final_repaint);
    capture_and_destroy(hwnd, out)
}

/// The settle shape every headless `--shot` capture uses before grabbing the frame: pump
/// `pump1` frames, force a repaint, pump `pump2` frames, then, unless `skip_final_repaint`,
/// force a second repaint. Only the counts and the final repaint differ per dialog (About's
/// "Checking…" spinner needs ~2s of pumping to settle past `MIN_SPIN_FRAMES`; the Convert
/// failure report is the one capture that skips the final repaint), so those stay parameters
/// rather than being assumed.
pub(crate) unsafe fn settle_pump(hwnd: HWND, pump1: usize, pump2: usize, skip_final_repaint: bool) {
    pump_msgs(pump1);
    force_repaint(hwnd);
    pump_msgs(pump2);
    if !skip_final_repaint {
        force_repaint(hwnd);
    }
}

/// `PrintWindow`-capture `hwnd` to a PNG at `out` and destroy it - the tail every headless
/// `--shot` capture shares, whatever built the window.
pub(crate) unsafe fn capture_and_destroy(hwnd: HWND, out: &str) -> bool {
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}
