//! The capture overlay window: freeze the screen, drag a region, annotate it with
//! the [`tools`](super::tools), then accept (clipboard + PNG via
//! [`output`](super::output)) or cancel. Owns all mutable capture state in a `Shot`
//! attached to the window (`GWLP_USERDATA`).

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    CloseHandle, COLORREF, ERROR_ALREADY_EXISTS, E_FAIL, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT,
    POINT, RECT, WPARAM,
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
// Layer 2 of the accessibility work: the UI Automation provider. Deliberately NOT
// glob-imported into this hub like its siblings - it is a self-contained boundary with a
// handful of entry points, and spelling `uia::` at each call site says which side of that
// boundary you are on.
mod uia;

// Parent-hub imports: the children are glob-imported PRIVATELY so this file and every
// sibling still see one flat namespace, exactly as when all of this lived in one file.
use actions::*;
use automation::*;
use dialogs::*;
use input::*;
use loupe::*;
use paint::*;
mod countdown;
use countdown::*;
mod grab;
pub(crate) use grab::capture_instant;
use grab::*;

/// Where keyboard focus currently is inside the editor's chrome.
///
/// The editor is a single owner-drawn popup with coordinate hit-testing and no child
/// controls at all, so there is nothing for Windows to move focus between: a keyboard-only
/// user had no route whatsoever to the colour palette or the font dropdown, because both
/// only ever opened from a mouse click. This is that missing route.
///
/// Each variant is an INDEX into the group's own laid-out item list, i.e. exactly the list
/// the mouse hit-test already walks (`toolbar::layout`, `toolbar::color_flyout_layout`,
/// `toolbar::text_flyout_layout`). That is deliberate rather than a second numbering of our
/// own: a UI Automation provider (layer 2) has to hand a screen reader the same items in the
/// same order the mouse sees, and one shared index means the two can never disagree.
///
/// `Toolbar` indexes the raw layout vector, separators INCLUDED. Focus never comes to rest
/// on a `Button::Sep` (that is the whole job of `toolbar::step_focus`), but keeping the
/// index in the unfiltered list means paint can look the rect straight up instead of
/// maintaining a parallel focusable-only list that could drift out of step with it.
#[derive(Clone, Copy, PartialEq, Debug)]
enum FocusTarget {
    /// An index into `toolbar::layout`'s vector.
    Toolbar(usize),
    /// An index into the open colour flyout's swatch list.
    ColorFlyout(usize),
    /// An index into the open text flyout's item list. The list LENGTHENS when the font
    /// dropdown expands, which is why `input::repair_focus` re-checks it on every focus key
    /// rather than trusting an index across a state change.
    TextFlyout(usize),
}

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
    // Keyboard focus, `None` until the user presses Tab for the very first time. Every
    // keyboard behaviour built on it is gated on it already being `Some` (see
    // `input::on_key_focus`), so a capture in which Tab is never pressed behaves exactly as
    // it did before the focus model existed, down to which keys are consumed.
    focus: Option<FocusTarget>,
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

    /// The focused index inside the toolbar, or `None` when focus is unset or lives in a
    /// flyout. Paint asks each group separately because each group draws its own ring, and
    /// asking here (rather than matching on `focus` in three places in `paint.rs`) keeps the
    /// variant-to-group mapping in one file.
    fn focus_in_toolbar(&self) -> Option<usize> {
        match self.focus {
            Some(FocusTarget::Toolbar(i)) => Some(i),
            _ => None,
        }
    }

    /// The focused index inside the colour flyout, or `None`.
    fn focus_in_color_flyout(&self) -> Option<usize> {
        match self.focus {
            Some(FocusTarget::ColorFlyout(i)) => Some(i),
            _ => None,
        }
    }

    /// The focused index inside the text flyout, or `None`.
    fn focus_in_text_flyout(&self) -> Option<usize> {
        match self.focus {
            Some(FocusTarget::TextFlyout(i)) => Some(i),
            _ => None,
        }
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
    // One overlay at a time: each hotkey press spawns a fresh `--screenshot` process, and
    // MOD_NOREPEAT only suppresses key auto-repeat — a second REAL press would stack another
    // fullscreen overlay whose frozen snapshot is a picture OF the first (dimmed) overlay.
    if last_err == ERROR_ALREADY_EXISTS
        || FindWindowW(w!("SageThumbs2KShot"), PCWSTR::null()).is_ok()
        || FindWindowW(w!("SageThumbs2KShotAutomation"), PCWSTR::null()).is_ok()
    {
        // A refused claim holds nothing worth keeping; `HANDLE` has no Drop.
        let _ = CloseHandle(lock);
        return Err(windows::core::Error::from(E_FAIL));
    }
    Ok(lock)
}

/// Is point `p` inside rect `r`?
fn pt_in(r: RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

#[cfg(test)]
mod tests;
