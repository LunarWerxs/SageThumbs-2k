//! Taking the picture: freeze the screen into a DC, build the overlay state, run its message loop, flash.

use super::*;
use crate::eyedropper::virtual_screen_metrics;

/// Release the GDI objects of a failed full-screen setup: logs `msg` (the caller's
/// diagnosable abort message), then deletes the memory DC and bitmap and releases the
/// screen DC, each only if it was actually created.
pub(super) unsafe fn release_gdi_on_fail(screen: HDC, mem: HDC, bmp: HBITMAP, msg: &str) {
    sagethumbs2k_core::safety::log(msg);
    if !mem.is_invalid() {
        let _ = DeleteDC(mem);
    }
    if !bmp.is_invalid() {
        let _ = DeleteObject(bmp.into());
    }
    if !screen.is_invalid() {
        ReleaseDC(None, screen);
    }
}

/// Create the memory DC and its compatible bitmap for a virtual-screen-sized capture.
/// A GDI failure here (object-quota exhaustion is the realistic cause) must not fall
/// through to SelectObject/BitBlt on a null handle, which paints "you captured a black
/// screen" instead of a diagnosable failure (A139), so this logs `fail_msg`, releases
/// everything already allocated (`screen` included), and returns `None` instead.
unsafe fn create_fullscreen_dc_pair(
    screen: HDC,
    vw: i32,
    vh: i32,
    fail_msg: &str,
) -> Option<(HDC, HBITMAP)> {
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, vw, vh);
    if screen.is_invalid() || mem.is_invalid() || bmp.is_invalid() {
        release_gdi_on_fail(screen, mem, bmp, fail_msg);
        return None;
    }
    Some((mem, bmp))
}

/// Freeze the screen into a memory DC (the normal overlay paints from this, never the
/// live desktop, so annotations don't fight with what's underneath), or fill it with the
/// deterministic synthetic automation canvas, since the automation route MUST NOT copy or
/// sample the desktop. A GDI failure here (object-quota exhaustion is the realistic
/// cause) must not fall through to SelectObject/BitBlt on a null handle, which paints
/// "you captured a black screen" instead of a diagnosable failure (A139), so this logs,
/// releases everything already allocated (`screen` included), and returns `None` instead.
pub(super) unsafe fn freeze_screen_to_dc(
    screen: HDC,
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    automation: bool,
) -> Option<(HDC, HBITMAP)> {
    let (mem, bmp) = create_fullscreen_dc_pair(
        screen,
        vw,
        vh,
        "screenshot: full-screen GDI setup failed, aborting capture",
    )?;
    SelectObject(mem, HGDIOBJ(bmp.0));
    if automation {
        draw_automation_canvas(mem, vw, vh);
    } else if !hdr_capture(mem, vx, vy, vw, vh) {
        // No HDR monitor attached (or a build without the feature): the original
        // single blit, unchanged. The HDR path only engages when it has something
        // to fix.
        let _ = BitBlt(mem, 0, 0, vw, vh, Some(screen), vx, vy, SRCCOPY);
    }
    Some((mem, bmp))
}

/// A pre-dimmed copy of the frozen snapshot: paint blits this for the surround (no
/// per-frame alpha) and blits the bright `mem` through for the selection. Releases
/// `screen`, which nothing needs any more once both DCs hold their own copy of it.
pub(super) unsafe fn build_dimmed_copy(screen: HDC, mem: HDC, vw: i32, vh: i32) -> (HDC, HBITMAP) {
    let dim = CreateCompatibleDC(Some(screen));
    let dim_bmp = CreateCompatibleBitmap(screen, vw, vh);
    SelectObject(dim, HGDIOBJ(dim_bmp.0));
    let _ = BitBlt(dim, 0, 0, vw, vh, Some(mem), 0, 0, SRCCOPY);
    apply_dim(dim, vw, vh);
    ReleaseDC(None, screen);
    (dim, dim_bmp)
}

/// The default annotation text size's DPI: the monitor under the cursor at capture start
/// (no selection exists yet to source one from), so the starting default feels the same
/// physical size on a HiDPI display, even though the user-chosen size from here on stays
/// physical (it's baked into the saved/copied image). Identity at 96 keeps a standard
/// display byte-identical, and is also what the automation route always uses (deterministic,
/// no live cursor to sample).
pub(super) unsafe fn seed_dpi_for_capture(automation: bool) -> i32 {
    if automation {
        return 96;
    }
    let mut cursor = POINT::default();
    if GetCursorPos(&mut cursor).is_ok() {
        dpi_for_sel(RECT {
            left: cursor.x,
            top: cursor.y,
            right: cursor.x + 1,
            bottom: cursor.y + 1,
        })
    } else {
        96
    }
}

/// Assemble the overlay's initial mutable state from the frozen/dimmed DCs and the
/// capture-start options.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn build_shot_state(
    mem: HDC,
    bmp: HBITMAP,
    dim: HDC,
    dim_bmp: HBITMAP,
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    automation: bool,
    ocr_mode: bool,
    seed_dpi: i32,
) -> Box<Shot> {
    Box::new(Shot {
        shot: mem,
        shot_bmp: bmp,
        dimmed: dim,
        dimmed_bmp: dim_bmp,
        vx,
        vy,
        vw,
        vh,
        sel: None,
        sel_dragging: false,
        sel_anchor: POINT::default(),
        // The user's chosen starting tool (Settings > Screenshots). Arrow by default.
        tool: Tool::from_default_index(sagethumbs2k_core::settings::screenshot_default_tool()),
        cur_color: {
            let (r, g, b) = PALETTE[0];
            rgb(r, g, b)
        },
        thickness: 3,
        shapes: Vec::new(),
        redo: Vec::new(),
        draw_from: None,
        pen_pts: Vec::new(),
        cur: POINT::default(),
        typing: None,
        typing_drag: false,
        eye_copied: false,
        pending_hi: None,
        number_next: 1,
        selected: None,
        move_from: None,
        text_font: tools::default_text_font(crate::win::dpi_scale_dpi(18, seed_dpi)),
        color_flyout: false,
        customs: if automation {
            Vec::new()
        } else {
            super::super::prefs::load_custom_colors()
        },
        cust_colors: [COLORREF(0); 16],
        text_flyout: false,
        font_dropdown: false,
        hover_btn: None,
        tip_show: false,
        // No focus until the user asks for it with Tab. See `Shot::focus`.
        focus: None,
        born: if automation {
            GetTickCount64().saturating_sub(SETTLE_CLOSE_MS)
        } else {
            GetTickCount64()
        },
        automation: automation.then(|| AutomationState {
            forced_shift: false,
            commit_gen: 0,
            painted_gen: 0,
            last_drag: None,
            status: "ready",
            published_title: String::new(),
        }),
        // The automation route owns the editor pipeline it exercises, so it never runs in
        // OCR mode even if both flags were somehow passed.
        ocr_mode: ocr_mode && !automation,
        win_hint: None,
        win_hint_scan_ms: 0,
        tb_cache_key: None,
        tb_cache: Vec::new(),
    })
}

/// Register the overlay window class (if needed), create the window over the whole
/// virtual screen, attach `state`, and pump its message loop until it closes.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn run_overlay_message_loop(
    hinst: HINSTANCE,
    automation: bool,
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    state: Box<Shot>,
) {
    let class = if automation {
        w!("SageThumbs2KShotAutomation")
    } else {
        w!("SageThumbs2KShot")
    };
    let wc = WNDCLASSW {
        lpfnWndProc: Some(shot_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);

    // GDI+ powers the anti-aliased annotation drawing; init it for the lifetime of
    // the overlay (the message loop) and shut it down once the window closes.
    let gdip_token = gdip::startup();

    if let Ok(hwnd) = CreateWindowExW(
        overlay_ex_style(),
        class,
        if automation {
            w!("SageThumbs 2K Screenshot Automation")
        } else {
            w!("Screenshot")
        },
        WS_POPUP,
        vx,
        vy,
        vw,
        vh,
        None,
        None,
        Some(hinst),
        None,
    ) {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
        let _ = ShowWindow(hwnd, SW_SHOW);
        activate_overlay(hwnd);
        crate::win::pump_plain();
    } else {
        // CreateWindowExW failed (window-handle exhaustion is the realistic cause): `state`
        // drops right here as a plain Box, which does NOT release the four GDI objects it
        // holds (Shot has no Drop impl) — free them explicitly so a failed launch doesn't
        // leak a full-screen DC + bitmap pair (A144).
        let _ = DeleteDC(state.shot);
        let _ = DeleteObject(state.shot_bmp.into());
        let _ = DeleteDC(state.dimmed);
        let _ = DeleteObject(state.dimmed_bmp.into());
    }
    gdip::shutdown(gdip_token);
}

pub(super) unsafe fn run_capture_inner(hinst: HINSTANCE, automation: bool, ocr_mode: bool) {
    // Claim one shared mutex before any window lookup or screen allocation — see
    // `claim_single_overlay_slot` for why this closes the TOCTOU race.
    let Ok(_overlay_lock) = claim_single_overlay_slot(w!("SageThumbs2K.ShotOverlay.Single")) else {
        return;
    };
    // The configured pre-capture delay (0 = none). Never for the automation route, whose
    // whole contract is deterministic synthetic pixels with no live-desktop interaction.
    if !automation && !countdown_delay() {
        return; // Esc during the countdown = the user changed their mind
    }
    let Some((vx, vy, vw, vh)) = virtual_screen_metrics() else {
        return;
    };

    let screen = GetDC(None);
    let Some((mem, bmp)) = freeze_screen_to_dc(screen, vx, vy, vw, vh, automation) else {
        return;
    };
    let (dim, dim_bmp) = build_dimmed_copy(screen, mem, vw, vh);
    let seed_dpi = seed_dpi_for_capture(automation);
    let state = build_shot_state(
        mem, bmp, dim, dim_bmp, vx, vy, vw, vh, automation, ocr_mode, seed_dpi,
    );

    run_overlay_message_loop(hinst, automation, vx, vy, vw, vh, state);
}

/// Instant capture: grab the WHOLE virtual screen straight to the clipboard + a
/// timestamped PNG, with no overlay/editor — the "quick-save" hotkey's action.
/// Mirrors the screen-freeze in [`run_capture`] but skips every bit of UI, so it
/// returns the moment the file/clipboard are written.
pub(crate) unsafe fn capture_instant() {
    // Same single-overlay guard as `run_capture_inner` (A135): without it, pressing this
    // hotkey while the full editor overlay is already open freezes a picture OF the
    // dimmed overlay window instead of the real screen.
    let Ok(_instant_lock) = claim_single_overlay_slot(w!("SageThumbs2K.ShotOverlay.Single")) else {
        return;
    };
    // Same configured delay as the editor path — the quick-save hotkey is the one most
    // likely to be aimed at a transient (a tooltip, an open menu).
    if !countdown_delay() {
        return;
    }
    let Some((vx, vy, vw, vh)) = virtual_screen_metrics() else {
        return;
    };
    let screen = GetDC(None);
    // Same null-check as run_capture_inner's screen-freeze (A139): a GDI failure must not
    // fall through to SelectObject/BitBlt on a null handle.
    let Some((mem, bmp)) = create_fullscreen_dc_pair(
        screen,
        vw,
        vh,
        "instant capture: full-screen GDI setup failed",
    ) else {
        return;
    };
    let old = SelectObject(mem, HGDIOBJ(bmp.0));
    // HDR capture first, same as run_capture_inner's non-automation path (A017): the
    // quick-save hotkey used to always plain-BitBlt, shipping a washed-out capture on an
    // HDR display while the full editor's capture path already handled it correctly.
    if !hdr_capture(mem, vx, vy, vw, vh) {
        let _ = BitBlt(mem, 0, 0, vw, vh, Some(screen), vx, vy, SRCCOPY);
    }

    // 64-bit size math + sane bail: the i32 product `vw*vh*4` could (only on an
    // absurd >0.5-gigapixel virtual screen) overflow into an undersized buffer that
    // GetDIBits then overruns. Never reachable on real hardware, but cheap to close.
    let n = vw as i64 * vh as i64 * 4;
    if n <= 0 || n > i32::MAX as i64 {
        // Mirror the cleanup the success path does a few lines down.
        SelectObject(mem, old);
        let _ = DeleteObject(bmp.into());
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        return;
    }
    // Pull top-down BGRA (negative biHeight) — exactly what `output` expects.
    let buf = window_shot::pull_top_down_bgra(mem, bmp, vw, vh, n as usize);
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    let Some(buf) = buf else {
        return;
    };
    let copied = output::copy_dib_to_clipboard(&buf, vw, vh);
    // The editor-less instant capture can't prompt, so it always auto-saves to the
    // effective save folder (the configured one, or the Desktop by default).
    let dir = super::super::effective_save_dir();
    let saved = output::save_png_to_dir(std::path::Path::new(&dir), &buf, vw, vh);

    // Feedback — this hotkey used to be TOTALLY silent, so "worked" and "did nothing"
    // were indistinguishable. Success gets a Win+Shift+S-style split-second flash;
    // any failure gets a tray toast naming exactly what failed (plus the log line).
    match (copied, saved) {
        (true, true) => flash_screen(vx, vy, vw, vh),
        (true, false) => {
            sagethumbs2k_core::safety::log(&format!(
                "instant capture: PNG save to {dir} failed (it's still on the clipboard)"
            ));
            crate::win::notify_toast(
                "SageThumbs 2K",
                crate::win::t("toast_shot_fail_save")
                    .replace("{dir}", &dir)
                    .as_str(),
                std::time::Duration::from_secs(5),
            );
        }
        (false, true) => {
            sagethumbs2k_core::safety::log("instant capture: clipboard copy failed (PNG saved)");
            crate::win::notify_toast(
                "SageThumbs 2K",
                crate::win::t("toast_shot_fail_clip"),
                std::time::Duration::from_secs(5),
            );
        }
        (false, false) => {
            sagethumbs2k_core::safety::log(&format!(
                "instant capture: BOTH clipboard copy and PNG save to {dir} failed"
            ));
            crate::win::notify_toast(
                "SageThumbs 2K",
                crate::win::t("toast_shot_fail_all"),
                std::time::Duration::from_secs(6),
            );
        }
    }
}

/// A split-second white flash over the captured area — the only success cue the
/// editor-less instant capture gives (same visual language as Win+Shift+S). The window
/// is layered + click-through + non-activating, so it can't steal focus or eat a click;
/// three quick alpha steps read as a camera flash without being a strobe.
pub(super) unsafe fn flash_screen(vx: i32, vy: i32, vw: i32, vh: i32) {
    let class = w!("SageThumbs2KShotFlash");
    let hmod = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
    let wc = WNDCLASSW {
        lpfnWndProc: Some(flash_wndproc),
        hInstance: HINSTANCE(hmod.0),
        lpszClassName: class,
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
            windows::Win32::Graphics::Gdi::GetStockObject(
                windows::Win32::Graphics::Gdi::WHITE_BRUSH,
            )
            .0,
        ),
        ..Default::default()
    };
    RegisterClassW(&wc);
    let Ok(hwnd) = CreateWindowExW(
        WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
        class,
        PCWSTR::null(),
        WS_POPUP,
        vx,
        vy,
        vw,
        vh,
        None,
        None,
        None,
        None,
    ) else {
        return;
    };
    for alpha in [80u8, 45, 18] {
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        if alpha == 80 {
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            // This thread never pumps messages, so the queued WM_PAINT would never be
            // dispatched and the window would be destroyed before it ever painted —
            // i.e. no flash at all. UpdateWindow delivers WM_PAINT synchronously
            // (DefWindowProc + the class WHITE_BRUSH do the fill); the later alpha
            // steps only change DWM blending of the already-rendered surface, so one
            // forced paint is enough.
            let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
        }
        std::thread::sleep(std::time::Duration::from_millis(45));
    }
    let _ = DestroyWindow(hwnd);
}

pub(super) extern "system" fn flash_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}
