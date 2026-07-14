//! The capture overlay window: freeze the screen, drag a region, annotate it with
//! the [`tools`](super::tools), then accept (clipboard + PNG via
//! [`output`](super::output)) or cancel. Owns all mutable capture state in a `Shot`
//! attached to the window (`GWLP_USERDATA`).

use core::ffi::c_void;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetDC, GetDIBits, GetPixel,
    IntersectClipRect, InvalidateRect, MonitorFromRect, ReleaseDC, RestoreDC, SaveDC, SelectObject,
    SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, TextOutW,
    AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, BLENDFUNCTION, COLORONCOLOR, DIB_RGB_COLORS,
    DT_CALCRECT, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HBITMAP, HDC, HGDIOBJ, LOGFONTW,
    MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Controls::Dialogs::{
    ChooseColorW, ChooseFontW, CC_ANYCOLOR, CC_ENABLEHOOK, CC_FULLOPEN, CC_RGBINIT, CF_EFFECTS,
    CF_ENABLEHOOK, CF_INITTOLOGFONTSTRUCT, CF_SCREENFONTS, CHOOSECOLORW, CHOOSEFONTW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, VK_CONTROL, VK_DELETE, VK_ESCAPE, VK_RETURN,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::Win32::System::SystemInformation::GetTickCount64;

use crate::dark::rgb;
use crate::win::{app_icon, gui_font, wide};

use crate::gdip;
use super::output;
use super::toolbar::{self, Button, Swatch, TextItem};
use super::tools::{self, Shape, Tool, PALETTE};

/// All mutable capture state, owned by the window (`GWLP_USERDATA`).
struct Shot {
    shot: HDC, // frozen virtual-screen snapshot (memory DC)
    shot_bmp: HBITMAP,
    dimmed: HDC, // a pre-dimmed copy of the snapshot (so paint blits it, no per-frame alpha)
    dimmed_bmp: HBITMAP,
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
        let pos = PALETTE.iter().position(|&(r, g, b)| rgb(r, g, b) == self.cur_color);
        let next = pos.map(|i| (i + 1) % PALETTE.len()).unwrap_or(0);
        let (r, g, b) = PALETTE[next];
        self.cur_color = rgb(r, g, b);
    }
}

fn pt(lparam: LPARAM) -> POINT {
    POINT { x: (lparam.0 & 0xffff) as u16 as i16 as i32, y: ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32 }
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

unsafe fn shot_ptr(hwnd: HWND) -> *mut Shot {
    GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut Shot
}

pub(crate) unsafe fn run_capture(hinst: HINSTANCE) {
    // One overlay at a time: each hotkey press spawns a fresh `--screenshot` process, and
    // MOD_NOREPEAT only suppresses key auto-repeat — a second REAL press would stack another
    // fullscreen overlay whose frozen snapshot is a picture OF the first (dimmed) overlay.
    if FindWindowW(w!("SageThumbs2KShot"), PCWSTR::null()).is_ok() {
        return;
    }
    let vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let vw = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let vh = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    if vw <= 0 || vh <= 0 {
        return;
    }
    // Freeze the screen into a memory DC (the overlay paints from this, never the
    // live desktop, so annotations don't fight with what's underneath).
    let screen = GetDC(None);
    let mem = CreateCompatibleDC(Some(screen));
    let bmp = CreateCompatibleBitmap(screen, vw, vh);
    SelectObject(mem, HGDIOBJ(bmp.0));
    let _ = BitBlt(mem, 0, 0, vw, vh, Some(screen), vx, vy, SRCCOPY);
    // A pre-dimmed copy of the snapshot — paint blits this for the surround (no
    // per-frame alpha) and blits the bright `mem` through for the selection.
    let dim = CreateCompatibleDC(Some(screen));
    let dim_bmp = CreateCompatibleBitmap(screen, vw, vh);
    SelectObject(dim, HGDIOBJ(dim_bmp.0));
    let _ = BitBlt(dim, 0, 0, vw, vh, Some(mem), 0, 0, SRCCOPY);
    apply_dim(dim, vw, vh);
    ReleaseDC(None, screen);

    // Seed the default annotation text size for the DPI of the monitor under the
    // cursor at capture start (no selection exists yet to source one from). The
    // user-chosen size from here on stays physical — it's baked into the saved/copied
    // image — but the starting default should feel the same physical size on a HiDPI
    // display. Identity at 96 keeps a standard display byte-identical.
    let mut cur = POINT::default();
    let seed_dpi = if GetCursorPos(&mut cur).is_ok() {
        dpi_for_sel(RECT { left: cur.x, top: cur.y, right: cur.x + 1, bottom: cur.y + 1 })
    } else {
        96
    };

    let state = Box::new(Shot {
        shot: mem,
        shot_bmp: bmp,
        dimmed: dim,
        dimmed_bmp: dim_bmp,
        vw,
        vh,
        sel: None,
        sel_dragging: false,
        sel_anchor: POINT::default(),
        tool: Tool::Rect,
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
        customs: super::prefs::load_custom_colors(),
        cust_colors: [COLORREF(0); 16],
        text_flyout: false,
        font_dropdown: false,
        hover_btn: None,
        tip_show: false,
        born: GetTickCount64(),
    });

    let class = w!("SageThumbs2KShot");
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
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        class,
        w!("Screenshot"),
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
        let _ = SetForegroundWindow(hwnd);
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    gdip::shutdown(gdip_token);
}

/// Instant capture: grab the WHOLE virtual screen straight to the clipboard + a
/// timestamped PNG, with no overlay/editor — the "quick-save" hotkey's action.
/// Mirrors the screen-freeze in [`run_capture`] but skips every bit of UI, so it
/// returns the moment the file/clipboard are written.
pub(crate) unsafe fn capture_instant() {
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
    let old = SelectObject(mem, HGDIOBJ(bmp.0));
    let _ = BitBlt(mem, 0, 0, vw, vh, Some(screen), vx, vy, SRCCOPY);

    // Pull top-down BGRA (negative biHeight) — exactly what `output` expects.
    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: vw,
            biHeight: -vh,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // 64-bit size math + sane bail: the i32 product `vw*vh*4` could (only on an
    // absurd >0.5-gigapixel virtual screen) overflow into an undersized buffer that
    // GetDIBits then overruns. Never reachable on real hardware, but cheap to close.
    let n = vw as i64 * vh as i64 * 4;
    if n <= 0 || n > i32::MAX as i64 {
        return;
    }
    let mut buf = vec![0u8; n as usize];
    let got = GetDIBits(mem, bmp, 0, vh as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(mem);
    ReleaseDC(None, screen);
    if got == 0 {
        return;
    }
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
                crate::win::t("toast_shot_fail_save").replace("{dir}", &dir).as_str(),
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
    let hmod =
        windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
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

extern "system" fn shot_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        // The Shot state is attached only after CreateWindowExW returns; any message
        // during creation has no state yet — pass it through so the deref'ing arms
        // always see a valid pointer. (WM_DESTROY guards its own null.)
        if shot_ptr(hwnd).is_null() && msg != WM_DESTROY {
            return DefWindowProcW(hwnd, msg, wparam, lparam);
        }
        match msg {
            WM_ERASEBKGND => LRESULT(1), // the snapshot covers every pixel
            WM_LBUTTONDOWN => {
                let s = &mut *shot_ptr(hwnd);
                let p = pt(lparam);
                match s.sel {
                    None => {
                        if s.tool == Tool::Eyedropper {
                            // Pick a colour without dragging a region first (E + click).
                            sample_pixel(s, p);
                        } else {
                            s.sel_dragging = true;
                            s.sel_anchor = p;
                            s.cur = p;
                        }
                    }
                    Some(sel) => {
                        let dpi = dpi_for_sel(sel);
                        let buttons = toolbar::layout(sel, s.vw, s.vh, dpi);
                        // The open colour palette intercepts clicks first.
                        if s.color_flyout {
                            if let Some((_, cbr)) = buttons.iter().find(|(b, _)| *b == Button::Color) {
                                let (_, sw) = toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
                                if let Some((swatch, _)) = sw.iter().find(|(_, r)| pt_in(*r, p)) {
                                    match *swatch {
                                        Swatch::Color(c) | Swatch::Custom(Some(c)) => s.cur_color = c,
                                        Swatch::Custom(None) | Swatch::Picker => pick_custom_color(hwnd, s),
                                    }
                                    s.color_flyout = false;
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                    return LRESULT(0);
                                }
                            }
                            // Clicked off the palette → close it; consume if that click
                            // was the Colour button itself (else fall through).
                            s.color_flyout = false;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            if toolbar::hit(&buttons, p.x, p.y) == Some(Button::Color) {
                                return LRESULT(0);
                            }
                        }
                        // The open text settings flyout intercepts clicks too.
                        if s.text_flyout {
                            if let Some((_, tbr)) = buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text)) {
                                let (_, its) = toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi);
                                if let Some((item, _)) = its.iter().find(|(_, r)| pt_in(*r, p)) {
                                    match *item {
                                        TextItem::FontField => s.font_dropdown = !s.font_dropdown,
                                        TextItem::FontOption(i) => {
                                            tools::set_face(&mut s.text_font, toolbar::PRESET_FONTS[i]);
                                            s.font_dropdown = false;
                                        }
                                        TextItem::SizeDown => {
                                            let sz = (-s.text_font.lfHeight - 2).max(8);
                                            s.text_font.lfHeight = -sz;
                                        }
                                        TextItem::SizeUp => {
                                            let sz = (-s.text_font.lfHeight + 2).min(120);
                                            s.text_font.lfHeight = -sz;
                                        }
                                        TextItem::Bold => {
                                            s.text_font.lfWeight = if s.text_font.lfWeight >= 700 { 400 } else { 700 };
                                        }
                                        TextItem::Underline => {
                                            s.text_font.lfUnderline = u8::from(s.text_font.lfUnderline == 0);
                                        }
                                        TextItem::More => {
                                            pick_text_font(hwnd, s);
                                            s.text_flyout = false;
                                            s.font_dropdown = false;
                                        }
                                    }
                                    let _ = InvalidateRect(Some(hwnd), None, false);
                                    return LRESULT(0);
                                }
                            }
                            // Clicked off the flyout → close it. Consume if it was the
                            // Text button itself; else fall through (a canvas click
                            // then drops the text caret and starts typing).
                            s.text_flyout = false;
                            s.font_dropdown = false;
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            if toolbar::hit(&buttons, p.x, p.y) == Some(Button::Tool(Tool::Text)) {
                                return LRESULT(0);
                            }
                        }
                        // A click on a toolbar button takes priority over drawing.
                        if let Some(btn) = toolbar::hit(&buttons, p.x, p.y) {
                            if handle_button(hwnd, s, btn) {
                                return LRESULT(0); // window destroyed — stop touching it
                            }
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                        if ctrl && s.typing.is_some() && s.tool == Tool::Text {
                            // Ctrl-drag while typing repositions the *active* text box
                            // (you stay in edit mode) — place the caption as you write it.
                            s.typing_drag = true;
                            s.move_from = Some(p);
                        } else if ctrl || s.tool == Tool::Move {
                            // Move tool — or Ctrl-drag with any tool — grabs the
                            // topmost shape under the cursor (if any).
                            s.selected = tools::hit_shape(&s.shapes, p.x, p.y);
                            s.move_from = s.selected.map(|_| p);
                        } else if s.tool == Tool::Eyedropper {
                            sample_pixel(s, p); // grab the pixel's colour; never draws
                        } else if s.tool == Tool::Text {
                            // Click while typing = finish & deselect (no new box on this
                            // click); a click when idle starts a fresh box. Predictable
                            // "click away to commit" instead of spawning an empty box you
                            // then have to Esc out of.
                            if s.typing.is_some() {
                                commit_text(s);
                            } else {
                                s.typing = Some((p, String::new()));
                                s.pending_hi = None; // fresh buffer, no half-typed surrogate
                            }
                        } else if s.tool == Tool::Number {
                            let n = s.number_next;
                            s.number_next += 1;
                            let color = s.color();
                            s.shapes.push(Shape::Number { at: p, n, color });
                            s.redo.clear();
                        } else {
                            s.draw_from = Some(p);
                            s.pen_pts.clear();
                            s.pen_pts.push(p);
                            s.cur = p;
                        }
                    }
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let s = &mut *shot_ptr(hwnd);
                let p = pt(lparam);
                // The Eyedropper loupe tracks the cursor: clear the "copied" flash and
                // repaint just the old + new loupe areas (not the whole virtual screen,
                // which would be a heavy blit per tick on a multi-monitor desktop).
                if s.tool == Tool::Eyedropper {
                    let old = loupe_rect(s, s.cur.x, s.cur.y);
                    let new = loupe_rect(s, p.x, p.y);
                    s.eye_copied = false;
                    let _ = InvalidateRect(Some(hwnd), Some(&old), false);
                    let _ = InvalidateRect(Some(hwnd), Some(&new), false);
                }
                s.cur = p;
                if s.sel_dragging {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if s.typing_drag {
                    // Reposition the active text box by the cursor delta (still editing).
                    if let Some(from) = s.move_from {
                        if let Some((at, _)) = s.typing.as_mut() {
                            at.x += p.x - from.x;
                            at.y += p.y - from.y;
                        }
                        s.move_from = Some(p);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if let (Some(from), Some(idx)) = (s.move_from, s.selected) {
                    // Drag the grabbed shape by the cursor delta.
                    let (dx, dy) = (p.x - from.x, p.y - from.y);
                    if idx < s.shapes.len() {
                        tools::translate_shape(&mut s.shapes[idx], dx, dy);
                    }
                    s.move_from = Some(p);
                    let _ = InvalidateRect(Some(hwnd), None, false);
                } else if s.draw_from.is_some() {
                    if s.tool == Tool::Pen {
                        s.pen_pts.push(p);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                // Track which toolbar button we're hovering (only when idle), and
                // (re)arm the hover-delay timer so the tooltip pops after a beat.
                let idle = !s.sel_dragging && s.draw_from.is_none() && s.move_from.is_none();
                let hovered = match (idle, s.sel) {
                    (true, Some(sel)) => toolbar::hit(&toolbar::layout(sel, s.vw, s.vh, dpi_for_sel(sel)), p.x, p.y),
                    _ => None,
                };
                if hovered != s.hover_btn {
                    s.hover_btn = hovered;
                    s.tip_show = false;
                    let _ = KillTimer(Some(hwnd), HOVER_TIMER);
                    if hovered.is_some() {
                        let _ = SetTimer(Some(hwnd), HOVER_TIMER, 450, None);
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let s = &mut *shot_ptr(hwnd);
                let p = pt(lparam);
                if s.sel_dragging {
                    s.sel_dragging = false;
                    let r = tools::norm(s.sel_anchor, p);
                    if (r.right - r.left) > 4 && (r.bottom - r.top) > 4 {
                        s.sel = Some(r);
                    }
                } else if s.typing_drag {
                    s.typing_drag = false;
                    s.move_from = None; // done repositioning the active text box
                } else if s.move_from.is_some() {
                    s.move_from = None; // finished dragging the selected shape
                } else if let Some(a) = s.draw_from.take() {
                    finish_shape(s, a, p);
                }
                let _ = InvalidateRect(Some(hwnd), None, false);
                LRESULT(0)
            }
            WM_CHAR => {
                let s = &mut *shot_ptr(hwnd);
                // WM_CHAR carries one UTF-16 code unit — decode it (not a single ASCII
                // byte) so accented and other Unicode characters type correctly. A
                // non-BMP character arrives as a high+low surrogate pair across two
                // messages; buffer the high half until its low half lands.
                if s.typing.is_some() {
                    let u = (wparam.0 & 0xFFFF) as u16;
                    if let Some(hi) = s.pending_hi.take() {
                        // Expecting the low half of a surrogate pair.
                        if (0xDC00..=0xDFFF).contains(&u) {
                            if let Some(ch) = char::decode_utf16([hi, u]).next().and_then(|r| r.ok()) {
                                if let Some((_, buf)) = s.typing.as_mut() {
                                    buf.push(ch);
                                }
                            }
                            let _ = InvalidateRect(Some(hwnd), None, false);
                            return LRESULT(0);
                        }
                        // Stray high surrogate without a matching low half — drop it and
                        // fall through to process `u` on its own.
                    }
                    if (0xD800..=0xDBFF).contains(&u) {
                        s.pending_hi = Some(u); // high surrogate — wait for its low half
                    } else if u == 0x08 {
                        if let Some((_, buf)) = s.typing.as_mut() {
                            buf.pop();
                        }
                    } else if u >= 0x20 {
                        // A BMP character (lone surrogates were handled above). Use the
                        // lossy path so an unexpected unpaired surrogate can't panic.
                        if let Some((_, buf)) = s.typing.as_mut() {
                            buf.push_str(&String::from_utf16_lossy(&[u]));
                        }
                    }
                    let _ = InvalidateRect(Some(hwnd), None, false);
                    return LRESULT(0);
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if handle_key(hwnd, wparam.0 as u16) {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                let s = &mut *shot_ptr(hwnd);
                if wparam.0 == HOVER_TIMER {
                    let _ = KillTimer(Some(hwnd), HOVER_TIMER);
                    if s.hover_btn.is_some() && !s.tip_show {
                        s.tip_show = true;
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Only override the client area; let the default handle the rest.
                if (lparam.0 & 0xffff) as u32 != HTCLIENT {
                    return DefWindowProcW(hwnd, msg, wparam, lparam);
                }
                let s = &*shot_ptr(hwnd);
                let p = s.cur; // last client-space mouse pos (WM_SETCURSOR precedes the move)
                let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;
                // Over the toolbar, or over an open flyout panel?
                let over_ui = s.sel.is_some_and(|sel| {
                    let dpi = dpi_for_sel(sel);
                    let buttons = toolbar::layout(sel, s.vw, s.vh, dpi);
                    if toolbar::hit(&buttons, p.x, p.y).is_some() {
                        return true;
                    }
                    if s.color_flyout {
                        if let Some((_, cbr)) = buttons.iter().find(|(b, _)| *b == Button::Color) {
                            let (panel, _) = toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
                            return pt_in(panel, p);
                        }
                    }
                    if s.text_flyout {
                        if let Some((_, tbr)) = buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text)) {
                            let (panel, _) = toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi);
                            return pt_in(panel, p);
                        }
                    }
                    false
                });
                // Arrow over the UI; move-cursor while Ctrl is held (move mode);
                // cross otherwise (the capture default).
                let id = if over_ui {
                    IDC_ARROW
                } else if ctrl {
                    IDC_SIZEALL
                } else {
                    IDC_CROSS
                };
                if let Ok(cur) = LoadCursorW(None, id) {
                    SetCursor(Some(cur));
                }
                LRESULT(1)
            }
            WM_PAINT => {
                shot_paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let ptr = shot_ptr(hwnd);
                if !ptr.is_null() {
                    let s = Box::from_raw(ptr);
                    let _ = DeleteDC(s.shot);
                    let _ = DeleteObject(HGDIOBJ(s.shot_bmp.0));
                    let _ = DeleteDC(s.dimmed);
                    let _ = DeleteObject(HGDIOBJ(s.dimmed_bmp.0));
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

/// Keyboard: tool shortcuts, colour/thickness, undo/redo, accept (Enter →
/// clipboard+save), cancel (Esc). Returns true if a repaint is needed.
unsafe fn handle_key(hwnd: HWND, vk: u16) -> bool {
    let s = &mut *shot_ptr(hwnd);
    let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000) != 0;

    // Ignore the close keys for a moment after the overlay opens, so the keystroke
    // that fired the launching hotkey can't instantly cancel/accept the capture.
    if (vk == VK_ESCAPE.0 || vk == VK_RETURN.0)
        && GetTickCount64().saturating_sub(s.born) < SETTLE_CLOSE_MS
    {
        return false;
    }

    if vk == VK_ESCAPE.0 {
        if s.typing.is_some() {
            s.typing = None; // cancel the in-progress text only
            s.typing_drag = false; // and any active reposition drag
            s.pending_hi = None; // drop any half-typed surrogate
            return true;
        }
        let _ = DestroyWindow(hwnd);
        return false;
    }
    if vk == VK_RETURN.0 {
        // Enter = accept to the clipboard (the quick "I'm done, it's copied" gesture).
        // Saving to a file is the explicit Ctrl+S / Save-button action.
        commit_text(s);
        if s.sel.is_some() {
            finish_copy(s);
        }
        let _ = DestroyWindow(hwnd);
        return false;
    }
    // While typing text, swallow every other key here (the characters are inserted
    // by WM_CHAR) so letters go into the text instead of triggering tool shortcuts.
    if s.typing.is_some() {
        return false;
    }

    // Move tool: Delete removes the grabbed shape.
    if vk == VK_DELETE.0 {
        if let Some(idx) = s.selected.take() {
            if idx < s.shapes.len() {
                s.shapes.remove(idx);
                s.redo.clear();
            }
        }
        s.move_from = None;
        return true;
    }

    if ctrl && vk == b'Z' as u16 {
        if let Some(sh) = s.shapes.pop() {
            s.redo.push(sh);
        }
        s.selected = None; // indices may have shifted
        return true;
    }
    if ctrl && vk == b'Y' as u16 {
        if let Some(sh) = s.redo.pop() {
            s.shapes.push(sh);
        }
        s.selected = None;
        return true;
    }
    // Ctrl+C = copy to clipboard; Ctrl+S = save. Both accept + close (only once a
    // region exists). Ctrl+S keeps the overlay open if the Save-As prompt is cancelled.
    // Checked before the plain-letter tool shortcuts below, so 'C' alone is still Ellipse.
    if ctrl && vk == b'C' as u16 {
        if s.sel.is_some() {
            commit_text(s);
            finish_copy(s);
            let _ = DestroyWindow(hwnd);
        }
        return false;
    }
    if ctrl && vk == b'S' as u16 {
        if s.sel.is_some() {
            commit_text(s);
            if finish_save(hwnd, s) {
                let _ = DestroyWindow(hwnd);
            }
        }
        return false;
    }

    let new_tool = match vk {
        x if x == b'R' as u16 => Some(Tool::Rect),
        x if x == b'O' as u16 || x == b'C' as u16 => Some(Tool::Ellipse),
        x if x == b'A' as u16 => Some(Tool::Arrow),
        x if x == b'L' as u16 => Some(Tool::Line),
        x if x == b'P' as u16 => Some(Tool::Pen),
        x if x == b'T' as u16 => Some(Tool::Text),
        x if x == b'N' as u16 => Some(Tool::Number),
        x if x == b'H' as u16 => Some(Tool::Highlight),
        x if x == b'B' as u16 => Some(Tool::Pixelate), // B = blur/blockify
        x if x == b'I' as u16 => Some(Tool::Invert),
        x if x == b'E' as u16 => Some(Tool::Eyedropper),
        x if x == b'M' as u16 => Some(Tool::Move),
        _ => None,
    };
    if let Some(t) = new_tool {
        commit_text(s);
        s.tool = t;
        s.selected = None; // dropping the move selection when switching tools
        s.move_from = None;
        s.typing_drag = false;
        return true;
    }
    if vk == b'K' as u16 {
        s.cycle_color();
        return true;
    }
    if vk == 0xDB {
        // VK_OEM_4 '[' — text size while the Text tool is active, else line thickness.
        if s.tool == Tool::Text {
            let sz = (-s.text_font.lfHeight - 2).max(10);
            s.text_font.lfHeight = -sz;
        } else {
            s.thickness = (s.thickness - 1).max(1);
        }
        return true;
    }
    if vk == 0xDD {
        // VK_OEM_6 ']'
        if s.tool == Tool::Text {
            let sz = (-s.text_font.lfHeight + 2).min(96);
            s.text_font.lfHeight = -sz;
        } else {
            s.thickness = (s.thickness + 1).min(40);
        }
        return true;
    }
    false
}

/// Turn the finished drag (anchor `a` → release `b`) into a [`Shape`].
fn finish_shape(s: &mut Shot, a: POINT, b: POINT) {
    let color = s.color();
    let w = s.thickness;
    let shape = match s.tool {
        Tool::Rect => Shape::Rect { r: tools::norm(a, b), color, w },
        Tool::Ellipse => Shape::Ellipse { r: tools::norm(a, b), color, w },
        Tool::Arrow => Shape::Arrow { a, b, color, w },
        Tool::Line => Shape::Line { a, b, color, w },
        Tool::Pen => Shape::Pen { pts: std::mem::take(&mut s.pen_pts), color, w },
        Tool::Highlight => Shape::Highlight { r: tools::norm(a, b), color },
        Tool::Pixelate => Shape::Pixelate { r: tools::norm(a, b) },
        Tool::Invert => Shape::Invert { r: tools::norm(a, b) },
        Tool::Text | Tool::Number | Tool::Eyedropper | Tool::Move => return,
    };
    // Skip a tiny accidental drag for any rect-based shape.
    if matches!(&shape,
        Shape::Rect { r, .. } | Shape::Ellipse { r, .. } | Shape::Highlight { r, .. }
            | Shape::Pixelate { r } | Shape::Invert { r }
        if (r.right - r.left).abs() < 3 && (r.bottom - r.top).abs() < 3)
    {
        return;
    }
    s.shapes.push(shape);
    s.redo.clear();
}

/// The Eyedropper: read the colour of the frozen-screenshot pixel under `p`, make it
/// the active drawing colour, and copy `#RRGGBB` to the clipboard — the whole point of
/// the tool is "grab a colour and go." Client coords map 1:1 to the snapshot (the
/// overlay spans the virtual screen from its top-left), so we read straight from
/// `s.shot`. A `CLR_INVALID` read (cursor past the bitmap edge) is ignored.
unsafe fn sample_pixel(s: &mut Shot, p: POINT) {
    let c = GetPixel(s.shot, p.x, p.y);
    if c.0 == 0xFFFF_FFFF {
        return; // CLR_INVALID — outside the snapshot
    }
    s.cur_color = c;
    let (r, g, b) = (c.0 & 0xFF, (c.0 >> 8) & 0xFF, (c.0 >> 16) & 0xFF);
    let _ = crate::win::set_clipboard_text(&format!("#{r:02X}{g:02X}{b:02X}"));
    s.eye_copied = true; // flash the loupe's "Copied" confirmation until the next move
}

// ---- Eyedropper magnifier loupe --------------------------------------------
// A port of the standalone `--eyedropper` picker's loupe into the overlay. We
// can't just launch that tool here: it would snapshot the *dimmed* overlay and
// pick washed-out colours. Instead we draw the same magnifier from the overlay's
// own BRIGHT snapshot (`s.shot`) so the zoomed pixels + sampled colour are true.

const LOUPE_K: i32 = 7; // half-window: a (2K+1)² block of screen pixels shown
const LOUPE_SPAN: i32 = 2 * LOUPE_K + 1; // 15 px sampled across
const LOUPE_MAG: i32 = 150; // magnified loupe size (design px) → 10× zoom
const LOUPE_LBL: i32 = 46; // label strip height (design px): hex row + hint row

/// Sample `(r, g, b)` from the bright snapshot DC at `(x, y)` (clamped in-bounds).
unsafe fn shot_sample(shot: HDC, x: i32, y: i32, vw: i32, vh: i32) -> (u8, u8, u8) {
    let x = x.clamp(0, (vw - 1).max(0));
    let y = y.clamp(0, (vh - 1).max(0));
    let c = GetPixel(shot, x, y).0; // 0x00BBGGRR, or CLR_INVALID
    if c == 0xFFFF_FFFF {
        return (0, 0, 0);
    }
    ((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8)
}

/// The loupe box (magnifier + label strip) for a cursor at `(cx, cy)`, nudged to
/// stay fully on the virtual screen. `mag`/`lbl`/`gap` are already DPI-scaled.
fn loupe_box(cx: i32, cy: i32, vw: i32, vh: i32, mag: i32, lbl: i32, gap: i32) -> RECT {
    let (bw, bh) = (mag, mag + lbl);
    let mut bx = cx + gap;
    let mut by = cy + gap;
    if bx + bw > vw {
        bx = cx - gap - bw;
    }
    if by + bh > vh {
        by = cy - gap - bh;
    }
    bx = bx.clamp(0, (vw - bw).max(0));
    by = by.clamp(0, (vh - bh).max(0));
    RECT { left: bx, top: by, right: bx + bw, bottom: by + bh }
}

/// The on-screen rect the loupe occupies for a cursor at `(cx, cy)` — used to
/// invalidate just the old/new loupe area on each move instead of the whole screen.
unsafe fn loupe_rect(s: &Shot, cx: i32, cy: i32) -> RECT {
    let dpi = match s.sel {
        Some(sel) => dpi_for_sel(sel),
        None => dpi_for_sel(RECT { left: cx, top: cy, right: cx + 1, bottom: cy + 1 }),
    };
    let mag = crate::win::dpi_scale_dpi(LOUPE_MAG, dpi);
    let lbl = crate::win::dpi_scale_dpi(LOUPE_LBL, dpi);
    let gap = crate::win::dpi_scale_dpi(18, dpi);
    loupe_box(cx, cy, s.vw, s.vh, mag, lbl, gap)
}

/// Draw the magnifier loupe near `(cx, cy)` into `hdc`, zooming the bright `shot`.
/// Mirrors the standalone picker: nearest-neighbour 10× block, a red ring on the
/// picked pixel, then a swatch + `#RRGGBB` + status hint. `copied` flips the hint to
/// a confirmation right after a pick.
// Geometry + state are all distinct scalars the GDI draw needs; bundling them into a
// struct would just move the arg list, so allow the count here.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_loupe(hdc: HDC, shot: HDC, cx: i32, cy: i32, vw: i32, vh: i32, dpi: i32, copied: bool) {
    let mag = crate::win::dpi_scale_dpi(LOUPE_MAG, dpi);
    let lbl = crate::win::dpi_scale_dpi(LOUPE_LBL, dpi);
    let gap = crate::win::dpi_scale_dpi(18, dpi);
    let pad = crate::win::dpi_scale_dpi(5, dpi);
    let swsz = crate::win::dpi_scale_dpi(16, dpi);
    let lb = loupe_box(cx, cy, vw, vh, mag, lbl, gap);
    let (bx, by) = (lb.left, lb.top);

    // The LOUPE_SPAN² sample window around the cursor, shifted to stay fully inside
    // the snapshot so StretchBlt never reads out of bounds (cursor at a screen edge).
    // `kx`/`ky` are the cursor pixel's cell within that (possibly shifted) window.
    let sx = (cx - LOUPE_K).clamp(0, (vw - LOUPE_SPAN).max(0));
    let sy = (cy - LOUPE_K).clamp(0, (vh - LOUPE_SPAN).max(0));
    let kx = (cx - sx).clamp(0, LOUPE_SPAN - 1);
    let ky = (cy - sy).clamp(0, LOUPE_SPAN - 1);

    // Magnified pixels — nearest-neighbour so each screen pixel is a crisp block.
    SetStretchBltMode(hdc, COLORONCOLOR);
    let _ = StretchBlt(hdc, bx, by, mag, mag, Some(shot), sx, sy, LOUPE_SPAN, LOUPE_SPAN, SRCCOPY);

    // Red ring on the cursor's cell (the pixel that gets picked). Boundaries are taken
    // per-edge from the StretchBlt grid so the ring stays aligned at any DPI.
    let cc = RECT {
        left: bx + kx * mag / LOUPE_SPAN,
        top: by + ky * mag / LOUPE_SPAN,
        right: bx + (kx + 1) * mag / LOUPE_SPAN,
        bottom: by + (ky + 1) * mag / LOUPE_SPAN,
    };
    let red = CreateSolidBrush(rgb(255, 40, 40));
    FrameRect(hdc, &cc, red);
    let _ = DeleteObject(red.into());

    // Label strip: swatch + hex (row 1), then a status hint (row 2).
    let (r, g, b) = shot_sample(shot, cx, cy, vw, vh);
    let strip = RECT { left: bx, top: by + mag, right: bx + mag, bottom: by + mag + lbl };
    let lbg = CreateSolidBrush(rgb(24, 24, 24));
    FillRect(hdc, &strip, lbg);
    let _ = DeleteObject(lbg.into());
    let sw = RECT { left: bx + pad, top: by + mag + pad, right: bx + pad + swsz, bottom: by + mag + pad + swsz };
    let swb = CreateSolidBrush(rgb(r, g, b));
    FillRect(hdc, &sw, swb);
    let _ = DeleteObject(swb.into());

    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(240, 240, 240));
    let mut hex = wide(&format!("#{r:02X}{g:02X}{b:02X}"));
    let hn = hex.len().saturating_sub(1);
    let mut hr = RECT { left: bx + pad * 2 + swsz, top: by + mag, right: bx + mag, bottom: by + mag + swsz + pad * 2 };
    DrawTextW(hdc, &mut hex[..hn], &mut hr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    let (hint_txt, hint_col) =
        if copied { ("Copied \u{2713}", rgb(120, 220, 120)) } else { ("Click to copy", rgb(150, 150, 150)) };
    SetTextColor(hdc, hint_col);
    let mut hint = wide(hint_txt);
    let hin = hint.len().saturating_sub(1);
    let mut hir = RECT { left: bx + pad, top: by + mag + swsz + pad, right: bx + mag, bottom: by + mag + lbl };
    DrawTextW(hdc, &mut hint[..hin], &mut hir, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

    // Outer + magnifier borders.
    let border = CreateSolidBrush(rgb(0, 0, 0));
    FrameRect(hdc, &RECT { left: bx, top: by, right: bx + mag, bottom: by + mag + lbl }, border);
    FrameRect(hdc, &RECT { left: bx, top: by, right: bx + mag, bottom: by + mag }, border);
    let _ = DeleteObject(border.into());
}

/// Commit a non-empty active text buffer into a placed Text shape.
fn commit_text(s: &mut Shot) {
    s.pending_hi = None; // any half-typed surrogate is abandoned when the buffer closes
    if let Some((at, buf)) = s.typing.take() {
        if !buf.is_empty() {
            let color = s.color();
            let font = s.text_font;
            s.shapes.push(Shape::Text { at, s: buf, color, font });
            s.redo.clear();
        }
    }
}

unsafe fn shot_paint(hwnd: HWND) {
    let s = &*shot_ptr(hwnd);
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    // Build the whole frame off-screen, then blit it once — this is what kills the
    // flicker (the screen was being assembled in several visible steps before).
    let mem = CreateCompatibleDC(Some(hdc));
    let frame_bmp = CreateCompatibleBitmap(hdc, s.vw, s.vh);
    let oldbmp = SelectObject(mem, HGDIOBJ(frame_bmp.0));

    // Dimmed screen everywhere; the selection shows through at full brightness.
    let _ = BitBlt(mem, 0, 0, s.vw, s.vh, Some(s.dimmed), 0, 0, SRCCOPY);
    let sel = match s.sel {
        Some(r) => r,
        None if s.sel_dragging => tools::norm(s.sel_anchor, s.cur),
        None => RECT { left: 0, top: 0, right: 0, bottom: 0 },
    };
    if sel.right > sel.left && sel.bottom > sel.top {
        let _ = BitBlt(mem, sel.left, sel.top, sel.right - sel.left, sel.bottom - sel.top, Some(s.shot), sel.left, sel.top, SRCCOPY);
    }

    // Annotations + the move-selection highlight + the in-progress shape + caret.
    // The overlay paints in screen space, so no coordinate offset (0, 0). Clip them
    // to the committed selection so the live preview matches the cropped output —
    // nothing drawn "into the void" outside the capture region (which would vanish
    // on copy/save). SaveDC/RestoreDC brackets the clip so the UI chrome below is
    // unclipped.
    let dc_state = SaveDC(mem);
    if let Some(r) = s.sel {
        let _ = IntersectClipRect(mem, r.left, r.top, r.right, r.bottom);
    }
    for sh in &s.shapes {
        tools::draw_shape(mem, 0, 0, sh);
    }
    if let Some(sh) = s.selected.and_then(|i| s.shapes.get(i)) {
        let bb = tools::shape_bbox(sh);
        let r = RECT { left: bb.left - 3, top: bb.top - 3, right: bb.right + 3, bottom: bb.bottom + 3 };
        tools::frame(mem, r, rgb(0, 200, 90), 1);
    }
    if let Some(a) = s.draw_from {
        tools::draw_inprogress(mem, 0, 0, s.tool, a, s.cur, s.color(), s.thickness, &s.pen_pts);
    }
    if let Some((at, buf)) = &s.typing {
        tools::draw_text(mem, 0, 0, *at, buf, s.color(), &s.text_font, true);
    }
    let _ = RestoreDC(mem, dc_state);

    // Selection outline + the floating toolbar (once committed) + the hint strip.
    if sel.right > sel.left && sel.bottom > sel.top {
        tools::frame(mem, sel, rgb(0, 174, 255), 1);
    }
    // Live "W × H" pixel readout while dragging the region, so the user can size things
    // precisely. Drag-only: once committed, the toolbar + hint strip take over the space.
    if s.sel_dragging && sel.right > sel.left && sel.bottom > sel.top {
        draw_dim_badge(mem, s, sel);
    }
    if let Some(committed) = s.sel {
        let dpi = dpi_for_sel(committed);
        let buttons = toolbar::layout(committed, s.vw, s.vh, dpi);
        toolbar::draw(mem, &buttons, s.tool, s.color(), dpi);
        if s.color_flyout {
            // The colour palette flyout (takes precedence over a tooltip).
            if let Some((_, cbr)) = buttons.iter().find(|(b, _)| *b == Button::Color) {
                let (panel, sw) = toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
                toolbar::draw_color_flyout(mem, panel, &sw, s.color());
            }
        } else if s.text_flyout {
            // The text settings flyout.
            if let Some((_, tbr)) = buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text)) {
                let (panel, its) = toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi);
                toolbar::draw_text_flyout(mem, panel, &its, &s.text_font, dpi);
            }
        } else if s.tip_show {
            // Hover tooltip (after the short delay) over the hovered button.
            if let Some(btn) = s.hover_btn {
                if let Some((_, r)) = buttons.iter().find(|(b, _)| *b == btn) {
                    toolbar::draw_tooltip(mem, *r, toolbar::button_tip(btn), s.vw, s.vh, dpi);
                }
            }
        }
    }
    draw_hint(mem, s);

    // The Eyedropper magnifier follows the cursor, on top of everything. Sized for
    // the monitor under the cursor (committed selection if there is one, else the
    // cursor point); drawn from the bright snapshot so the zoom shows true colours.
    if s.tool == Tool::Eyedropper {
        let dpi = match s.sel {
            Some(sel) => dpi_for_sel(sel),
            None => dpi_for_sel(RECT { left: s.cur.x, top: s.cur.y, right: s.cur.x + 1, bottom: s.cur.y + 1 }),
        };
        draw_loupe(mem, s.shot, s.cur.x, s.cur.y, s.vw, s.vh, dpi, s.eye_copied);
    }

    // One blit to the window.
    let _ = BitBlt(hdc, 0, 0, s.vw, s.vh, Some(mem), 0, 0, SRCCOPY);
    SelectObject(mem, oldbmp);
    let _ = DeleteObject(HGDIOBJ(frame_bmp.0));
    let _ = DeleteDC(mem);
    let _ = EndPaint(hwnd, &ps);
}

/// Alpha-blend a ~55%-opacity black layer over `dc` (a 1×1 black source stretched
/// over the whole area) — turning the snapshot copy into the dimmed surround.
unsafe fn apply_dim(dc: HDC, w: i32, h: i32) {
    let tmp = CreateCompatibleDC(Some(dc));
    let bmp = CreateCompatibleBitmap(dc, 1, 1);
    let old = SelectObject(tmp, HGDIOBJ(bmp.0));
    let br = CreateSolidBrush(rgb(0, 0, 0));
    FillRect(tmp, &RECT { left: 0, top: 0, right: 1, bottom: 1 }, br);
    let _ = DeleteObject(br.into());
    let bf = BLENDFUNCTION { BlendOp: AC_SRC_OVER as u8, BlendFlags: 0, SourceConstantAlpha: 140, AlphaFormat: 0 };
    let _ = AlphaBlend(dc, 0, 0, w, h, tmp, 0, 0, 1, 1, bf);
    SelectObject(tmp, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(tmp);
}

/// A small "W × H" pixel-size readout drawn while the region is being dragged, so the
/// user can gauge the exact size of what they're capturing. The overlay maps 1:1 to the
/// virtual screen, so `sel`'s width/height ARE the true output pixel dimensions — no DPI
/// conversion on the numbers (only the chip's own chrome scales). Styled like the hint
/// strip (dark chip, light text) with a hairline border for legibility over any content;
/// anchored at the selection's top-left, just above the rect (or just inside it when the
/// region hugs the top of the screen).
unsafe fn draw_dim_badge(hdc: HDC, s: &Shot, sel: RECT) {
    let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
    let dpi = dpi_for_sel(sel);
    let txt = format!("{w} \u{00d7} {h}");
    SelectObject(hdc, HGDIOBJ(gui_font().0));
    // Measure the text so the chip hugs it (DT_CALCRECT writes the extent into `calc`).
    let mut buf = wide(&txt);
    let n = buf.len().saturating_sub(1);
    let mut calc = RECT::default();
    DrawTextW(hdc, &mut buf[..n], &mut calc, DT_CALCRECT | DT_SINGLELINE | DT_LEFT);
    let padx = crate::win::dpi_scale_dpi(8, dpi);
    let pady = crate::win::dpi_scale_dpi(3, dpi);
    let gap = crate::win::dpi_scale_dpi(6, dpi);
    let bw = (calc.right - calc.left) + padx * 2;
    let bh = (calc.bottom - calc.top) + pady * 2;
    let bx = sel.left.min(s.vw - bw).max(0);
    let by = if sel.top - bh - gap >= 0 {
        sel.top - bh - gap // just above the selection
    } else {
        (sel.top + gap).min(s.vh - bh).max(0) // no room above → just inside the top
    };
    let bar = RECT { left: bx, top: by, right: bx + bw, bottom: by + bh };
    let bg = CreateSolidBrush(rgb(20, 20, 20));
    FillRect(hdc, &bar, bg);
    let _ = DeleteObject(bg.into());
    let border = CreateSolidBrush(rgb(90, 90, 90));
    FrameRect(hdc, &bar, border);
    let _ = DeleteObject(border.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(235, 235, 235));
    let mut tr = RECT { left: bx + padx, top: by, right: bx + bw, bottom: by + bh };
    DrawTextW(hdc, &mut buf[..n], &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
}

/// The instructional strip. Pinned to the selection's top-left once a region is
/// committed (so it's right by what you're working on, not stranded in the screen
/// corner); shown at the screen corner during the initial drag.
unsafe fn draw_hint(hdc: HDC, s: &Shot) {
    let c = s.cur_color.0;
    let (cr, cg, cb) = (c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF);
    // `[ ]` controls text size for the Text tool, line thickness otherwise.
    let sz = if s.tool == Tool::Text {
        format!("text {}", -s.text_font.lfHeight)
    } else {
        format!("size {}", s.thickness)
    };
    // Kept short — the toolbar tooltips carry the per-button shortcuts now.
    let txt = format!(
        "[{tool}]  ·  [ ] {sz}  ·  #{cr:02X}{cg:02X}{cb:02X}  ·  Ctrl-drag moves  ·  Enter copy+save  ·  Esc   (hover buttons for help)",
        tool = s.tool.label(),
    );
    // Size the strip for the monitor it sits on: the selection's monitor once
    // committed, else the monitor under the in-progress drag (or the cursor before a
    // drag). Falls back to 96 (identity), so a standard display is unchanged.
    let dpi = match s.sel {
        Some(sel) => dpi_for_sel(sel),
        None if s.sel_dragging => dpi_for_sel(tools::norm(s.sel_anchor, s.cur)),
        None => dpi_for_sel(RECT { left: s.cur.x, top: s.cur.y, right: s.cur.x + 1, bottom: s.cur.y + 1 }),
    };
    let bar_w = s.vw.min(crate::win::dpi_scale_dpi(980, dpi));
    let bar_h = crate::win::dpi_scale_dpi(26, dpi);
    let gap = crate::win::dpi_scale_dpi(6, dpi); // gap above the selection
    let inset = crate::win::dpi_scale_dpi(4, dpi); // inset when there's no room above
    // Anchor to the selection's top-left if committed; else the screen corner.
    let (bx, by) = match s.sel {
        Some(sel) => {
            let x = sel.left.min(s.vw - bar_w).max(0);
            let y = if sel.top - bar_h - gap >= 0 {
                sel.top - bar_h - gap // just above the selection
            } else {
                (sel.top + inset).min(s.vh - bar_h) // no room above → just inside the top
            };
            (x, y)
        }
        None => (0, 0),
    };
    let bg = CreateSolidBrush(rgb(20, 20, 20));
    let bar = RECT { left: bx, top: by, right: bx + bar_w, bottom: by + bar_h };
    FillRect(hdc, &bar, bg);
    let _ = DeleteObject(bg.into());
    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(235, 235, 235));
    let w = wide(&txt);
    let tx = crate::win::dpi_scale_dpi(10, dpi); // text left padding
    let ty = crate::win::dpi_scale_dpi(5, dpi); // text top padding
    let _ = TextOutW(hdc, bx + tx, by + ty, &w[..w.len().saturating_sub(1)]);
}

/// Composite the selected region (snapshot + annotations) into an offscreen DC
/// and pull its top-down BGRA pixels. Returns `(pixels, w, h)` — the callers route
/// it to the clipboard and/or a PNG.
unsafe fn compose(s: &Shot) -> Option<(Vec<u8>, i32, i32)> {
    let sel = s.sel?;
    let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    let screen = GetDC(None);
    let comp = CreateCompatibleDC(Some(screen));
    let cbmp = CreateCompatibleBitmap(screen, w, h);
    ReleaseDC(None, screen);
    let oldbmp = SelectObject(comp, HGDIOBJ(cbmp.0));
    let _ = BitBlt(comp, 0, 0, w, h, Some(s.shot), sel.left, sel.top, SRCCOPY);
    // Offset the annotations (screen space) into region space. We pass the shift
    // explicitly rather than via SetViewportOrgEx because GDI+ (the anti-aliased
    // drawing) ignores the DC's viewport origin — only plain GDI honours it.
    for sh in &s.shapes {
        tools::draw_shape(comp, -sel.left, -sel.top, sh);
    }

    let mut bi = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: core::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // negative = top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    // 64-bit size math (w/h are already > 0 above); bail on an absurd selection so
    // the i32 product can't overflow into an undersized buffer for GetDIBits.
    let n = w as i64 * h as i64 * 4;
    if n > i32::MAX as i64 {
        return None;
    }
    let mut buf = vec![0u8; n as usize];
    let got = GetDIBits(comp, cbmp, 0, h as u32, Some(buf.as_mut_ptr() as *mut c_void), &mut bi, DIB_RGB_COLORS);
    SelectObject(comp, oldbmp);
    let _ = DeleteDC(comp);
    let _ = DeleteObject(HGDIOBJ(cbmp.0));
    if got == 0 {
        return None;
    }
    Some((buf, w, h))
}

/// Copy the composited capture to the clipboard. (Caller commits in-progress text first.)
unsafe fn finish_copy(s: &Shot) {
    if let Some((buf, w, h)) = compose(s) {
        output::copy_dib_to_clipboard(&buf, w, h);
    }
}

/// Save the composited capture. With the "fixed save folder" option on, auto-saves a
/// timestamped PNG into the configured folder (Desktop by default) and returns true.
/// Otherwise prompts via a Save-As dialog and returns true iff the user picked a path
/// and it saved — false on cancel, so the caller can leave the overlay open. (Caller
/// commits in-progress text first.)
unsafe fn finish_save(hwnd: HWND, s: &Shot) -> bool {
    let Some((buf, w, h)) = compose(s) else { return false };
    if sagethumbs2k_core::settings::screenshot_use_save_dir() {
        let dir = super::effective_save_dir();
        let ok = output::save_png_to_dir(std::path::Path::new(&dir), &buf, w, h);
        if !ok {
            // A `false` here is a DISK failure (full/unwritable/missing folder), NOT a cancel
            // (the Save-As path can't run in this branch). Tell the user — otherwise the caller
            // treats false as "keep editing" and the capture silently never lands.
            with_modal(hwnd, || {
                let m = wide(&crate::win::t("shot_save_failed").replace("{dir}", &dir));
                let cap = wide("SageThumbs 2K");
                MessageBoxW(Some(hwnd), PCWSTR(m.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | MB_ICONWARNING);
            });
        }
        ok
    } else {
        let mut saved = false;
        // Drop the overlay's always-on-top so the picker isn't trapped behind the
        // fullscreen capture window (it pumps its own modal loop while shown).
        with_modal(hwnd, || {
            if let Some(path) =
                crate::win::pick_save_png(hwnd, &super::effective_save_dir(), &output::timestamped_name())
            {
                saved = output::save_png_to_path(std::path::Path::new(&path), &buf, w, h);
            }
        });
        saved
    }
}

/// Handle a toolbar button click. Returns true if it destroyed the window (the
/// caller must then stop touching `s`/`hwnd`).
unsafe fn handle_button(hwnd: HWND, s: &mut Shot, btn: Button) -> bool {
    match btn {
        Button::Tool(Tool::Text) => {
            if s.tool == Tool::Text {
                // Already active → toggle the text settings flyout.
                s.text_flyout = !s.text_flyout;
                if !s.text_flyout {
                    s.font_dropdown = false;
                }
            } else {
                commit_text(s);
                s.tool = Tool::Text;
                s.selected = None;
                s.move_from = None;
                s.text_flyout = true; // open settings when the Text tool is picked
            }
            s.color_flyout = false;
            false
        }
        Button::Tool(t) => {
            commit_text(s);
            s.tool = t;
            s.selected = None;
            s.move_from = None;
            s.typing_drag = false;
            s.text_flyout = false;
            s.font_dropdown = false;
            s.color_flyout = false;
            false
        }
        Button::Color => {
            s.color_flyout = !s.color_flyout;
            s.text_flyout = false;
            s.font_dropdown = false;
            false
        }
        Button::Undo => {
            if let Some(sh) = s.shapes.pop() {
                s.redo.push(sh);
            }
            false
        }
        Button::Redo => {
            if let Some(sh) = s.redo.pop() {
                s.shapes.push(sh);
            }
            false
        }
        Button::Copy => {
            commit_text(s);
            finish_copy(s);
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Save => {
            commit_text(s);
            if finish_save(hwnd, s) {
                let _ = DestroyWindow(hwnd);
                true
            } else {
                false // Save-As cancelled → keep the overlay open for more edits
            }
        }
        Button::Upload => {
            commit_text(s);
            if let Some((buf, w, h)) = compose(s) {
                if let Some(path) = output::save_temp_png(&buf, w, h) {
                    spawn_mode("--upload", &path);
                }
            }
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Close => {
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Sep => false, // not clickable (hit() skips separators)
    }
}

/// Spawn ourselves in `mode` (e.g. `--upload`) over `path`, separate process.
fn spawn_mode(mode: &str, path: &str) {
    super::spawn_self(&[mode, path]);
}

/// Is point `p` inside rect `r`?
fn pt_in(r: RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

/// Drop the overlay's always-on-top so a modal common dialog isn't hidden behind it,
/// run `f`, then restore topmost + repaint. (The dialog pumps its own message loop.)
unsafe fn with_modal<F: FnOnce()>(hwnd: HWND, f: F) {
    let _ = SetWindowPos(hwnd, Some(HWND_NOTOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    f();
    let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
    let _ = SetForegroundWindow(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Common-dialog hook: centre the dialog on the work area at init (without it the
/// colour/font dialogs drift to the top-left over our fullscreen owner).
unsafe extern "system" fn center_dialog_hook(hdlg: HWND, msg: u32, _w: WPARAM, _l: LPARAM) -> usize {
    if msg == WM_INITDIALOG {
        let mut dr = RECT::default();
        if GetWindowRect(hdlg, &mut dr).is_ok() {
            let (dw, dh) = (dr.right - dr.left, dr.bottom - dr.top);
            let mut wa = RECT::default();
            let _ = SystemParametersInfoW(
                SPI_GETWORKAREA,
                0,
                Some(&mut wa as *mut _ as *mut core::ffi::c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            let x = wa.left + ((wa.right - wa.left) - dw) / 2;
            let y = wa.top + ((wa.bottom - wa.top) - dh) / 2;
            let _ = SetWindowPos(hdlg, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE);
        }
    }
    0
}

/// The native Windows colour picker (Choose Colour), seeded with the current colour;
/// a chosen colour is remembered across captures.
unsafe fn pick_custom_color(hwnd: HWND, s: &mut Shot) {
    // Seed the dialog's 16 custom slots with the colours we remember.
    for (slot, c) in s.cust_colors.iter_mut().zip(s.customs.iter()) {
        *slot = *c;
    }
    with_modal(hwnd, || {
        let mut cc: CHOOSECOLORW = core::mem::zeroed();
        cc.lStructSize = core::mem::size_of::<CHOOSECOLORW>() as u32;
        cc.hwndOwner = hwnd;
        cc.rgbResult = s.cur_color;
        cc.lpCustColors = s.cust_colors.as_mut_ptr();
        cc.Flags = CC_RGBINIT | CC_FULLOPEN | CC_ANYCOLOR | CC_ENABLEHOOK;
        cc.lpfnHook = Some(center_dialog_hook);
        if ChooseColorW(&mut cc).as_bool() {
            s.cur_color = cc.rgbResult;
            super::prefs::remember_custom_color(cc.rgbResult);
            s.customs = super::prefs::load_custom_colors();
        }
    });
}

/// The native Windows font picker (Choose Font) — family, size, bold/italic,
/// underline, colour — seeded with the current text font + colour.
unsafe fn pick_text_font(hwnd: HWND, s: &mut Shot) {
    with_modal(hwnd, || {
        let mut lf = s.text_font;
        let mut cf: CHOOSEFONTW = core::mem::zeroed();
        cf.lStructSize = core::mem::size_of::<CHOOSEFONTW>() as u32;
        cf.hwndOwner = hwnd;
        cf.lpLogFont = &mut lf;
        cf.Flags = CF_SCREENFONTS | CF_INITTOLOGFONTSTRUCT | CF_EFFECTS | CF_ENABLEHOOK;
        cf.lpfnHook = Some(center_dialog_hook);
        cf.rgbColors = s.cur_color;
        if ChooseFontW(&mut cf).as_bool() {
            s.text_font = lf;
            s.cur_color = cf.rgbColors; // honour the dialog's colour control too
        }
    });
}
