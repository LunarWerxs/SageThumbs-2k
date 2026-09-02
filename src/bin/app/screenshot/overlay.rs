//! The capture overlay window: freeze the screen, drag a region, annotate it with
//! the [`tools`](super::tools), then accept (clipboard + PNG via
//! [`output`](super::output)) or cancel. Owns all mutable capture state in a `Shot`
//! attached to the window (`GWLP_USERDATA`).

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    COLORREF, ERROR_ALREADY_EXISTS, E_FAIL, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT,
    WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GdiFlush, GetDC, GetPixel,
    IntersectClipRect, InvalidateRect, MonitorFromRect, ReleaseDC, RestoreDC, SaveDC, SelectObject,
    SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, TextOutW, AC_SRC_OVER, BLENDFUNCTION,
    COLORONCOLOR, DT_CALCRECT, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HGDIOBJ, LOGFONTW,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Controls::Dialogs::{
    ChooseColorW, ChooseFontW, CC_ANYCOLOR, CC_ENABLEHOOK, CC_FULLOPEN, CC_RGBINIT, CF_EFFECTS,
    CF_ENABLEHOOK, CF_INITTOLOGFONTSTRUCT, CF_SCREENFONTS, CHOOSECOLORW, CHOOSEFONTW,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_F8, VK_RETURN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::dark::rgb;
use crate::win::{app_icon, gui_font, wide};

use super::output;
use super::toolbar::{self, Button, Swatch, TextItem};
use super::tools::{self, Shape, Tool, PALETTE};
use super::window_shot;
use crate::gdip;

mod actions;
mod automation;
mod dialogs;
mod input;
mod loupe;
mod paint;

// Parent-hub imports: the children are glob-imported PRIVATELY so this file and every
// sibling still see one flat namespace, exactly as when all of this lived in one file.
use actions::*;
use automation::*;
use dialogs::*;
use input::*;
use loupe::*;
use paint::*;

/// All mutable capture state, owned by the window (`GWLP_USERDATA`).
struct Shot {
    shot: HDC, // frozen virtual-screen snapshot (memory DC)
    shot_bmp: HBITMAP,
    dimmed: HDC, // a pre-dimmed copy of the snapshot (so paint blits it, no per-frame alpha)
    dimmed_bmp: HBITMAP,
    // The overlay's client origin in physical virtual-screen coordinates. All editor
    // geometry is client-relative, but monitor/DPI APIs require screen coordinates.
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    sel: Option<RECT>, // committed region; None until the first drag completes
    sel_dragging: bool,
    sel_anchor: POINT,
    tool: Tool,
    cur_color: COLORREF,
    thickness: i32,
    shapes: Vec<Shape>,
    redo: Vec<Shape>,
    draw_from: Option<POINT>,
    pen_pts: Vec<POINT>,
    cur: POINT,
    typing: Option<(POINT, String)>,
    // True while Ctrl-dragging the *active* (not-yet-committed) text box to reposition
    // it without ending the edit. Paired with `move_from` for the drag delta.
    typing_drag: bool,
    // True for one paint after the Eyedropper copies a colour — flips the loupe label
    // to a "Copied" confirmation. Cleared on the next cursor move.
    eye_copied: bool,
    // A pending UTF-16 high surrogate from a WM_CHAR, awaiting its low surrogate (a
    // non-BMP character arrives as two WM_CHAR messages). None most of the time.
    pending_hi: Option<u16>,
    number_next: u32,
    // Move tool: which shape is grabbed + the last drag point.
    selected: Option<usize>,
    move_from: Option<POINT>,
    // Text tool font (family/size/style); size via `[` / `]`, full set via the Font
    // dialog (click the active Text button).
    text_font: LOGFONTW,
    // Colour palette flyout open? + remembered custom colours + the dialog's 16-slot
    // custom array (this session).
    color_flyout: bool,
    customs: Vec<COLORREF>,
    cust_colors: [COLORREF; 16],
    // Text settings flyout open? + is its font dropdown expanded?
    text_flyout: bool,
    font_dropdown: bool,
    // Toolbar hover → delayed tooltip: the hovered button + whether to show its tip.
    hover_btn: Option<Button>,
    tip_show: bool,
    // Tick (GetTickCount64) the overlay was created — used to swallow the in-flight
    // hotkey keystroke that would otherwise instantly close it (see SETTLE_CLOSE_MS).
    born: u64,
    // Present only for the hidden, synthetic full-screen automation route. It uses
    // the real editor/input/paint pipeline while fencing off clipboard, disk,
    // dialogs, and upload/network side effects.
    automation: Option<AutomationState>,
    // "Copy text on screen (OCR)" launch mode (the custom hotkey action): the FIRST
    // completed region drag runs OCR and closes, skipping the editor entirely. The
    // annotation toolbar never appears, because there is nothing to annotate.
    ocr_mode: bool,
    // The top-level window under the cursor while nothing is selected and no drag is in
    // progress (client coords, clamped to the overlay). Painted as a live preview — the
    // window shows bright inside the dim, framed like a drag — and a CLICK (a "drag" under
    // the 4 px threshold) captures exactly that rect. `None` over the bare desktop, over
    // our own overlay, and the moment a real drag starts.
    win_hint: Option<RECT>,
    // `GetTickCount64` at the last full z-order walk `update_window_hint` did: that
    // walk does up to two DWM calls per top-level window, so it's throttled to run at most
    // every [`input::WINDOW_HINT_THROTTLE_MS`] rather than on every single WM_MOUSEMOVE.
    win_hint_scan_ms: u64,
    // Memoized `toolbar::layout`: WM_MOUSEMOVE (`update_hover_button`) and
    // WM_SETCURSOR (`is_over_toolbar_ui`) both ask for the current toolbar layout on
    // every tick: keyed on the selection rect + DPI so a WM_SETCURSOR right after a
    // WM_MOUSEMOVE (the common case — Windows sends both per tick) reuses the same
    // layout instead of rebuilding it from scratch a second time.
    tb_cache_key: Option<(i32, i32, i32, i32, i32)>,
    tb_cache: Vec<(Button, RECT)>,
}

/// Hover-delay timer id (one-shot, re-armed on each new hovered button).
const HOVER_TIMER: usize = 1;

/// Grace window (ms) after the overlay opens during which the close keys (Esc/Enter)
/// are ignored. When a *global hotkey* launches the overlay, the keystroke that
/// triggered it (and its key-up) are still in flight; the moment the overlay grabs
/// focus they arrive here and would cancel/accept-and-close the capture in a split
/// second. Swallowing the close keys this briefly lets the triggering press settle.
const SETTLE_CLOSE_MS: u64 = 400;

impl Shot {
    fn color(&self) -> COLORREF {
        self.cur_color
    }
    /// Advance to the next palette colour (the `K` key) — wraps; jumps to the first
    /// entry if the current colour isn't a palette one (e.g. a custom pick).
    fn cycle_color(&mut self) {
        let pos = PALETTE
            .iter()
            .position(|&(r, g, b)| rgb(r, g, b) == self.cur_color);
        let next = pos.map(|i| (i + 1) % PALETTE.len()).unwrap_or(0);
        let (r, g, b) = PALETTE[next];
        self.cur_color = rgb(r, g, b);
    }
}

/// The effective DPI of the monitor the selection sits on. The overlay window itself
/// spans the whole virtual screen (so `GetDpiForWindow` on it is meaningless across a
/// mixed-DPI setup); we ask the monitor *under the region* instead so the chrome is
/// sized for the display the user is actually working on. Falls back to 96 (the
/// identity for `dpi_scale_dpi`, keeping a standard display byte-identical).
unsafe fn dpi_for_sel(sel: RECT) -> i32 {
    let hmon = MonitorFromRect(&sel, MONITOR_DEFAULTTONEAREST);
    if hmon.is_invalid() {
        return 96;
    }
    let mut dpix = 0u32;
    let mut dpiy = 0u32;
    if GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpix, &mut dpiy).is_ok() && dpix != 0 {
        dpix as i32
    } else {
        96
    }
}

/// Convert overlay-client geometry to the physical virtual-screen coordinates used by
/// monitor APIs. The overlay intentionally paints its backing bitmap at `(0, 0)`, even
/// when a monitor sits left of or above the primary display, so this translation must
/// happen only at the OS boundary — never in drawing or hit-testing code.
fn client_rect_to_screen(rect: RECT, vx: i32, vy: i32) -> RECT {
    RECT {
        left: rect.left.saturating_add(vx),
        top: rect.top.saturating_add(vy),
        right: rect.right.saturating_add(vx),
        bottom: rect.bottom.saturating_add(vy),
    }
}

unsafe fn shot_ptr(hwnd: HWND) -> *mut Shot {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Shot
}

fn effective_shift(physical_shift: bool, automation: Option<&AutomationState>) -> bool {
    physical_shift || automation.is_some_and(|state| state.forced_shift)
}

unsafe fn shift_active(s: &Shot) -> bool {
    let physical = (GetKeyState(VK_SHIFT.0 as i32) as u16 & 0x8000) != 0;
    effective_shift(physical, s.automation.as_ref())
}

unsafe fn shot_dpi_for_sel(s: &Shot, sel: RECT) -> i32 {
    if s.automation.is_some() {
        96
    } else {
        dpi_for_sel(client_rect_to_screen(sel, s.vx, s.vy))
    }
}

/// HDR capture when the feature is compiled in, otherwise "nothing to do".
///
/// The `hdr-capture` feature is app-only (see Cargo.toml): it drags in D3D11 and
/// DXGI, which the shell DLL must never link. This shim keeps the one call site
/// free of `cfg` noise.
#[cfg(feature = "hdr-capture")]
unsafe fn hdr_capture(dst: HDC, vx: i32, vy: i32, vw: i32, vh: i32) -> bool {
    super::hdr::capture_into(dst, vx, vy, vw, vh)
}

#[cfg(not(feature = "hdr-capture"))]
unsafe fn hdr_capture(_dst: HDC, _vx: i32, _vy: i32, _vw: i32, _vh: i32) -> bool {
    false
}

unsafe fn activate_overlay(hwnd: HWND) {
    // Repeated on click too: if Windows denied the initial grab AND the fallback
    // could not run (no foreground window at spawn time), the first click is the
    // next chance to become focusable.
    crate::win::force_foreground(hwnd);
}

pub(crate) unsafe fn run_capture(hinst: HINSTANCE) {
    run_capture_inner(hinst, false, false);
}

/// `--screenshot-ocr`: the same capture overlay, but the first finished region drag goes
/// straight to OCR and closes. One keystroke, one drag, text on the clipboard — no editor,
/// no toolbar click. Bound to the custom hotkey action "Copy text on screen (OCR)".
pub(crate) unsafe fn run_capture_ocr(hinst: HINSTANCE) {
    run_capture_inner(hinst, false, true);
}

/// Hidden integration-test route: the real full-screen editor over a deterministic,
/// opaque synthetic canvas. This intentionally ships without a UI entry point so an
/// installed build can be exercised through Windows automation without exposing the
/// user's desktop or producing clipboard/file/network side effects.
pub(crate) unsafe fn run_capture_automation(hinst: HINSTANCE) {
    run_capture_inner(hinst, true, false);
}

fn overlay_ex_style() -> WINDOW_EX_STYLE {
    // WS_EX_TOOLWINDOW is deliberately absent. Windows automation enumerators
    // commonly reject tool windows outright. WS_EX_NOACTIVATE still keeps this
    // ownerless popup out of the taskbar while SetForegroundWindow below explicitly
    // activates it for keyboard shortcuts.
    WS_EX_TOPMOST | WS_EX_NOACTIVATE
}

/// Claim the single-overlay mutex `name` and confirm neither overlay window class is
/// already up. Shared by [`run_capture_inner`] (the full editor) and [`capture_instant`]
/// (the quick-save hotkey) so a press of either while the other is already running cannot
/// stack a second freeze on top of it: a named kernel mutex closes the TOCTOU race between
/// two near-simultaneous launches (`CreateMutexW` requesting initial ownership of an
/// ALREADY-existing mutex reports `ERROR_ALREADY_EXISTS` without granting it, which is
/// exactly the "someone else got there first" signal this checks for), and the FindWindow
/// pair catches the case where an editor overlay is already up and holding the SAME mutex.
/// `Err` means the caller must return immediately without allocating any screen resources.
/// Returns the held mutex `HANDLE` on success — keep it alive for as long as the capture
/// runs, the same way the original single-function version did.
unsafe fn claim_single_overlay_slot(name: PCWSTR) -> windows::core::Result<HANDLE> {
    let (lock, last_err) = crate::win::create_mutex_user_only(true, name);
    let lock = lock?;
    if last_err == ERROR_ALREADY_EXISTS {
        return Err(windows::core::Error::from(E_FAIL));
    }
    // One overlay at a time: each hotkey press spawns a fresh `--screenshot` process, and
    // MOD_NOREPEAT only suppresses key auto-repeat — a second REAL press would stack another
    // fullscreen overlay whose frozen snapshot is a picture OF the first (dimmed) overlay.
    if FindWindowW(w!("SageThumbs2KShot"), PCWSTR::null()).is_ok()
        || FindWindowW(w!("SageThumbs2KShotAutomation"), PCWSTR::null()).is_ok()
    {
        return Err(windows::core::Error::from(E_FAIL));
    }
    Ok(lock)
}

/// Honour the configured capture delay: a small topmost countdown chip near the cursor
/// ticks the remaining seconds, then returns `true` to proceed. Esc aborts (`false`).
///
/// Runs BEFORE the screen freeze — that is the entire point: the delay exists so a
/// hover-only menu or tooltip can be summoned and still be on screen when the snapshot
/// happens. The chip deliberately sits near the cursor (where the user's attention already
/// is) and repaints only on whole-second boundaries.
unsafe fn countdown_delay() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
    let secs = sagethumbs2k_core::settings::screenshot_delay_sec();
    if secs == 0 {
        return true;
    }
    // Swallow a stale Esc state from before the countdown started.
    let _ = GetAsyncKeyState(VK_ESCAPE.0 as i32);
    let deadline = GetTickCount64() + u64::from(secs) * 1000;
    let mut shown: u64 = 0;
    let mut chip: Option<HWND> = None;
    loop {
        let now = GetTickCount64();
        if now >= deadline {
            break;
        }
        // Esc pressed since the last poll → abort the capture entirely.
        if GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8001 != 0 {
            if let Some(h) = chip {
                let _ = DestroyWindow(h);
            }
            return false;
        }
        let remaining = (deadline - now).div_ceil(1000);
        if remaining != shown {
            shown = remaining;
            let mut cur = POINT::default();
            let _ = GetCursorPos(&mut cur);
            chip = Some(countdown_chip(chip, cur, remaining));
        }
        crate::win::pump_msgs(4);
        std::thread::sleep(std::time::Duration::from_millis(40));
    }
    if let Some(h) = chip {
        let _ = DestroyWindow(h);
    }
    // One last pump so the chip's pixels are OFF screen before the freeze — otherwise the
    // countdown photographs itself into the capture.
    crate::win::pump_msgs(4);
    std::thread::sleep(std::time::Duration::from_millis(60));
    true
}

/// Create (or move) the countdown chip showing `n`, offset from the cursor. A plain
/// topmost popup painted in WM_PAINT — no layering, no timer of its own; the countdown
/// loop drives it.
unsafe fn countdown_chip(existing: Option<HWND>, cur: POINT, n: u64) -> HWND {
    const W: i32 = 46;
    const H: i32 = 40;
    let class = w!("SageThumbs2KShotCountdown");
    if existing.is_none() {
        let wc = WNDCLASSW {
            lpfnWndProc: Some(countdown_wndproc),
            hInstance: HINSTANCE(unsafe {
                windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                    .map(|h| h.0)
                    .unwrap_or(core::ptr::null_mut())
            }),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc); // idempotent; a re-register just fails quietly
    }
    let hwnd = match existing {
        Some(h) => h,
        None => CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            class,
            w!(""),
            WS_POPUP,
            0,
            0,
            W,
            H,
            None,
            None,
            None,
            None,
        )
        .unwrap_or_default(),
    };
    if hwnd.is_invalid() {
        return hwnd;
    }
    // Stash the digit for WM_PAINT, then place beside the cursor (offset so the chip is
    // visible and never UNDER the point being aimed at).
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, n as isize);
    let _ = SetWindowPos(
        hwnd,
        Some(HWND_TOPMOST),
        cur.x + 24,
        cur.y + 24,
        W,
        H,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    let _ = InvalidateRect(Some(hwnd), None, true);
    hwnd
}

unsafe extern "system" fn countdown_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);
            let bg = CreateSolidBrush(rgb(24, 24, 24));
            FillRect(hdc, &rc, bg);
            let _ = DeleteObject(bg.into());
            let n = GetWindowLongPtrW(hwnd, GWLP_USERDATA).max(0);
            SelectObject(hdc, HGDIOBJ(crate::win::gui_font_title(hwnd).0));
            SetBkMode(hdc, windows::Win32::Graphics::Gdi::TRANSPARENT);
            SetTextColor(hdc, rgb(240, 240, 240));
            let mut txt = crate::win::wide(&format!("{n}"));
            let tn = txt.len().saturating_sub(1);
            DrawTextW(
                hdc,
                &mut txt[..tn],
                &mut rc,
                windows::Win32::Graphics::Gdi::DT_CENTER
                    | windows::Win32::Graphics::Gdi::DT_VCENTER
                    | windows::Win32::Graphics::Gdi::DT_SINGLELINE,
            );
            let _ = EndPaint(hwnd, &ps);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// The virtual desktop's origin and size, or `None` if either dimension is non-positive
/// (no monitors attached is the realistic cause).
unsafe fn virtual_screen_metrics() -> Option<(i32, i32, i32, i32)> {
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    (vw > 0 && vh > 0).then_some((vx, vy, vw, vh))
}

/// Freeze the screen into a memory DC (the normal overlay paints from this, never the
/// live desktop, so annotations don't fight with what's underneath), or fill it with the
/// deterministic synthetic automation canvas, since the automation route MUST NOT copy or
/// sample the desktop. A GDI failure here (object-quota exhaustion is the realistic
/// cause) must not fall through to SelectObject/BitBlt on a null handle, which paints
/// "you captured a black screen" instead of a diagnosable failure (A139), so this logs,
/// releases everything already allocated (`screen` included), and returns `None` instead.
unsafe fn freeze_screen_to_dc(
    screen: HDC,
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
    automation: bool,
) -> Option<(HDC, HBITMAP)> {
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, vw, vh);
    if screen.is_invalid() || mem.is_invalid() || bmp.is_invalid() {
        sagethumbs2k_core::safety::log(
            "screenshot: full-screen GDI setup failed, aborting capture",
        );
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
        if !bmp.is_invalid() {
            let _ = DeleteObject(bmp.into());
        }
        if !screen.is_invalid() {
            ReleaseDC(None, screen);
        }
        return None;
    }
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
unsafe fn build_dimmed_copy(screen: HDC, mem: HDC, vw: i32, vh: i32) -> (HDC, HBITMAP) {
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
unsafe fn seed_dpi_for_capture(automation: bool) -> i32 {
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
unsafe fn build_shot_state(
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
            super::prefs::load_custom_colors()
        },
        cust_colors: [COLORREF(0); 16],
        text_flyout: false,
        font_dropdown: false,
        hover_btn: None,
        tip_show: false,
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
unsafe fn run_overlay_message_loop(
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
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
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

unsafe fn run_capture_inner(hinst: HINSTANCE, automation: bool, ocr_mode: bool) {
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
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    if vw <= 0 || vh <= 0 {
        return;
    }
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, vw, vh);
    // Same null-check as run_capture_inner's screen-freeze (A139): a GDI failure must not
    // fall through to SelectObject/BitBlt on a null handle.
    if screen.is_invalid() || mem.is_invalid() || bmp.is_invalid() {
        sagethumbs2k_core::safety::log("instant capture: full-screen GDI setup failed");
        if !mem.is_invalid() {
            let _ = DeleteDC(mem);
        }
        if !bmp.is_invalid() {
            let _ = DeleteObject(bmp.into());
        }
        if !screen.is_invalid() {
            ReleaseDC(None, screen);
        }
        return;
    }
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
    let dir = super::effective_save_dir();
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
unsafe fn flash_screen(vx: i32, vy: i32, vw: i32, vh: i32) {
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

extern "system" fn flash_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// Is point `p` inside rect `r`?
fn pt_in(r: RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

#[cfg(test)]
mod tests {
    use super::*;

    fn automation_state() -> AutomationState {
        AutomationState {
            forced_shift: false,
            commit_gen: 0,
            painted_gen: 0,
            last_drag: None,
            status: "ready",
            published_title: String::new(),
        }
    }

    /// A135: `capture_instant` used to have no guard at all against a second capture
    /// starting while one is already in flight. This exercises the shared mechanism
    /// directly (with a private test-only mutex name, so it can't collide with a real
    /// overlay or with another test in this same process) rather than driving the full
    /// GDI/window-message `run_capture_inner`/`capture_instant` paths, which need a live
    /// desktop session this unit test does not assume.
    #[test]
    fn single_overlay_guard_blocks_a_second_concurrent_claim() {
        unsafe {
            let name = w!("SageThumbs2K.ShotOverlay.Single.UnitTest");
            let first = claim_single_overlay_slot(name);
            assert!(first.is_ok(), "first claim must succeed");
            let second = claim_single_overlay_slot(name);
            assert!(
                second.is_err(),
                "a second concurrent claim while the first is still held must be refused"
            );
            drop(first);
        }
    }

    #[test]
    fn overlay_style_is_visible_to_windows_automation_without_taskbar_chrome() {
        let style = overlay_ex_style().0;
        assert_ne!(style & WS_EX_TOPMOST.0, 0);
        assert_ne!(style & WS_EX_NOACTIVATE.0, 0);
        assert_eq!(style & WS_EX_TOOLWINDOW.0, 0);
    }

    #[test]
    fn automation_shift_latch_ors_with_physical_shift() {
        let mut state = automation_state();
        assert!(!effective_shift(false, None));
        assert!(effective_shift(true, None));
        assert!(!effective_shift(false, Some(&state)));
        assert!(effective_shift(true, Some(&state)));

        state.forced_shift = true;
        assert!(effective_shift(false, Some(&state)));
        assert!(effective_shift(true, Some(&state)));
    }

    /// Regression for a 150%-scaled display to the left of a 100% primary. Mouse
    /// input and the backing bitmap are overlay-client coordinates, while
    /// `MonitorFromRect` expects physical desktop coordinates. Feeding it the client
    /// rect used to make a selection on the left display look as though it belonged to
    /// the primary, so all selection chrome used the wrong DPI.
    #[test]
    fn mixed_dpi_selection_maps_client_geometry_to_the_virtual_desktop() {
        // Virtual desktop: 2560x1440 @ 150% on the left and 120 px above a
        // 1920x1080 @ 100% primary at (0, 0). The overlay starts at that origin.
        let selection = RECT {
            left: 320,
            top: 240,
            right: 1120,
            bottom: 840,
        };
        let desktop = client_rect_to_screen(selection, -2560, -120);

        assert_eq!(
            desktop,
            RECT {
                left: -2240,
                top: 120,
                right: -1440,
                bottom: 720,
            }
        );
        // Translation is only for the monitor query: the saved crop/layout stays
        // pixel-for-pixel in the overlay's client bitmap.
        assert_eq!(
            selection.right - selection.left,
            desktop.right - desktop.left
        );
        assert_eq!(
            selection.bottom - selection.top,
            desktop.bottom - desktop.top
        );
    }

    #[test]
    fn automation_title_reports_post_paint_committed_geometry() {
        let mut state = automation_state();
        state.forced_shift = true;
        state.commit_gen = 2;
        state.painted_gen = 2;
        state.last_drag = Some(AutomationDrag {
            tool: Tool::Line,
            anchor: POINT { x: 150, y: 290 },
            raw: POINT { x: 350, y: 370 },
            final_point: POINT { x: 302, y: 442 },
            snapped: true,
        });

        assert_eq!(
            automation_title(&state),
            "SageThumbs 2K Screenshot Automation | snap=1 | commit=2 | painted=2 | status=ready | tool=Line | anchor=150,290 | raw=350,370 | final=302,442 | shifted=1"
        );
    }
}
