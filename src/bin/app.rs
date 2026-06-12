//! SageThumbs 2K — Options.
//!
//! A native Win32 settings window (a faithful, modernized port of the original
//! SageThumbs Options dialog) that edits HKCU\Software\SageThumbs2K via the
//! crate's `settings` module, plus a per-format checkbox list. It is also the
//! `Application` entry the sparse package needs.
//!
//! Built programmatically (CreateWindowExW) rather than from a dialog-template
//! resource so it doesn't depend on a resource compiler — the GNU toolchain's
//! `windres`, like `dlltool`, chokes on the spaces in this project's path.
//!
//! Reachable settings take effect immediately (the provider reads them per
//! request). Changing the per-format list rewrites the HKCR `shellex` keys,
//! which needs elevation — handled by re-running `regsvr32` (which honors the
//! per-extension flags we just wrote) elevated, exactly as the original did.
#![windows_subsystem = "windows"]
#![allow(non_snake_case)]

use core::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::OnceLock;

use windows::core::{w, BOOL, PCSTR, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    GlobalFree, COLORREF, HANDLE, HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect, GetDC,
    GetPixel, GetTextExtentPoint32W, GetStockObject, InvalidateRect, ReleaseDC, SelectObject,
    SetBkColor, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, COLORONCOLOR,
    DEFAULT_GUI_FONT, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HBITMAP, HBRUSH, HDC, HFONT, HGDIOBJ,
    PAINTSTRUCT, SRCCOPY, TRANSPARENT,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows::Win32::System::Com::Urlmon::URLDownloadToFileW;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Controls::{
    GetComboBoxInfo, InitCommonControlsEx, SetWindowTheme, CDDS_ITEMPREPAINT, CDDS_PREPAINT,
    CDRF_DODEFAULT, CDRF_NOTIFYITEMDRAW, COMBOBOXINFO, ICC_LISTVIEW_CLASSES, INITCOMMONCONTROLSEX,
    LVCFMT_LEFT, LVCF_FMT, LVCF_TEXT, LVCF_WIDTH, LVCOLUMNW, LVIF_TEXT, LVIS_STATEIMAGEMASK, LVITEMW,
    LVM_GETHEADER, LVM_GETITEMSTATE, LVM_GETNEXTITEM, LVM_GETSELECTEDCOUNT, LVM_INSERTCOLUMNW,
    LVM_INSERTITEMW, LVM_SETBKCOLOR, LVM_SETCOLUMNW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE,
    LVM_SETITEMW, LVM_SETTEXTBKCOLOR, LVM_SETTEXTCOLOR, LVNI_FOCUSED, LVNI_SELECTED,
    LVS_EX_CHECKBOXES, LVS_EX_FULLROWSELECT, LVS_NOSORTHEADER, LVS_REPORT, LIST_VIEW_ITEM_STATE_FLAGS,
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, NMCUSTOMDRAW, NMHDR, NMLINK, NM_CLICK, NM_CUSTOMDRAW,
    NM_RETURN, ODS_SELECTED, ODT_MENU, ICC_LINK_CLASS, ICC_STANDARD_CLASSES, WC_LISTVIEWW,
    TTTOOLINFOW, TTF_IDISHWND, TTF_SUBCLASS, TTM_ADDTOOLW, TTM_SETMAXTIPWIDTH,
    NMTTDISPINFOW, TTN_GETDISPINFOW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, SetFocus, VK_ESCAPE, VK_SPACE};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass, ShellExecuteW};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows_registry::CURRENT_USER;

use sagethumbs2k::{convert_file_opts, formats, i18n, settings, ConvertOpts, Resize, Target};

// --- Convert… dialog (launched by the DLL verb as `--convert <listfile>`) ---
use image::ImageFormat;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::Controls::{
    ICC_BAR_CLASSES, ICC_PROGRESS_CLASS, PBM_SETPOS, PBM_SETRANGE32, TBM_SETPOS, TBM_SETRANGE,
    TBS_HORZ,
};
const TBM_GETPOS: u32 = 0x0400; // WM_USER + 0 (not surfaced by this metadata)
use windows::Win32::UI::Shell::{
    FileOpenDialog, IFileOpenDialog, IShellItem, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
    SIGDN_FILESYSPATH,
};

/// Shorthand for a translated UI string in the active language.
fn t(key: &str) -> &'static str {
    i18n::t(key)
}

// ---- Control IDs --------------------------------------------------------
const IDOK: i32 = 1;
const IDCANCEL: i32 = 2;
const ID_ENABLE_THUMBS: i32 = 1001;
const ID_USE_EMBEDDED: i32 = 1002;
const ID_ENABLE_MENU: i32 = 1003;
const ID_MAXSIZE: i32 = 1004;
const ID_SIZE: i32 = 1005;
const ID_JPEG: i32 = 1006;
const ID_PNG: i32 = 1007;
const ID_LIST: i32 = 1008;
const ID_SELECT_ALL: i32 = 1009;
const ID_CLEAR_ALL: i32 = 1010;
const ID_DEFAULTS: i32 = 1011;
// Translatable static labels (need IDs so the language picker can relabel live).
const ID_LBL_THUMBS: i32 = 1100;
const ID_LBL_LIMITS: i32 = 1101;
const ID_LBL_MAXFILE: i32 = 1102;
const ID_LBL_MAXTHUMB: i32 = 1103;
const ID_LBL_JPEG: i32 = 1104;
const ID_LBL_PNG: i32 = 1105;
const ID_LBL_FORMATS: i32 = 1106;
const ID_LBL_LANG: i32 = 1107;
const ID_LANG: i32 = 1108;
// Ebook/comic archive cover options.
const ID_LBL_EBOOK: i32 = 1109;
const ID_C_SORT: i32 = 1110;
const ID_C_PREFER_COVER: i32 = 1111;
const ID_C_SKIP_SCAN: i32 = 1112;
// Company promotion (footer link + clickable banner + About box).
const ID_ABOUT: i32 = 1113;
const ID_PROMO_LINK: i32 = 1114;
const ID_BANNER: i32 = 1115;
// Context-menu preview placement (Off / submenu / main menu).
const ID_LBL_PREVIEW: i32 = 1116;
const ID_MENU_PREVIEW: i32 = 1117;
const ID_ABOUT_LINK: i32 = 1116;
// Quick verbs directly on the main right-click menu.
const ID_MENU_QUICK: i32 = 1118;
// The clickable LunarWerx wordmark in the About box.
const ID_LW_LOGO: i32 = 1119;

// --- Branding (edit these / swap the assets to rebrand) -----------------
const URL_PARENT: &str = "https://lunarwerx.com";
const URL_PRODUCT: &str = "https://connections.icu";
const URL_GITHUB: &str = "https://github.com/LunarWerxs/SageThumbs-2k";
/// The About box's LunarWerx wordmark links here.
const URL_COMPANIES: &str = "https://lunarwerx.com/#companies";
/// Remote ad manifest — a small JSON file, loaded at runtime so the ads can change
/// without a rebuild. Schema:
/// `{ "rotate_seconds": N, "random": bool, "ads": [ { "image": url | [url, …],
/// "text": tip, "link": url }, … ] }`. Each ad ("company") carries one click
/// `link` + one hover `text`, and either a single `image` or a list of them — when
/// it's a list, a random one is shown each time that company comes up. There is
/// **no** single-image fallback: if the JSON is missing or unparseable the embedded
/// `banner.png` placeholder simply stays put, making a broken feed visible.
const BANNER_URL: &str = "https://connections.icu/sagethumbs2k_ad1";

/// Posted from the manifest-download thread once every ad's art is decoded
/// (wParam = banner HWND, lParam = `*mut AdRotator`); installed on the UI thread.
const WM_APP_ADS: u32 = 0x8000 + 7; // WM_APP + 7
const TIMER_BANNER: usize = 1; // animates the current image's GIF frames
const TIMER_ROTATE: usize = 2; // advances to the next ad / image

/// One decoded piece of art: a single HBITMAP for a still image, or many for an
/// animated GIF (with the inter-frame `delay_ms`).
struct AdImage {
    frames: Vec<isize>, // one HBITMAP handle per frame
    delay_ms: u32,      // GIF inter-frame delay (ignored for stills)
}

/// One ad ("company") in the rotation: one or more interchangeable images, a hover
/// `tip`, and a click `link` (both NUL-terminated wide, ready for the tooltip /
/// ShellExecute). With several images, a random one is shown per appearance.
struct Ad {
    images: Vec<AdImage>,
    tip: Vec<u16>,
    link: Vec<u16>,
}

/// The banner's ad-rotation state, owned by the banner control via GWLP_USERDATA
/// and freed on WM_DESTROY. TIMER_ROTATE advances the company (random or in order)
/// and re-picks its image; TIMER_BANNER animates the current image while it's a GIF.
struct AdRotator {
    ads: Vec<Ad>,
    cur: usize,   // current company
    img: usize,   // current image within the company
    frame: usize, // current GIF frame within the image
    rotate_ms: u32,
    random: bool,
    rng: u32, // xorshift state for random company + image picks
}

impl AdRotator {
    /// Build from decoded ads. When `random`, start on a random company so a fresh
    /// open doesn't always show ad #0 (the bug where the banner looked "stuck").
    fn new(ads: Vec<Ad>, rotate_ms: u32, random: bool, mut rng: u32) -> Self {
        let cur = if random && ads.len() > 1 { (xorshift(&mut rng) as usize) % ads.len() } else { 0 };
        let mut r = Self { ads, cur, img: 0, frame: 0, rotate_ms, random, rng };
        r.pick_image();
        r
    }

    /// Pick a (random, if several) image within the current company; reset to its
    /// first frame.
    fn pick_image(&mut self) {
        self.frame = 0;
        let m = self.ads.get(self.cur).map_or(0, |a| a.images.len());
        self.img = if m > 1 { (xorshift(&mut self.rng) as usize) % m } else { 0 };
    }

    /// Advance to the next company (random avoids an immediate repeat; otherwise in
    /// order) and re-pick its image. A lone company just re-rolls its own images.
    fn advance(&mut self) {
        let n = self.ads.len();
        if n > 1 {
            self.cur = if self.random {
                let mut k = (xorshift(&mut self.rng) as usize) % n;
                if k == self.cur {
                    k = (k + 1) % n;
                }
                k
            } else {
                (self.cur + 1) % n
            };
        }
        self.pick_image();
    }

    /// The image currently on display (its frames + delay).
    fn current(&self) -> Option<&AdImage> {
        self.ads.get(self.cur).and_then(|a| a.images.get(self.img))
    }

    /// Whether anything actually rotates: more than one company, or any company
    /// with more than one image.
    fn rotates(&self) -> bool {
        self.ads.len() > 1 || self.ads.iter().any(|a| a.images.len() > 1)
    }
}

/// xorshift32 — enough randomness to shuffle ad order without an RNG crate.
fn xorshift(s: &mut u32) -> u32 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    *s = x;
    x
}
/// Logo + banner default artwork, embedded so they always render. A file of the
/// same name dropped next to the EXE overrides each at runtime (user-swappable).
const LOGO_PNG: &[u8] = include_bytes!("../../assets/logo.png");
const BANNER_PNG: &[u8] = include_bytes!("../../assets/banner.png");
/// LunarWerx wordmark (white-on-transparent, 1680×273) for the About box.
const LW_LOGO_PNG: &[u8] = include_bytes!("../../assets/lw_logo_white.png");
/// Window/taskbar icon (16/32/48). Embedded; the EXE-file icon in Explorer comes
/// from the installer's shortcut. A `app.ico` next to the EXE overrides at runtime.
const APP_ICO: &[u8] = include_bytes!("../../assets/app-win.ico");

const CHECKED: u32 = 0x2000; // INDEXTOSTATEIMAGEMASK(2)
const UNCHECKED: u32 = 0x1000; // INDEXTOSTATEIMAGEMASK(1)

fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s).encode_wide().chain(std::iter::once(0)).collect()
}

// ============================= Dark mode =============================
// Follows the system theme. Themed (v6) controls only render *dark* if the
// process opts in via the undocumented-but-stable uxtheme ordinals (135/133/104)
// — the same ones every dark-mode Win32 app uses — plus DWM for the title bar
// and WM_CTLCOLOR* for the bits the theme doesn't color (static/edit text).

const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF((r as u32) | ((g as u32) << 8) | ((b as u32) << 16))
}
const DARK_BG: COLORREF = rgb(32, 32, 32);
const DARK_CTL_BG: COLORREF = rgb(45, 45, 45);
const DARK_TEXT: COLORREF = rgb(232, 232, 232);

/// True when the system is in dark mode (AppsUseLightTheme == 0). Cached.
fn is_dark() -> bool {
    static DARK: OnceLock<bool> = OnceLock::new();
    *DARK.get_or_init(|| {
        CURRENT_USER
            .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
            .and_then(|k| k.get_u32("AppsUseLightTheme"))
            .map(|v| v == 0)
            .unwrap_or(false)
    })
}

type FnSetPreferredAppMode = unsafe extern "system" fn(i32) -> i32;
type FnAllowDarkModeForWindow = unsafe extern "system" fn(HWND, BOOL) -> BOOL;
type FnRefreshImmersive = unsafe extern "system" fn();

struct Uxtheme {
    set_preferred_app_mode: Option<FnSetPreferredAppMode>, // ordinal 135 (Win 1903+)
    allow_dark_for_window: Option<FnAllowDarkModeForWindow>, // ordinal 133
    refresh_immersive: Option<FnRefreshImmersive>,           // ordinal 104
}
unsafe impl Send for Uxtheme {}
unsafe impl Sync for Uxtheme {}

fn uxtheme() -> &'static Uxtheme {
    static U: OnceLock<Uxtheme> = OnceLock::new();
    U.get_or_init(|| unsafe {
        let h: HMODULE = LoadLibraryW(w!("uxtheme.dll")).unwrap_or_default();
        let by_ord = |ord: u16| GetProcAddress(h, PCSTR(ord as usize as *const u8));
        Uxtheme {
            set_preferred_app_mode: by_ord(135).map(|p| std::mem::transmute::<_, FnSetPreferredAppMode>(p)),
            allow_dark_for_window: by_ord(133).map(|p| std::mem::transmute::<_, FnAllowDarkModeForWindow>(p)),
            refresh_immersive: by_ord(104).map(|p| std::mem::transmute::<_, FnRefreshImmersive>(p)),
        }
    })
}

/// Put the process into "allow dark" mode — call once before creating windows.
unsafe fn init_dark_app() {
    let ux = uxtheme();
    if let Some(f) = ux.set_preferred_app_mode {
        f(1); // PreferredAppMode::AllowDark
    }
    if let Some(f) = ux.refresh_immersive {
        f();
    }
}

/// Opt one window/control into dark mode + apply a dark visual-style class.
unsafe fn dark_control(h: HWND, theme: PCWSTR) {
    if let Some(f) = uxtheme().allow_dark_for_window {
        let _ = f(h, BOOL(1));
    }
    let _ = SetWindowTheme(h, theme, PCWSTR::null());
}

/// Dark title bar via DWM.
unsafe fn dark_titlebar(h: HWND) {
    let on = BOOL(1);
    let _ = DwmSetWindowAttribute(
        h,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &on as *const _ as *const c_void,
        std::mem::size_of::<BOOL>() as u32,
    );
}

unsafe fn cached_brush(color: COLORREF, slot: &'static OnceLock<usize>) -> HBRUSH {
    HBRUSH(*slot.get_or_init(|| CreateSolidBrush(color).0 as usize) as *mut c_void)
}
unsafe fn dark_bg_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    cached_brush(DARK_BG, &B)
}
unsafe fn dark_ctl_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    cached_brush(DARK_CTL_BG, &B)
}
unsafe fn dark_menu_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    cached_brush(rgb(43, 43, 43), &B)
}
unsafe fn dark_menu_sel_brush() -> HBRUSH {
    static B: OnceLock<usize> = OnceLock::new();
    cached_brush(rgb(62, 62, 66), &B)
}

fn main() {
    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();

        // Resolve the UI language (HKCU override or system) before any control
        // is created so the dialog opens already localized.
        i18n::ensure_init();

        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES
                | ICC_LINK_CLASS
                | ICC_STANDARD_CLASSES
                | ICC_BAR_CLASSES
                | ICC_PROGRESS_CLASS,
        };
        let _ = InitCommonControlsEx(&icc);

        if is_dark() {
            init_dark_app();
        }

        // Convert… mode: `--convert <listfile>` (spawned by the DLL verb) shows
        // the batch-convert dialog instead of the Options window.
        let args: Vec<String> = std::env::args().collect();
        if let Some(pos) = args.iter().position(|a| a == "--convert") {
            if let Some(listfile) = args.get(pos + 1) {
                run_convert_dialog(hinst, listfile);
            }
            return;
        }
        // Eyedropper mode: `--eyedropper` (spawned by the DLL verb) opens the
        // system-wide screen color picker.
        if args.iter().any(|a| a == "--eyedropper") {
            run_eyedropper(hinst);
            return;
        }
        // Files-to-folder mode: `--files-to-folder <listfile>` (spawned by the DLL
        // verb for a multi-file selection) prompts for a folder name, then moves.
        if let Some(pos) = args.iter().position(|a| a == "--files-to-folder") {
            if let Some(listfile) = args.get(pos + 1) {
                run_files_to_folder_dialog(hinst, listfile);
            }
            return;
        }
        // Tags-to-folders mode: `--tags-to-folders <listfile>` (spawned by the DLL
        // verb) sorts audio files into folders by their tags.
        if let Some(pos) = args.iter().position(|a| a == "--tags-to-folders") {
            if let Some(listfile) = args.get(pos + 1) {
                run_tags_to_folders_dialog(hinst, listfile);
            }
            return;
        }

        let class = w!("SageThumbs2KOptions");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            hIcon: app_icon().unwrap_or_default(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            // Dark window background when the system is dark; otherwise the
            // classic button-face system color ((COLOR_BTNFACE + 1) as HBRUSH).
            hbrBackground: if is_dark() {
                dark_bg_brush()
            } else {
                HBRUSH(16isize as *mut c_void)
            },
            ..Default::default()
        };
        RegisterClassW(&wc);

        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let hwnd = CreateWindowExW(
            WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
            class,
            w!("SageThumbs 2K — Settings"),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            736,
            680,
            None,
            None,
            Some(hinst),
            None,
        )
        .expect("create window");

        if is_dark() {
            dark_control(hwnd, w!("DarkMode_Explorer"));
            dark_titlebar(hwnd);
        }

        let _ = ShowWindow(hwnd, SW_SHOW);

        let mut msg = MSG::default();
        loop {
            // GetMessageW returns -1 on error, 0 on WM_QUIT, >0 otherwise.
            // as_bool() (`!= 0`) would treat -1 as "keep going" and then spin on
            // a MSG it never populated — branch on the raw value instead.
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

/// The system message font (Segoe UI / Segoe UI Variable on Win11), cached.
/// Falls back to the stock GUI font if the metrics query fails.
unsafe fn gui_font() -> HFONT {
    static FONT: OnceLock<usize> = OnceLock::new();
    let p = *FONT.get_or_init(|| {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let hf = if SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok()
        {
            CreateFontIndirectW(&ncm.lfMessageFont)
        } else {
            HFONT(GetStockObject(DEFAULT_GUI_FONT).0)
        };
        hf.0 as usize
    });
    HFONT(p as *mut c_void)
}

/// Create a child control, set the GUI font, return its HWND.
#[allow(clippy::too_many_arguments)]
unsafe fn ctl(
    parent: HWND,
    class: PCWSTR,
    text: &str,
    style: WINDOW_STYLE,
    x: i32,
    y: i32,
    cw: i32,
    ch: i32,
    id: i32,
    hinst: HINSTANCE,
) -> HWND {
    let t = wide(text);
    let h = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        class,
        PCWSTR(t.as_ptr()),
        WS_CHILD | WS_VISIBLE | style,
        x,
        y,
        cw,
        ch,
        Some(parent),
        Some(HMENU(id as usize as *mut c_void)),
        Some(hinst),
        None,
    )
    .expect("create control");
    SendMessageW(h, WM_SETFONT, Some(WPARAM(gui_font().0 as usize)), Some(LPARAM(1)));
    if is_dark() {
        // Edit boxes use the dark common-file-dialog style; everything else the
        // dark Explorer style (themed checkbox glyphs, scrollbars, list rows).
        let theme = if class.0 == EDIT.0 {
            w!("DarkMode_CFD")
        } else {
            w!("DarkMode_Explorer")
        };
        dark_control(h, theme);
    }
    h
}

const STATIC: PCWSTR = w!("STATIC");
const BUTTON: PCWSTR = w!("BUTTON");
const EDIT: PCWSTR = w!("EDIT");
const COMBOBOX: PCWSTR = w!("COMBOBOX");
const SYSLINK: PCWSTR = w!("SysLink");

// STATIC control styles (not surfaced by this windows-rs metadata).
const SS_CENTER: u32 = 0x0000_0001;
const SS_BITMAP: u32 = 0x0000_000E;
const SS_NOTIFY: u32 = 0x0000_0100;

/// Open a URL in the default browser (user-initiated, via the company links).
unsafe fn open_url(url: &str) {
    let u = wide(url);
    let _ = ShellExecuteW(None, w!("open"), PCWSTR(u.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
}

/// A NUL-terminated wide buffer (e.g. a SysLink's szUrl) as a String.
fn wstr_to_string(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// Decode logo/banner artwork to an HBITMAP sized to `w`x`h`. Prefers a file of
/// `override_name` next to the EXE (user-swappable) and falls back to the
/// embedded `default_png`.
unsafe fn load_art(default_png: &[u8], override_name: &str, w: u32, h: u32) -> Option<HBITMAP> {
    let from_file = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(override_name)))
        .and_then(|f| std::fs::read(f).ok());
    let data = from_file.as_deref().unwrap_or(default_png);
    sagethumbs2k::image_to_hbitmap_sized(data, w, h).map(|h| HBITMAP(h as *mut c_void))
}

/// Load the app icon for the title bar + taskbar. Prefers an `app.ico` next to
/// the EXE (swappable), else the embedded icon written to a temp file (LoadImageW
/// needs a path). None if unavailable.
unsafe fn app_icon() -> Option<HICON> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("app.ico")))
        .filter(|p| p.exists());
    let path = beside.unwrap_or_else(|| {
        let mut p = std::env::temp_dir();
        p.push("sagethumbs2k.ico");
        let _ = std::fs::write(&p, APP_ICO);
        p
    });
    let w = wide(&path.to_string_lossy());
    let h = LoadImageW(None, PCWSTR(w.as_ptr()), IMAGE_ICON, 0, 0, LR_LOADFROMFILE | LR_DEFAULTSIZE).ok()?;
    Some(HICON(h.0))
}

/// Set a static control's bitmap, freeing whatever bitmap it held before.
unsafe fn set_static_bitmap(ctl: HWND, hbmp: HBITMAP) {
    let old = SendMessageW(ctl, STM_SETIMAGE, Some(WPARAM(IMAGE_BITMAP.0 as usize)), Some(LPARAM(hbmp.0 as isize)));
    if old.0 != 0 {
        let _ = DeleteObject(HGDIOBJ(old.0 as *mut c_void));
    }
}

/// Download `url` to a temp file and return its bytes (None on any failure).
/// Used to fetch the remote Options banner so it can be updated without a rebuild.
fn download_bytes(url: &str) -> Option<Vec<u8>> {
    let mut path = std::env::temp_dir();
    path.push("st2k_banner_dl.img");
    let u = wide(url);
    let p = wide(&path.to_string_lossy());
    unsafe { URLDownloadToFileW(None, PCWSTR(u.as_ptr()), PCWSTR(p.as_ptr()), 0, None).ok()? };
    let bytes = std::fs::read(&path).ok();
    let _ = std::fs::remove_file(&path);
    bytes
}

/// Download a URL while bypassing the WinINet cache entirely — used for the ad
/// manifest, whose stable entry URL 301-redirects to a fixed CDN object whose
/// *content* changes. `URLDownloadToFileW` caches aggressively (and caches 301s
/// permanently), and a cache-buster on the entry URL doesn't carry through the
/// redirect, so urlmon kept serving a stale manifest. `InternetOpenUrlW` with
/// `INTERNET_FLAG_RELOAD` forces a fresh fetch from the origin across the whole
/// redirect chain. Per-ad image URLs are versioned/immutable and still use the
/// cached `download_bytes` path. Returns None on any failure.
fn download_no_cache(url: &str) -> Option<Vec<u8>> {
    use windows::Win32::Networking::WinInet::{
        InternetCloseHandle, InternetOpenUrlW, InternetOpenW, InternetReadFile,
        INTERNET_FLAG_NO_CACHE_WRITE, INTERNET_FLAG_PRAGMA_NOCACHE, INTERNET_FLAG_RELOAD,
    };
    unsafe {
        let agent = wide("SageThumbs2K");
        let session = InternetOpenW(PCWSTR(agent.as_ptr()), 0, PCWSTR::null(), PCWSTR::null(), 0);
        if session.is_null() {
            return None;
        }
        let url_w = wide(url);
        let flags = INTERNET_FLAG_RELOAD | INTERNET_FLAG_NO_CACHE_WRITE | INTERNET_FLAG_PRAGMA_NOCACHE;
        let req = InternetOpenUrlW(session, PCWSTR(url_w.as_ptr()), None, flags, None);
        if req.is_null() {
            let _ = InternetCloseHandle(session);
            return None;
        }
        let mut data = Vec::new();
        let mut buf = [0u8; 16384];
        loop {
            let mut read = 0u32;
            if InternetReadFile(req, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut read).is_err() {
                break;
            }
            if read == 0 {
                break; // end of stream
            }
            data.extend_from_slice(&buf[..read as usize]);
        }
        let _ = InternetCloseHandle(req);
        let _ = InternetCloseHandle(session);
        (!data.is_empty()).then_some(data)
    }
}

/// Parse the ad manifest JSON and decode each ad's art (sized to `w`×`h`). Returns
/// the usable ads plus the rotation cadence (ms) and order flag, or None if the
/// JSON is unparseable, carries no `ads` array, or yields no decodable ad. Ads
/// whose image fails to download/decode are dropped. Creates GDI bitmaps (via
/// CreateDIBSection — no window needed), so it runs headless too.
fn build_ads_from_manifest(bytes: &[u8], w: u32, h: u32) -> Option<(Vec<Ad>, u32, bool)> {
    let manifest = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    let rotate_ms = (manifest.get("rotate_seconds").and_then(|v| v.as_u64()).unwrap_or(10).max(1)
        as u32)
        .saturating_mul(1000)
        .max(1000);
    let random = manifest.get("random").and_then(|v| v.as_bool()).unwrap_or(false);
    let items = manifest.get("ads").and_then(|v| v.as_array())?;

    let mut ads: Vec<Ad> = Vec::new();
    for item in items {
        let text = item.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let link = item.get("link").and_then(|v| v.as_str()).unwrap_or(URL_PRODUCT);
        // `image` is a single URL or a list of interchangeable URLs.
        let urls: Vec<&str> = if let Some(s) = item.get("image").and_then(|v| v.as_str()) {
            vec![s]
        } else if let Some(arr) = item.get("image").and_then(|v| v.as_array()) {
            arr.iter().filter_map(|v| v.as_str()).collect()
        } else {
            continue;
        };
        let mut images: Vec<AdImage> = Vec::new();
        for url in urls {
            let Some(img_bytes) = download_bytes(url) else { continue };
            // Animated GIF → many frames; anything else → one still frame.
            let (frames, delay_ms) =
                if let Some((fr, d)) = sagethumbs2k::decode_gif_frames_sized(&img_bytes, w, h) {
                    (fr, d)
                } else if let Some(handle) = sagethumbs2k::image_to_hbitmap_sized(&img_bytes, w, h) {
                    (vec![handle], 0)
                } else {
                    continue;
                };
            if frames.is_empty() {
                continue;
            }
            images.push(AdImage { frames, delay_ms });
        }
        if images.is_empty() {
            continue;
        }
        ads.push(Ad { images, tip: wide(text), link: wide(link) });
    }
    if ads.is_empty() {
        return None;
    }
    Some((ads, rotate_ms, random))
}

/// Fetch the ad manifest on a background thread: parse the JSON, download + decode
/// each ad's art (sized to the control), and hand the finished `AdRotator` to the
/// UI thread, which installs it and starts rotating. The embedded placeholder
/// stays until (and unless) ads arrive. No-op — and no single-image fallback — if
/// the manifest is missing, unparseable, or yields no usable ad.
fn spawn_remote_ads(banner: HWND, url: &'static str, w: u32, h: u32) {
    let hwnd = banner.0 as usize;
    std::thread::spawn(move || {
        let Some(bytes) = download_no_cache(url) else { return };
        let Some((ads, rotate_ms, random)) = build_ads_from_manifest(&bytes, w, h) else { return };

        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u32)
            .unwrap_or(0x9e37_79b9)
            | 1; // xorshift seed must be non-zero
        let rot = Box::into_raw(Box::new(AdRotator::new(ads, rotate_ms, random, seed)));

        let banner = HWND(hwnd as *mut c_void);
        unsafe {
            let parent = IsWindow(Some(banner)).as_bool().then(|| GetParent(banner).ok()).flatten();
            match parent {
                Some(p) => {
                    let _ = PostMessageW(Some(p), WM_APP_ADS, WPARAM(hwnd), LPARAM(rot as isize));
                }
                None => drop_ad_rotator(rot), // window gone; free everything
            }
        }
    });
}

/// Show the rotator's current image on the banner and (re)arm the GIF frame timer
/// for it. `free_prev` uses set_static_bitmap to delete the bitmap currently held
/// (only true for the very first swap, which frees the embedded placeholder); every
/// later swap reuses bitmaps that the rotator still owns, so it must NOT delete them.
unsafe fn show_current_image(hwnd: HWND, banner: HWND, r: &AdRotator, free_prev: bool) {
    let Some(img) = r.current() else { return };
    if let Some(&first) = img.frames.first() {
        if free_prev {
            set_static_bitmap(banner, HBITMAP(first as *mut c_void));
        } else {
            SendMessageW(banner, STM_SETIMAGE, Some(WPARAM(IMAGE_BITMAP.0 as usize)), Some(LPARAM(first)));
        }
    }
    let _ = KillTimer(Some(hwnd), TIMER_BANNER);
    if img.frames.len() > 1 {
        let _ = SetTimer(Some(hwnd), TIMER_BANNER, img.delay_ms.max(20), None);
    }
}

/// Free an ad rotator: every frame of every image of every ad, then the box.
unsafe fn drop_ad_rotator(ptr: *mut AdRotator) {
    if ptr.is_null() {
        return;
    }
    let rot = Box::from_raw(ptr);
    for ad in &rot.ads {
        for img in &ad.images {
            for &f in &img.frames {
                let _ = DeleteObject(HGDIOBJ(f as *mut c_void));
            }
        }
    }
}

unsafe fn build_controls(hwnd: HWND, hinst: HINSTANCE) {
    let cb = WINDOW_STYLE(BS_AUTOCHECKBOX as u32);
    let edit_style = WINDOW_STYLE((ES_NUMBER | ES_AUTOHSCROLL) as u32) | WS_BORDER | WS_TABSTOP;

    // ===== Left column: options =====
    ctl(hwnd, STATIC, t("grp_thumbnails"), WINDOW_STYLE(0), 16, 12, 300, 18, ID_LBL_THUMBS, hinst);
    ctl(hwnd, BUTTON, t("chk_enable_thumbs"), cb, 26, 38, 300, 22, ID_ENABLE_THUMBS, hinst);
    ctl(hwnd, BUTTON, t("chk_prefer_embedded"), cb, 26, 66, 300, 22, ID_USE_EMBEDDED, hinst);
    ctl(hwnd, BUTTON, t("chk_enable_menu"), cb, 26, 94, 300, 22, ID_ENABLE_MENU, hinst);

    // Context-menu preview placement (classic menu only).
    ctl(hwnd, STATIC, t("lbl_menu_preview"), WINDOW_STYLE(0), 26, 124, 130, 18, ID_LBL_PREVIEW, hinst);
    let prev = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        160,
        120,
        148,
        160,
        ID_MENU_PREVIEW,
        hinst,
    );
    for key in ["prev_off", "prev_submenu", "prev_main"] {
        let w = wide(t(key));
        SendMessageW(prev, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    SendMessageW(prev, CB_SETCURSEL, Some(WPARAM(settings::menu_preview() as usize)), None);
    dark_theme_combo(prev);

    ctl(hwnd, STATIC, t("grp_limits"), WINDOW_STYLE(0), 16, 166, 300, 18, ID_LBL_LIMITS, hinst);
    ctl(hwnd, STATIC, t("lbl_max_file"), WINDOW_STYLE(0), 26, 197, 190, 18, ID_LBL_MAXFILE, hinst);
    ctl(hwnd, EDIT, "", edit_style, 224, 194, 84, 24, ID_MAXSIZE, hinst);
    ctl(hwnd, STATIC, t("lbl_max_thumb"), WINDOW_STYLE(0), 26, 229, 190, 18, ID_LBL_MAXTHUMB, hinst);
    ctl(hwnd, EDIT, "", edit_style, 224, 226, 84, 24, ID_SIZE, hinst);
    ctl(hwnd, STATIC, t("lbl_jpeg"), WINDOW_STYLE(0), 26, 261, 190, 18, ID_LBL_JPEG, hinst);
    ctl(hwnd, EDIT, "", edit_style, 224, 258, 84, 24, ID_JPEG, hinst);
    ctl(hwnd, STATIC, t("lbl_png"), WINDOW_STYLE(0), 26, 293, 190, 18, ID_LBL_PNG, hinst);
    ctl(hwnd, EDIT, "", edit_style, 224, 290, 84, 24, ID_PNG, hinst);

    // Ebook & comic archive cover options (the DarkThumbs toggles).
    ctl(hwnd, STATIC, t("grp_ebook"), WINDOW_STYLE(0), 16, 330, 320, 18, ID_LBL_EBOOK, hinst);
    ctl(hwnd, BUTTON, t("chk_sort"), cb, 26, 356, 312, 22, ID_C_SORT, hinst);
    ctl(hwnd, BUTTON, t("chk_prefer_cover"), cb, 26, 384, 312, 22, ID_C_PREFER_COVER, hinst);
    ctl(hwnd, BUTTON, t("chk_skip_scanlation"), cb, 26, 412, 312, 22, ID_C_SKIP_SCAN, hinst);

    // Quick verbs (Convert/Resize/Rotate) directly on the main right-click menu.
    ctl(hwnd, BUTTON, t("chk_menu_quick"), cb, 26, 444, 312, 22, ID_MENU_QUICK, hinst);

    // Language picker (bottom-left), follows the system language unless overridden.
    ctl(hwnd, STATIC, t("lbl_language"), WINDOW_STYLE(0), 16, 474, 74, 20, ID_LBL_LANG, hinst);
    let combo = ctl(
        hwnd,
        COMBOBOX,
        "",
        WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP,
        94,
        470,
        210,
        260,
        ID_LANG,
        hinst,
    );
    fill_lang_combo(combo);
    dark_theme_combo(combo);

    // ===== Right column: supported file types =====
    let rx = 348;
    ctl(hwnd, STATIC, t("lbl_formats"), WINDOW_STYLE(0), rx, 12, 300, 18, ID_LBL_FORMATS, hinst);
    ctl(hwnd, BUTTON, t("btn_select_all"), WS_TABSTOP, rx, 34, 84, 26, ID_SELECT_ALL, hinst);
    ctl(hwnd, BUTTON, t("btn_clear_all"), WS_TABSTOP, rx + 90, 34, 84, 26, ID_CLEAR_ALL, hinst);
    ctl(hwnd, BUTTON, t("btn_defaults"), WS_TABSTOP, rx + 180, 34, 84, 26, ID_DEFAULTS, hinst);

    let list = ctl(
        hwnd,
        WC_LISTVIEWW,
        "",
        WINDOW_STYLE((LVS_REPORT | LVS_NOSORTHEADER) as u32) | WS_BORDER | WS_TABSTOP,
        rx,
        68,
        356,
        388,
        ID_LIST,
        hinst,
    );
    SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        Some(WPARAM(0)),
        Some(LPARAM((LVS_EX_CHECKBOXES | LVS_EX_FULLROWSELECT) as isize)),
    );
    if is_dark() {
        SendMessageW(list, LVM_SETBKCOLOR, None, Some(LPARAM(DARK_BG.0 as isize)));
        SendMessageW(list, LVM_SETTEXTBKCOLOR, None, Some(LPARAM(DARK_BG.0 as isize)));
        SendMessageW(list, LVM_SETTEXTCOLOR, None, Some(LPARAM(DARK_TEXT.0 as isize)));
        let header = HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut c_void);
        dark_control(header, w!("DarkMode_ItemsView"));
    }
    // Subclass for dark header text + SPACE/right-click bulk checkbox toggle.
    let _ = SetWindowSubclass(list, Some(list_subclass), 0, 0);
    // Extension | Category | Description. FORMATS is ordered by category, so the
    // list naturally clusters: Images, then Camera RAW, then Ebooks & comics —
    // and the Category column labels each (robust in dark mode, unlike native
    // ListView group headers, which the dark theme refuses to render).
    insert_column(list, 0, t("col_extension"), 64);
    insert_column(list, 1, t("col_category"), 92);
    insert_column(list, 2, t("col_description"), 196);

    for (i, &(ext, desc)) in formats::FORMATS.iter().enumerate() {
        let elabel = wide(&format!(".{ext}"));
        let mut item = LVITEMW {
            mask: LVIF_TEXT,
            iItem: i as i32,
            iSubItem: 0,
            pszText: PWSTR(elabel.as_ptr() as *mut u16),
            ..Default::default()
        };
        SendMessageW(list, LVM_INSERTITEMW, Some(WPARAM(0)), Some(LPARAM(&mut item as *mut _ as isize)));
        set_subitem(list, i as i32, 1, formats::category_label(formats::category(ext)));
        set_subitem(list, i as i32, 2, desc);
        set_check(list, i as i32, settings::format_enabled(ext));
    }

    // ===== Company promotion =====
    // Centered clickable banner (the product push), loaded from a remote URL at
    // runtime so it can change without a rebuild. SS_NOTIFY -> STN_CLICKED.
    let banner = ctl(hwnd, STATIC, "", WINDOW_STYLE((SS_BITMAP | SS_NOTIFY) as u32), 138, 516, 440, 56, ID_BANNER, hinst);
    if let Some(hbmp) = load_art(BANNER_PNG, "banner.png", 440, 56) {
        set_static_bitmap(banner, hbmp);
    }
    spawn_remote_ads(banner, BANNER_URL, 440, 56);

    // ===== Bottom row: About + credit (left), inline with Save / Cancel (right) =====
    ctl(hwnd, BUTTON, t("btn_about"), WS_TABSTOP, 16, 594, 96, 28, ID_ABOUT, hinst);
    let credit = format!("{} <a href=\"{URL_PARENT}\">Lunarwerx</a>", t("promo_made_by"));
    ctl(hwnd, SYSLINK, &credit, WS_TABSTOP, 122, 600, 240, 20, ID_PROMO_LINK, hinst);
    ctl(hwnd, BUTTON, t("btn_ok"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 520, 594, 92, 28, IDOK, hinst);
    ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 620, 594, 92, 28, IDCANCEL, hinst);

    set_window_title(hwnd);
    load_values(hwnd);
    add_tooltips(hwnd, hinst);
}

// Tooltip-window style bits (not surfaced by this windows-rs metadata).
const TTS_ALWAYSTIP: u32 = 0x01;
const TTS_NOPREFIX: u32 = 0x02;

/// Attach a hover hint to every interactive Settings control. One tooltip window
/// owns them all; `TTF_SUBCLASS` lets it relay its own mouse messages, so the
/// dialog's wndproc needs no extra handling. Hint text is localized with an
/// English fallback, so untranslated locales still get a hint. Labels stay plain
/// STATICs (no SS_NOTIFY = no mouse messages), so the hint rides the control they
/// describe — which is what a user actually hovers.
unsafe fn add_tooltips(hwnd: HWND, hinst: HINSTANCE) {
    let Ok(tip) = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("tooltips_class32"),
        PCWSTR::null(),
        WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
        0,
        0,
        0,
        0,
        Some(hwnd),
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };
    // Let long hints wrap (and honor explicit line breaks) instead of one wide line.
    SendMessageW(tip, TTM_SETMAXTIPWIDTH, Some(WPARAM(0)), Some(LPARAM(320)));

    let tips: &[(i32, &str)] = &[
        (ID_ENABLE_THUMBS, "tip_enable_thumbs"),
        (ID_USE_EMBEDDED, "tip_prefer_embedded"),
        (ID_ENABLE_MENU, "tip_enable_menu"),
        (ID_MENU_PREVIEW, "tip_menu_preview"),
        (ID_MENU_QUICK, "tip_menu_quick"),
        (ID_MAXSIZE, "tip_max_file"),
        (ID_SIZE, "tip_max_thumb"),
        (ID_JPEG, "tip_jpeg"),
        (ID_PNG, "tip_png"),
        (ID_C_SORT, "tip_sort"),
        (ID_C_PREFER_COVER, "tip_prefer_cover"),
        (ID_C_SKIP_SCAN, "tip_skip_scan"),
        (ID_LANG, "tip_lang"),
        (ID_SELECT_ALL, "tip_select_all"),
        (ID_CLEAR_ALL, "tip_clear_all"),
        (ID_DEFAULTS, "tip_defaults"),
        (ID_LIST, "tip_list"),
        (ID_ABOUT, "tip_about"),
        (ID_BANNER, "tip_banner"),
        (IDOK, "tip_save"),
        (IDCANCEL, "tip_cancel"),
    ];
    for &(id, key) in tips {
        let Ok(ctl) = GetDlgItem(Some(hwnd), id) else { continue };
        // comctl32 copies the text on TTM_ADDTOOL, so this buffer can be temporary.
        let text = wide(t(key));
        // The banner's hint rotates with the ad, so it pulls live text via a
        // TTN_GETDISPINFO callback (handled in WM_NOTIFY) instead of fixed text.
        let lpsz = if id == ID_BANNER {
            PWSTR((-1isize) as *mut u16) // LPSTR_TEXTCALLBACKW
        } else {
            PWSTR(text.as_ptr() as *mut u16)
        };
        let mut ti = TTTOOLINFOW {
            cbSize: core::mem::size_of::<TTTOOLINFOW>() as u32,
            uFlags: TTF_IDISHWND | TTF_SUBCLASS,
            hwnd,
            uId: ctl.0 as usize,
            lpszText: lpsz,
            ..Default::default()
        };
        SendMessageW(tip, TTM_ADDTOOLW, Some(WPARAM(0)), Some(LPARAM(&mut ti as *mut _ as isize)));
    }
}

/// Insert one ListView report column.
unsafe fn insert_column(list: HWND, idx: i32, title: &str, cx: i32) {
    let t = wide(title);
    let mut col = LVCOLUMNW {
        mask: LVCF_FMT | LVCF_WIDTH | LVCF_TEXT,
        fmt: LVCFMT_LEFT,
        cx,
        pszText: PWSTR(t.as_ptr() as *mut u16),
        ..Default::default()
    };
    SendMessageW(list, LVM_INSERTCOLUMNW, Some(WPARAM(idx as usize)), Some(LPARAM(&mut col as *mut _ as isize)));
}

/// Set a ListView subitem's text (Category / Description columns).
unsafe fn set_subitem(list: HWND, row: i32, col: i32, text: &str) {
    let w = wide(text);
    let sub = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row,
        iSubItem: col,
        pszText: PWSTR(w.as_ptr() as *mut u16),
        ..Default::default()
    };
    SendMessageW(list, LVM_SETITEMW, Some(WPARAM(0)), Some(LPARAM(&sub as *const _ as isize)));
}

// ---- Localization helpers ----------------------------------------------

/// All shipped language codes (English first).
fn lang_codes() -> Vec<&'static str> {
    i18n::codes().collect()
}

/// Fill the language combo: item 0 = "follow system", then each language by its
/// native name. Selects the current override (or "system" if none).
unsafe fn fill_lang_combo(combo: HWND) {
    add_combo_string(combo, t("lang_system"));
    let current = settings::lang_override();
    let mut sel = 0i32;
    for (i, code) in lang_codes().iter().enumerate() {
        add_combo_string(combo, i18n::native_name(code));
        if current.as_deref() == Some(*code) {
            sel = (i + 1) as i32;
        }
    }
    SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
}

unsafe fn add_combo_string(combo: HWND, s: &str) {
    let w = wide(s);
    SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
}

/// The language code selected in the combo, or None for "follow system".
unsafe fn selected_lang(hwnd: HWND) -> Option<&'static str> {
    let combo = GetDlgItem(Some(hwnd), ID_LANG).ok()?;
    let sel = SendMessageW(combo, CB_GETCURSEL, None, None).0;
    if sel <= 0 {
        None
    } else {
        lang_codes().get((sel - 1) as usize).copied()
    }
}

/// Live language preview: re-resolve the locale and re-label every control
/// (without persisting — persistence happens on OK).
unsafe fn on_lang_change(hwnd: HWND) {
    i18n::apply_override_or_system(selected_lang(hwnd));
    apply_labels(hwnd);
}

/// Re-apply every translatable label in the active language (used after a live
/// language change). Edits/selections are preserved (we only set text).
unsafe fn apply_labels(hwnd: HWND) {
    set_window_title(hwnd);
    let pairs: &[(i32, &str)] = &[
        (ID_LBL_THUMBS, "grp_thumbnails"),
        (ID_ENABLE_THUMBS, "chk_enable_thumbs"),
        (ID_USE_EMBEDDED, "chk_prefer_embedded"),
        (ID_ENABLE_MENU, "chk_enable_menu"),
        (ID_LBL_PREVIEW, "lbl_menu_preview"),
        (ID_MENU_QUICK, "chk_menu_quick"),
        (ID_LBL_LIMITS, "grp_limits"),
        (ID_LBL_MAXFILE, "lbl_max_file"),
        (ID_LBL_MAXTHUMB, "lbl_max_thumb"),
        (ID_LBL_JPEG, "lbl_jpeg"),
        (ID_LBL_PNG, "lbl_png"),
        (ID_LBL_EBOOK, "grp_ebook"),
        (ID_C_SORT, "chk_sort"),
        (ID_C_PREFER_COVER, "chk_prefer_cover"),
        (ID_C_SKIP_SCAN, "chk_skip_scanlation"),
        (ID_LBL_FORMATS, "lbl_formats"),
        (ID_SELECT_ALL, "btn_select_all"),
        (ID_CLEAR_ALL, "btn_clear_all"),
        (ID_DEFAULTS, "btn_defaults"),
        (ID_LBL_LANG, "lbl_language"),
        (IDOK, "btn_ok"),
        (IDCANCEL, "btn_cancel"),
    ];
    for &(id, key) in pairs {
        set_dlg_text(hwnd, id, t(key));
    }
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        set_column_text(list, 0, t("col_extension"));
        set_column_text(list, 1, t("col_description"));
    }
    // The preview-placement combo holds translated items: rebuild, keep selection.
    if let Ok(prev) = GetDlgItem(Some(hwnd), ID_MENU_PREVIEW) {
        let sel = SendMessageW(prev, CB_GETCURSEL, None, None).0.max(0);
        SendMessageW(prev, CB_RESETCONTENT, None, None);
        for key in ["prev_off", "prev_submenu", "prev_main"] {
            let w = wide(t(key));
            SendMessageW(prev, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
        }
        SendMessageW(prev, CB_SETCURSEL, Some(WPARAM(sel as usize)), None);
    }
}

unsafe fn set_dlg_text(hwnd: HWND, id: i32, s: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        let w = wide(s);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

unsafe fn set_window_title(hwnd: HWND) {
    let title = format!("SageThumbs 2K \u{2014} {}", t("lbl_options"));
    let w = wide(&title);
    let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
}

unsafe fn set_column_text(list: HWND, idx: i32, s: &str) {
    let w = wide(s);
    let mut col = LVCOLUMNW {
        mask: LVCF_TEXT,
        pszText: PWSTR(w.as_ptr() as *mut u16),
        ..Default::default()
    };
    SendMessageW(list, LVM_SETCOLUMNW, Some(WPARAM(idx as usize)), Some(LPARAM(&mut col as *mut _ as isize)));
}

/// Populate every control from the persisted settings.
unsafe fn load_values(hwnd: HWND) {
    check(hwnd, ID_ENABLE_THUMBS, settings::thumbnails_enabled());
    check(hwnd, ID_USE_EMBEDDED, settings::use_embedded());
    check(hwnd, ID_ENABLE_MENU, settings::menu_enabled());
    let mb = (settings::max_file_size_bytes() / (1024 * 1024)).min(u32::MAX as u64) as u32;
    let _ = SetDlgItemInt(hwnd, ID_MAXSIZE, mb, false);
    let _ = SetDlgItemInt(hwnd, ID_SIZE, settings::max_thumb_size(), false);
    let _ = SetDlgItemInt(hwnd, ID_JPEG, settings::jpeg_quality() as u32, false);
    let _ = SetDlgItemInt(hwnd, ID_PNG, settings::png_level(), false);
    check(hwnd, ID_C_SORT, settings::container_sort());
    check(hwnd, ID_C_PREFER_COVER, settings::container_prefer_cover());
    check(hwnd, ID_C_SKIP_SCAN, settings::container_skip_scanlation());
    check(hwnd, ID_MENU_QUICK, settings::menu_quick_verbs());
}

/// Reset every control to the factory defaults (does not write yet).
unsafe fn load_defaults(hwnd: HWND) {
    check(hwnd, ID_ENABLE_THUMBS, true);
    check(hwnd, ID_USE_EMBEDDED, false);
    check(hwnd, ID_ENABLE_MENU, true);
    let _ = SetDlgItemInt(hwnd, ID_MAXSIZE, settings::DEFAULT_MAX_FILE_MB, false);
    let _ = SetDlgItemInt(hwnd, ID_SIZE, settings::DEFAULT_THUMB_SIZE, false);
    let _ = SetDlgItemInt(hwnd, ID_JPEG, settings::DEFAULT_JPEG, false);
    let _ = SetDlgItemInt(hwnd, ID_PNG, settings::DEFAULT_PNG, false);
    check(hwnd, ID_C_SORT, true);
    check(hwnd, ID_C_PREFER_COVER, true);
    check(hwnd, ID_C_SKIP_SCAN, false);
    check(hwnd, ID_MENU_QUICK, false);
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        for i in 0..formats::FORMATS.len() as i32 {
            set_check(list, i, true);
        }
    }
}

/// Persist all settings; re-register formats (elevated) if the list changed.
/// Persist all settings (and re-register formats if the list changed). Apply-only
/// — does NOT close the window, so the user can save and keep tweaking.
unsafe fn apply_settings(hwnd: HWND) {
    let _ = settings::set_dword("EnableThumbs", checked(hwnd, ID_ENABLE_THUMBS) as u32);
    let _ = settings::set_dword("UseEmbedded", checked(hwnd, ID_USE_EMBEDDED) as u32);
    let _ = settings::set_dword("EnableMenu", checked(hwnd, ID_ENABLE_MENU) as u32);
    let _ = settings::set_dword("MenuQuickVerbs", checked(hwnd, ID_MENU_QUICK) as u32);
    if let Ok(prev) = GetDlgItem(Some(hwnd), ID_MENU_PREVIEW) {
        let sel = SendMessageW(prev, CB_GETCURSEL, None, None).0.clamp(0, 2);
        let _ = settings::set_dword("MenuPreview", sel as u32);
    }
    let _ = settings::set_dword("ContainerSort", checked(hwnd, ID_C_SORT) as u32);
    let _ = settings::set_dword("ContainerPreferCover", checked(hwnd, ID_C_PREFER_COVER) as u32);
    let _ = settings::set_dword("ContainerSkipScanlation", checked(hwnd, ID_C_SKIP_SCAN) as u32);

    let mut ok = Default::default();
    let max_mb = GetDlgItemInt(hwnd, ID_MAXSIZE, Some(&mut ok), false);
    let _ = settings::set_dword("MaxSize", if ok.as_bool() { max_mb } else { settings::DEFAULT_MAX_FILE_MB });

    let size = GetDlgItemInt(hwnd, ID_SIZE, Some(&mut ok), false);
    let size = if ok.as_bool() {
        size.clamp(settings::THUMB_MIN, settings::THUMB_MAX)
    } else {
        settings::DEFAULT_THUMB_SIZE
    };
    let _ = settings::set_dword("Width", size);
    let _ = settings::set_dword("Height", size);

    let jpeg = GetDlgItemInt(hwnd, ID_JPEG, Some(&mut ok), false).min(100);
    let _ = settings::set_dword("JPEG", if ok.as_bool() { jpeg } else { settings::DEFAULT_JPEG });
    let png = GetDlgItemInt(hwnd, ID_PNG, Some(&mut ok), false).min(9);
    let _ = settings::set_dword("PNG", if ok.as_bool() { png } else { settings::DEFAULT_PNG });

    // Persist the UI-language choice ("" = follow the system language).
    let _ = settings::set_lang(selected_lang(hwnd).unwrap_or(""));

    // Per-format flags. Collect the changes first; persist them, then run the
    // elevated re-register that rewrites the HKCR shell hooks to match. If that
    // elevation is declined or fails, roll the HKCU flags back so the persisted
    // settings stay consistent with the (unchanged) hooks — otherwise the two
    // silently diverge and, because change-detection reads HKCU, never reconcile.
    let mut changes: Vec<(&'static str, bool, bool)> = Vec::new();
    if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
        for (i, &(ext, _)) in formats::FORMATS.iter().enumerate() {
            let want = is_checked(list, i as i32);
            let old = settings::format_enabled(ext);
            if old != want {
                changes.push((ext, want, old));
            }
        }
    }
    if !changes.is_empty() {
        for &(ext, want, _) in &changes {
            let _ = settings::set_format_enabled(ext, want);
        }
        if !reregister_elevated() {
            for &(ext, _, old) in &changes {
                let _ = settings::set_format_enabled(ext, old);
            }
            message_box(hwnd, t("msg_admin_required"), "SageThumbs 2K");
        }
    }
}

/// Show a simple warning message box owned by the dialog.
unsafe fn message_box(hwnd: HWND, text: &str, caption: &str) {
    let t = wide(text);
    let c = wide(caption);
    MessageBoxW(Some(hwnd), PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | MB_ICONWARNING);
}

/// Re-run `regsvr32` elevated against the installed DLL. `register()` reads the
/// per-extension flags we just wrote, so this brings the HKCR `shellex` keys in
/// line with the Options format list. On an admin account with the silent-
/// elevation policy this raises no prompt.
unsafe fn reregister_elevated() -> bool {
    let dll = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("sagethumbs2k.dll")))
        .unwrap_or_default();
    let params = wide(&format!("/s \"{}\"", dll.display()));
    let verb = wide("runas");
    let file = wide("regsvr32.exe");
    let h = ShellExecuteW(
        Some(HWND::default()),
        PCWSTR(verb.as_ptr()),
        PCWSTR(file.as_ptr()),
        PCWSTR(params.as_ptr()),
        PCWSTR::null(),
        SW_HIDE,
    );
    // ShellExecuteW returns a value > 32 on success; <= 32 means it failed to
    // launch (notably SE_ERR_ACCESSDENIED when the user declines the UAC prompt).
    (h.0 as usize) > 32
}

// ============================== About box ===============================
// A small popup: logo, version, the company links (SysLink), a tagline. The
// richer home for the promotion; the main dialog keeps just the footer link.

/// Open the About box, owned by `parent`.
unsafe fn show_about(parent: HWND) {
    let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
    let class = w!("SageThumbs2KAbout");
    // Idempotent: a second RegisterClassW returns 0 (already registered) — fine.
    let wc = WNDCLASSW {
        lpfnWndProc: Some(about_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if is_dark() { dark_bg_brush() } else { HBRUSH(16isize as *mut c_void) },
        ..Default::default()
    };
    RegisterClassW(&wc);

    if let Ok(hwnd) = CreateWindowExW(
        WS_EX_DLGMODALFRAME,
        class,
        w!("About SageThumbs 2K"),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        400,
        422,
        Some(parent),
        None,
        Some(hinst),
        None,
    ) {
        if is_dark() {
            dark_control(hwnd, w!("DarkMode_Explorer"));
            dark_titlebar(hwnd);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
}

/// Pixel width of `s` rendered in the GUI font (for centering controls).
unsafe fn text_width(s: &str) -> i32 {
    let hdc = GetDC(None);
    let old = SelectObject(hdc, HGDIOBJ(gui_font().0));
    let w = wide(s);
    let n = w.len().saturating_sub(1);
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w[..n], &mut sz);
    SelectObject(hdc, old);
    ReleaseDC(None, hdc);
    sz.cx
}

/// The LunarWerx wordmark sized to `w`×`h`. The art is white-on-transparent, so
/// in LIGHT mode it's composited onto a dark chip first (it would otherwise be
/// invisible on the pale dialog); in dark mode the transparency is kept.
unsafe fn lw_logo_hbitmap(w: u32, h: u32) -> Option<HBITMAP> {
    let logo = image::load_from_memory(LW_LOGO_PNG)
        .ok()?
        .resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let rgba = if is_dark() {
        logo
    } else {
        let mut chip = image::RgbaImage::from_pixel(w, h, image::Rgba([43, 43, 43, 255]));
        image::imageops::overlay(&mut chip, &logo, 0, 0);
        chip
    };
    sagethumbs2k::rgba_to_hbitmap(w, h, rgba.as_raw()).map(|h| HBITMAP(h as *mut c_void))
}

unsafe fn build_about(hwnd: HWND, hinst: HINSTANCE) {
    let logo = ctl(hwnd, STATIC, "", WINDOW_STYLE(SS_BITMAP as u32), 164, 18, 72, 72, -1, hinst);
    if let Some(hbmp) = load_art(LOGO_PNG, "logo.png", 72, 72) {
        set_static_bitmap(logo, hbmp);
    }
    let center = WINDOW_STYLE(SS_CENTER as u32);
    ctl(hwnd, STATIC, "SageThumbs 2K", center, 20, 100, 360, 22, -1, hinst);
    let ver = format!("{} {}", t("about_version"), env!("CARGO_PKG_VERSION"));
    ctl(hwnd, STATIC, &ver, center, 20, 124, 360, 18, -1, hinst);
    ctl(hwnd, STATIC, t("about_desc"), center, 20, 150, 360, 18, -1, hinst);
    // Center the repo link: measure the visible text and place the SysLink rect so
    // it sits in the middle (SysLink left-aligns its text within its own rect).
    let visible = "github.com/LunarWerxs/SageThumbs-2k";
    let tw = text_width(visible);
    let lx = ((400 - tw) / 2).max(8);
    let link = format!("<a href=\"{URL_GITHUB}\">{visible}</a>");
    ctl(hwnd, SYSLINK, &link, WINDOW_STYLE(0), lx, 184, tw + 8, 20, ID_ABOUT_LINK, hinst);
    ctl(hwnd, STATIC, t("about_tagline"), center, 20, 216, 360, 34, -1, hinst);
    // The LunarWerx wordmark, below the tagline and above Close; clicking it
    // opens the companies page (SS_NOTIFY → STN_CLICKED; hand cursor in wndproc).
    // Tuned size/spacing: 231×38 (25% down from 308×50), 30px above Close.
    let lw = ctl(hwnd, STATIC, "", WINDOW_STYLE((SS_BITMAP | SS_NOTIFY) as u32), 84, 258, 231, 38, ID_LW_LOGO, hinst);
    if let Some(hbmp) = lw_logo_hbitmap(231, 38) {
        set_static_bitmap(lw, hbmp);
    }
    ctl(hwnd, BUTTON, t("btn_close"), WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 158, 326, 84, 28, IDOK, hinst);
}

extern "system" fn about_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                build_about(hwnd, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                if id == IDOK || id == IDCANCEL {
                    let _ = DestroyWindow(hwnd);
                } else if id == ID_LW_LOGO {
                    open_url(URL_COMPANIES); // the wordmark is a link (STN_CLICKED)
                }
                LRESULT(0)
            }
            WM_SETCURSOR => {
                // Hand cursor over the clickable wordmark; everything else default.
                let over = HWND(wparam.0 as *mut c_void);
                if GetDlgItem(Some(hwnd), ID_LW_LOGO).map(|h| h == over).unwrap_or(false) {
                    if let Ok(hand) = LoadCursorW(None, IDC_HAND) {
                        SetCursor(Some(hand));
                    }
                    return LRESULT(1);
                }
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const NMHDR;
                if (*nmhdr).code == NM_CLICK || (*nmhdr).code == NM_RETURN {
                    let link = lparam.0 as *const NMLINK;
                    let url = wstr_to_string(&(*link).item.szUrl);
                    if !url.is_empty() {
                        open_url(&url);
                    }
                }
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_BG);
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(dark_bg_brush().0 as isize)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ============================ Convert… dialog ============================
// A batch image converter (format / quality / resize / output folder), shown by
// the EXE when launched as `--convert <listfile>` from the DLL's menu verb.

const CID_FORMAT: i32 = 3001;
const CID_RESIZE: i32 = 3004;
const CID_OUTDIR: i32 = 3005;
const CID_BROWSE: i32 = 3006;
const CID_PROGRESS: i32 = 3007;
const CID_SETTINGS: i32 = 3008;
const CID_RESIZE_CHK: i32 = 3009;
const CID_RESIZE_W: i32 = 3010;
const CID_RESIZE_H: i32 = 3011;
const WM_CONVERT_PROGRESS: u32 = 0x8000 + 30; // WM_APP + 30
const WM_CONVERT_DONE: u32 = 0x8000 + 31;

static CONVERT_FILES: OnceLock<Vec<String>> = OnceLock::new();
/// Per-format encode settings, chosen in the Settings… popup, read by the worker.
static QUALITY: AtomicI32 = AtomicI32::new(90); // JPEG quality 1..=100
static WEBP_QUALITY: AtomicI32 = AtomicI32::new(80); // lossy WebP quality 1..=100
static WEBP_LOSSLESS: AtomicI32 = AtomicI32::new(1); // 1 = lossless (default), 0 = lossy
static PNG_LEVEL: AtomicI32 = AtomicI32::new(6); // PNG compression 0..=9

/// (display name, `Some(format)` or `None` for PDF, output extension). The
/// image-crate encoders are all behind features the crate already enables.
const CV_FORMATS: &[(&str, Option<ImageFormat>, &str)] = &[
    ("JPG  \u{2014}  JPEG / JFIF", Some(ImageFormat::Jpeg), "jpg"),
    ("PNG  \u{2014}  Portable Network Graphics", Some(ImageFormat::Png), "png"),
    ("WEBP  \u{2014}  WebP (lossless)", Some(ImageFormat::WebP), "webp"),
    ("BMP  \u{2014}  Windows Bitmap", Some(ImageFormat::Bmp), "bmp"),
    ("GIF  \u{2014}  CompuServe GIF", Some(ImageFormat::Gif), "gif"),
    ("TIFF  \u{2014}  Revision 6", Some(ImageFormat::Tiff), "tiff"),
    ("ICO  \u{2014}  Windows Icon", Some(ImageFormat::Ico), "ico"),
    ("TGA  \u{2014}  Truevision Targa", Some(ImageFormat::Tga), "tga"),
    ("QOI  \u{2014}  Quite OK Image", Some(ImageFormat::Qoi), "qoi"),
    ("PNM  \u{2014}  Portable Pixmap (PPM)", Some(ImageFormat::Pnm), "ppm"),
    ("PDF  \u{2014}  Portable Document Format", None, "pdf"),
];

/// Extra Convert targets the `image` crate can't encode — written via the bundled
/// ImageMagick (hidden on a compact install). Our decode pipeline handles the
/// input; magick only writes the exotic output. (display name, extension)
const CV_MAGICK_FORMATS: &[(&str, &str)] = &[
    ("PSD  \u{2014}  Adobe Photoshop", "psd"),
    ("DDS  \u{2014}  DirectDraw Surface", "dds"),
    ("JP2  \u{2014}  JPEG 2000", "jp2"),
    ("PCX  \u{2014}  PC Paintbrush", "pcx"),
    ("SGI  \u{2014}  Silicon Graphics", "sgi"),
    ("EXR  \u{2014}  OpenEXR (HDR)", "exr"),
    ("HDR  \u{2014}  Radiance RGBE (HDR)", "hdr"),
    ("FF  \u{2014}  Farbfeld", "ff"),
    ("PAM  \u{2014}  Portable Arbitrary Map", "pam"),
    ("PFM  \u{2014}  Portable Float Map", "pfm"),
    ("DPX  \u{2014}  Digital Picture Exchange", "dpx"),
    ("FITS  \u{2014}  Flexible Image Transport", "fits"),
    ("XPM  \u{2014}  X11 Pixmap", "xpm"),
    ("PICT  \u{2014}  Apple PICT", "pict"),
    ("RAS  \u{2014}  Sun Raster", "ras"),
    ("PALM  \u{2014}  Palm Pixmap", "palm"),
];

/// The resolved Convert target the worker thread acts on.
#[derive(Clone, Copy)]
enum CvTarget {
    Native(ImageFormat, &'static str),
    Pdf,
    Magick(&'static str),
}

/// Map the format combo's selection index to a target. Magick entries sit after
/// the native ones (and only exist when magick is available), so an index past
/// `CV_FORMATS` is a magick target.
fn resolve_cv_target(sel: usize) -> CvTarget {
    if sel < CV_FORMATS.len() {
        let (_, fmt, ext) = CV_FORMATS[sel];
        match fmt {
            Some(f) => CvTarget::Native(f, ext),
            None => CvTarget::Pdf,
        }
    } else {
        match CV_MAGICK_FORMATS.get(sel - CV_FORMATS.len()) {
            Some((_, ext)) => CvTarget::Magick(ext),
            None => CvTarget::Native(ImageFormat::Png, "png"),
        }
    }
}

/// Resize modes in the dialog dropdown. `Defined` reads the W×H edit fields.
#[derive(Clone, Copy)]
enum ResizeMode {
    Defined,
    Fit(u32, u32),
    Pct(u32),
}
const CV_RESIZE: &[(&str, ResizeMode)] = &[
    ("Defined size", ResizeMode::Defined),
    ("Fit 1920 \u{00d7} 1080", ResizeMode::Fit(1920, 1080)),
    ("Fit 1280 \u{00d7} 720", ResizeMode::Fit(1280, 720)),
    ("Fit 800 \u{00d7} 600", ResizeMode::Fit(800, 600)),
    ("Scale 50%", ResizeMode::Pct(50)),
    ("Scale 25%", ResizeMode::Pct(25)),
];

const fn make_lparam(low: i32, high: i32) -> isize {
    ((low & 0xFFFF) | (high << 16)) as isize
}

unsafe fn run_convert_dialog(hinst: HINSTANCE, listfile: &str) {
    let files: Vec<String> = std::fs::read_to_string(listfile)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let _ = std::fs::remove_file(listfile);
    if files.is_empty() {
        return;
    }
    let n = files.len();
    let _ = CONVERT_FILES.set(files);

    let class = w!("SageThumbs2KConvert");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(convert_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if is_dark() { dark_bg_brush() } else { HBRUSH(16isize as *mut c_void) },
        ..Default::default()
    };
    RegisterClassW(&wc);

    let title = wide(&format!("Convert {n} image(s) \u{2014} SageThumbs 2K"));
    if let Ok(hwnd) = CreateWindowExW(
        WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
        class,
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        500,
        274,
        None,
        None,
        Some(hinst),
        None,
    ) {
        if is_dark() {
            dark_control(hwnd, w!("DarkMode_Explorer"));
            dark_titlebar(hwnd);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

unsafe fn build_convert_controls(hwnd: HWND, hinst: HINSTANCE) {
    let lbl = WINDOW_STYLE(0);

    // Row 1 — output format + per-format Settings…
    ctl(hwnd, STATIC, "Output format:", lbl, 16, 23, 92, 18, -1, hinst);
    let fcombo = ctl(hwnd, COMBOBOX, "", WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP, 110, 20, 252, 360, CID_FORMAT, hinst);
    for (name, _, _) in CV_FORMATS {
        let w = wide(name);
        SendMessageW(fcombo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    // Magick-backed exotic targets, only when ImageMagick is present (full install).
    if sagethumbs2k::magick_available() {
        for (name, _) in CV_MAGICK_FORMATS {
            let w = wide(name);
            SendMessageW(fcombo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
        }
    }
    SendMessageW(fcombo, CB_SETCURSEL, Some(WPARAM(0)), None); // JPG
    dark_theme_combo(fcombo);
    ctl(hwnd, BUTTON, "Settings\u{2026}", WS_TABSTOP, 372, 19, 96, 26, CID_SETTINGS, hinst);

    // Row 2 — resize on/off + mode
    ctl(hwnd, BUTTON, "Resize", WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP, 16, 58, 90, 20, CID_RESIZE_CHK, hinst);
    let rcombo = ctl(hwnd, COMBOBOX, "", WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_VSCROLL | WS_TABSTOP, 110, 56, 180, 240, CID_RESIZE, hinst);
    for (name, _) in CV_RESIZE {
        let w = wide(name);
        SendMessageW(rcombo, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
    SendMessageW(rcombo, CB_SETCURSEL, Some(WPARAM(0)), None);
    dark_theme_combo(rcombo);

    // Row 3 — custom W × H (only used when Resize is on + mode is "Defined size")
    ctl(hwnd, EDIT, "1280", WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 110, 88, 64, 24, CID_RESIZE_W, hinst);
    ctl(hwnd, STATIC, "\u{00d7}", WINDOW_STYLE(SS_CENTER), 178, 91, 16, 18, -1, hinst);
    ctl(hwnd, EDIT, "720", WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 198, 88, 64, 24, CID_RESIZE_H, hinst);
    ctl(hwnd, STATIC, "px", lbl, 268, 91, 24, 18, -1, hinst);

    // Row 4 — output folder
    ctl(hwnd, STATIC, "Output folder:", lbl, 16, 131, 92, 18, -1, hinst);
    ctl(hwnd, EDIT, "", WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 110, 128, 292, 24, CID_OUTDIR, hinst);
    set_edit_text(hwnd, CID_OUTDIR, "(same folder as each image)");
    ctl(hwnd, BUTTON, "\u{2026}", WS_TABSTOP, 408, 127, 60, 26, CID_BROWSE, hinst);

    // Progress bar stays hidden until a conversion is actually running.
    let prog = ctl(hwnd, w!("msctls_progress32"), "", WINDOW_STYLE(0), 16, 172, 452, 14, CID_PROGRESS, hinst);
    let _ = ShowWindow(prog, SW_HIDE);

    ctl(hwnd, BUTTON, "Convert", WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 280, 202, 88, 28, IDOK, hinst);
    ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 380, 202, 88, 28, IDCANCEL, hinst);

    update_resize_enabled(hwnd);
    update_settings_enabled(hwnd);
}

/// "Settings…" is enabled only for formats that have a settings panel (JPG/PDF
/// quality, WebP lossless+quality, PNG compression).
unsafe fn update_settings_enabled(hwnd: HWND) {
    let has = settings_kind(combo_sel(hwnd, CID_FORMAT)) != SK_NONE;
    if let Ok(b) = GetDlgItem(Some(hwnd), CID_SETTINGS) {
        let _ = EnableWindow(b, has);
    }
}

/// Enable the resize controls only when the checkbox is on; the W×H edits only
/// when the mode is "Defined size".
unsafe fn update_resize_enabled(hwnd: HWND) {
    let on = checked(hwnd, CID_RESIZE_CHK);
    if let Ok(c) = GetDlgItem(Some(hwnd), CID_RESIZE) {
        let _ = EnableWindow(c, on);
    }
    let defined = matches!(
        CV_RESIZE.get(combo_sel(hwnd, CID_RESIZE)).map(|r| r.1),
        Some(ResizeMode::Defined)
    );
    for id in [CID_RESIZE_W, CID_RESIZE_H] {
        if let Ok(e) = GetDlgItem(Some(hwnd), id) {
            let _ = EnableWindow(e, on && defined);
        }
    }
}

/// The verbs-crate `Resize` selected in the dialog (None when unchecked).
unsafe fn read_resize(hwnd: HWND) -> Resize {
    if !checked(hwnd, CID_RESIZE_CHK) {
        return Resize::None;
    }
    match CV_RESIZE.get(combo_sel(hwnd, CID_RESIZE)).map(|r| r.1) {
        Some(ResizeMode::Fit(w, h)) => Resize::Fit(w, h),
        Some(ResizeMode::Pct(p)) => Resize::Percent(p),
        _ => {
            let w = get_edit_text(hwnd, CID_RESIZE_W).trim().parse::<u32>().unwrap_or(0);
            let h = get_edit_text(hwnd, CID_RESIZE_H).trim().parse::<u32>().unwrap_or(0);
            if w > 0 && h > 0 {
                // Explicitly typed dimensions scale UP too — "make it bigger"
                // must make it bigger. The presets above stay shrink-only.
                Resize::FitUp(w, h)
            } else {
                Resize::None
            }
        }
    }
}

unsafe fn combo_sel(hwnd: HWND, id: i32) -> usize {
    GetDlgItem(Some(hwnd), id)
        .map(|c| SendMessageW(c, CB_GETCURSEL, None, None).0.max(0) as usize)
        .unwrap_or(0)
}

unsafe fn set_edit_text(hwnd: HWND, id: i32, text: &str) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        let w = wide(text);
        let _ = SetWindowTextW(h, PCWSTR(w.as_ptr()));
    }
}

unsafe fn get_edit_text(hwnd: HWND, id: i32) -> String {
    let Ok(h) = GetDlgItem(Some(hwnd), id) else {
        return String::new();
    };
    let n = GetWindowTextLengthW(h);
    if n <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; n as usize + 1];
    let got = GetWindowTextW(h, &mut buf) as usize;
    String::from_utf16_lossy(&buf[..got])
}

/// Read the dialog options and run the batch conversion on a worker thread,
/// posting progress back to the window.
unsafe fn start_convert(hwnd: HWND) {
    let files = match CONVERT_FILES.get() {
        Some(f) => f.clone(),
        None => return,
    };
    if files.is_empty() {
        return;
    }
    let tgt = resolve_cv_target(combo_sel(hwnd, CID_FORMAT));
    let quality = QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8;
    let png_level = PNG_LEVEL.load(Ordering::Relaxed).clamp(0, 9) as u32;
    let webp_quality = if matches!(tgt, CvTarget::Native(ImageFormat::WebP, _)) && WEBP_LOSSLESS.load(Ordering::Relaxed) == 0 {
        Some(WEBP_QUALITY.load(Ordering::Relaxed).clamp(1, 100) as u8)
    } else {
        None
    };
    let resize = read_resize(hwnd);
    let outdir_text = get_edit_text(hwnd, CID_OUTDIR);
    let outdir = (!outdir_text.starts_with('(') && !outdir_text.is_empty())
        .then(|| std::path::PathBuf::from(&outdir_text));

    if let Ok(prog) = GetDlgItem(Some(hwnd), CID_PROGRESS) {
        let _ = ShowWindow(prog, SW_SHOW);
        SendMessageW(prog, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(files.len() as isize)));
        SendMessageW(prog, PBM_SETPOS, Some(WPARAM(0)), None);
    }
    if let Ok(btn) = GetDlgItem(Some(hwnd), IDOK) {
        let _ = EnableWindow(btn, false);
    }

    let raw = hwnd.0 as usize;
    std::thread::spawn(move || {
        let total = files.len();
        let mut ok = 0usize;
        for (i, f) in files.iter().enumerate() {
            let Some(dir) = outdir
                .clone()
                .or_else(|| std::path::Path::new(f).parent().map(|p| p.to_path_buf()))
            else {
                continue;
            };
            // A unique output path in `dir` with extension `e`.
            let out_path = |e: &str| {
                let stem = std::path::Path::new(f).file_stem().and_then(|s| s.to_str()).unwrap_or("image");
                let mut out = dir.join(format!("{stem}.{e}"));
                let mut n = 1u32;
                while out.exists() {
                    out = dir.join(format!("{stem} ({n}).{e}"));
                    n += 1;
                }
                out
            };
            let done = match tgt {
                CvTarget::Native(format, ext) => {
                    let opts = ConvertOpts {
                        target: Target { format, ext },
                        jpeg_quality: quality,
                        png_level,
                        webp_quality,
                        resize,
                    };
                    convert_file_opts(f, opts, &dir).is_ok()
                }
                CvTarget::Pdf => {
                    // One image → one single-page PDF.
                    sagethumbs2k::combine_to_pdf(std::slice::from_ref(f), &out_path("pdf"), quality).is_ok()
                }
                CvTarget::Magick(ext) => {
                    // Exotic target written by the bundled ImageMagick.
                    sagethumbs2k::convert_to_magick(f, &out_path(ext), resize).is_ok()
                }
            };
            if done {
                ok += 1;
            }
            let _ = PostMessageW(Some(HWND(raw as *mut c_void)), WM_CONVERT_PROGRESS, WPARAM(i + 1), LPARAM(0));
        }
        let _ = PostMessageW(Some(HWND(raw as *mut c_void)), WM_CONVERT_DONE, WPARAM(ok), LPARAM(total as isize));
    });
}

/// Folder picker via IFileOpenDialog (FOS_PICKFOLDERS).
unsafe fn pick_folder(owner: HWND) -> Option<String> {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
    let opts = dlg.GetOptions().ok()?;
    dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM).ok()?;
    dlg.Show(Some(owner)).ok()?;
    let item: IShellItem = dlg.GetResult().ok()?;
    let pw = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = pw.to_string().ok();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

const CID_POPUP_TB: i32 = 4001;
const CID_POPUP_VAL: i32 = 4002;
const CID_POPUP_LOSSLESS: i32 = 4003;

const SK_NONE: i32 = 0;
const SK_JPEG: i32 = 1;
const SK_WEBP: i32 = 2;
const SK_PNG: i32 = 3;
/// Which settings panel the popup should show (set before opening).
static POPUP_KIND: AtomicI32 = AtomicI32::new(SK_JPEG);

/// The settings panel a format index needs (JPEG/PDF → quality, WebP →
/// lossless+quality, PNG → compression, others → none).
fn settings_kind(idx: usize) -> i32 {
    match CV_FORMATS.get(idx) {
        Some((_, Some(ImageFormat::Jpeg), _)) | Some((_, None, _)) => SK_JPEG,
        Some((_, Some(ImageFormat::WebP), _)) => SK_WEBP,
        Some((_, Some(ImageFormat::Png), _)) => SK_PNG,
        _ => SK_NONE,
    }
}

/// Modal per-format "Settings…" popup; stores into the format's static.
unsafe fn run_format_settings(owner: HWND, hinst: HINSTANCE, idx: usize) {
    let kind = settings_kind(idx);
    if kind == SK_NONE {
        return;
    }
    POPUP_KIND.store(kind, Ordering::Relaxed);

    let class = w!("SageThumbs2KSettings");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(settings_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if is_dark() { dark_bg_brush() } else { HBRUSH(16isize as *mut c_void) },
        ..Default::default()
    };
    RegisterClassW(&wc);

    let (pw, ph) = (300, if kind == SK_WEBP { 202 } else { 172 });
    let mut orc = RECT::default();
    let _ = GetWindowRect(owner, &mut orc);
    let px = orc.left + ((orc.right - orc.left) - pw) / 2;
    let py = orc.top + ((orc.bottom - orc.top) - ph) / 2;
    let title = wide(match kind {
        SK_WEBP => "WebP settings",
        SK_PNG => "PNG settings",
        _ => "JPEG settings",
    });
    let Ok(pop) = CreateWindowExW(
        WS_EX_DLGMODALFRAME,
        class,
        PCWSTR(title.as_ptr()),
        WS_POPUP | WS_CAPTION | WS_SYSMENU,
        px,
        py,
        pw,
        ph,
        Some(owner),
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };
    if is_dark() {
        dark_control(pop, w!("DarkMode_Explorer"));
        dark_titlebar(pop);
    }
    let _ = EnableWindow(owner, false);
    let _ = ShowWindow(pop, SW_SHOW);

    // Modal pump: runs until the popup destroys itself (no PostQuitMessage there,
    // which would kill the parent dialog's loop).
    let mut msg = MSG::default();
    while IsWindow(Some(pop)).as_bool() {
        let r = GetMessageW(&mut msg, None, 0, 0).0;
        if r == 0 || r == -1 {
            break;
        }
        if !IsDialogMessageW(pop, &msg).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let _ = EnableWindow(owner, true);
}

extern "system" fn settings_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        let kind = POPUP_KIND.load(Ordering::Relaxed);
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                let mut y = 16;
                if kind == SK_WEBP {
                    let lossless = WEBP_LOSSLESS.load(Ordering::Relaxed) != 0;
                    let cb = ctl(hwnd, BUTTON, "Lossless", WINDOW_STYLE(BS_AUTOCHECKBOX as u32) | WS_TABSTOP, 16, y, 130, 22, CID_POPUP_LOSSLESS, hinst);
                    SendMessageW(cb, BM_SETCHECK_MSG, Some(WPARAM(lossless as usize)), Some(LPARAM(0)));
                    y += 30;
                }
                let (label, lo, hi, init) = match kind {
                    SK_PNG => ("Compression (0\u{2013}9):", 0, 9, PNG_LEVEL.load(Ordering::Relaxed)),
                    SK_WEBP => ("Quality (1\u{2013}100):", 1, 100, WEBP_QUALITY.load(Ordering::Relaxed)),
                    _ => ("JPEG quality (1\u{2013}100):", 1, 100, QUALITY.load(Ordering::Relaxed)),
                };
                ctl(hwnd, STATIC, label, WINDOW_STYLE(0), 16, y, 200, 18, -1, hinst);
                let tb = ctl(hwnd, w!("msctls_trackbar32"), "", WINDOW_STYLE(TBS_HORZ as u32) | WS_TABSTOP, 12, y + 24, 210, 28, CID_POPUP_TB, hinst);
                SendMessageW(tb, TBM_SETRANGE, Some(WPARAM(1)), Some(LPARAM(make_lparam(lo, hi))));
                SendMessageW(tb, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(init as isize)));
                ctl(hwnd, STATIC, &init.to_string(), WINDOW_STYLE(0), 232, y + 28, 40, 18, CID_POPUP_VAL, hinst);
                if kind == SK_WEBP && WEBP_LOSSLESS.load(Ordering::Relaxed) != 0 {
                    let _ = EnableWindow(tb, false); // quality irrelevant while lossless
                }
                let by = if kind == SK_WEBP { 132 } else { 102 };
                ctl(hwnd, BUTTON, "OK", WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 108, by, 76, 28, IDOK, hinst);
                ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 192, by, 80, 28, IDCANCEL, hinst);
                LRESULT(0)
            }
            WM_HSCROLL => {
                if let Ok(tb) = GetDlgItem(Some(hwnd), CID_POPUP_TB) {
                    let pos = SendMessageW(tb, TBM_GETPOS, None, None).0;
                    set_edit_text(hwnd, CID_POPUP_VAL, &pos.to_string());
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    CID_POPUP_LOSSLESS => {
                        // Lossless toggles the quality slider on/off.
                        let on = checked(hwnd, CID_POPUP_LOSSLESS);
                        if let Ok(tb) = GetDlgItem(Some(hwnd), CID_POPUP_TB) {
                            let _ = EnableWindow(tb, !on);
                        }
                    }
                    IDOK => {
                        let pos = GetDlgItem(Some(hwnd), CID_POPUP_TB)
                            .map(|tb| SendMessageW(tb, TBM_GETPOS, None, None).0 as i32)
                            .unwrap_or(90);
                        match kind {
                            SK_PNG => PNG_LEVEL.store(pos.clamp(0, 9), Ordering::Relaxed),
                            SK_WEBP => {
                                WEBP_LOSSLESS.store(checked(hwnd, CID_POPUP_LOSSLESS) as i32, Ordering::Relaxed);
                                WEBP_QUALITY.store(pos.clamp(1, 100), Ordering::Relaxed);
                            }
                            _ => QUALITY.store(pos.clamp(1, 100), Ordering::Relaxed),
                        }
                        let _ = DestroyWindow(hwnd);
                    }
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_BG);
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(dark_bg_brush().0 as isize)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

extern "system" fn convert_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                build_convert_controls(hwnd, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
                match id {
                    IDOK => start_convert(hwnd),
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    CID_BROWSE => {
                        if let Some(dir) = pick_folder(hwnd) {
                            set_edit_text(hwnd, CID_OUTDIR, &dir);
                        }
                    }
                    CID_SETTINGS => {
                        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                        run_format_settings(hwnd, hinst, combo_sel(hwnd, CID_FORMAT));
                    }
                    CID_FORMAT if notify == CBN_SELCHANGE => update_settings_enabled(hwnd),
                    CID_RESIZE_CHK => update_resize_enabled(hwnd),
                    CID_RESIZE if notify == CBN_SELCHANGE => update_resize_enabled(hwnd),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CONVERT_PROGRESS => {
                if let Ok(p) = GetDlgItem(Some(hwnd), CID_PROGRESS) {
                    SendMessageW(p, PBM_SETPOS, Some(WPARAM(wparam.0)), None);
                }
                LRESULT(0)
            }
            WM_CONVERT_DONE => {
                let done = wide(&format!("Converted {} of {} image(s).", wparam.0, lparam.0));
                let cap = wide("SageThumbs 2K");
                MessageBoxW(Some(hwnd), PCWSTR(done.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | MB_ICONINFORMATION);
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_BG);
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(dark_bg_brush().0 as isize)
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_CTL_BG);
                LRESULT(dark_ctl_brush().0 as isize)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                build_controls(hwnd, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
                match id {
                    IDOK => apply_settings(hwnd), // Save = apply only, keep the window open
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    ID_SELECT_ALL | ID_CLEAR_ALL => {
                        if let Ok(list) = GetDlgItem(Some(hwnd), ID_LIST) {
                            let on = id == ID_SELECT_ALL;
                            for i in 0..formats::FORMATS.len() as i32 {
                                set_check(list, i, on);
                            }
                        }
                    }
                    ID_DEFAULTS => load_defaults(hwnd),
                    ID_LANG if notify == CBN_SELCHANGE => on_lang_change(hwnd),
                    ID_ABOUT => show_about(hwnd),
                    ID_BANNER if notify == STN_CLICKED => {
                        // Open the currently-shown ad's link (or the product page
                        // if no ad feed loaded).
                        let mut url = None;
                        if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
                            let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut AdRotator;
                            if !rot.is_null() {
                                let r = &*rot;
                                if let Some(ad) = r.ads.get(r.cur) {
                                    url = Some(wstr_to_string(&ad.link));
                                }
                            }
                        }
                        match url {
                            Some(u) if !u.is_empty() => open_url(&u),
                            _ => open_url(URL_PRODUCT),
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            // A company SysLink (footer credit or the About box) was clicked, or
            // the banner tooltip is asking for its (rotating) text.
            WM_NOTIFY => {
                let nmhdr = lparam.0 as *const NMHDR;
                if (*nmhdr).code == NM_CLICK || (*nmhdr).code == NM_RETURN {
                    let link = lparam.0 as *const NMLINK;
                    let url = wstr_to_string(&(*link).item.szUrl);
                    if !url.is_empty() {
                        open_url(&url);
                    }
                } else if (*nmhdr).code == TTN_GETDISPINFOW {
                    // Banner hover: hand back the current ad's tooltip. The buffer
                    // lives in the AdRotator (stable until WM_DESTROY frees it).
                    if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
                        if (*nmhdr).idFrom == banner.0 as usize {
                            let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut AdRotator;
                            if !rot.is_null() {
                                let r = &*rot;
                                if let Some(ad) = r.ads.get(r.cur) {
                                    let di = lparam.0 as *mut NMTTDISPINFOW;
                                    (*di).lpszText = PWSTR(ad.tip.as_ptr() as *mut u16);
                                }
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            // Right-click / Shift+F10 on the format list → bulk check/uncheck menu.
            WM_CONTEXTMENU if HWND(wparam.0 as *mut c_void) == GetDlgItem(Some(hwnd), ID_LIST).unwrap_or_default() => {
                list_context_menu(HWND(wparam.0 as *mut c_void), hwnd, lparam);
                LRESULT(0)
            }
            // Owner-drawn dark context-menu items (light text on dark).
            WM_MEASUREITEM => {
                let m = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
                if m.CtlType == ODT_MENU {
                    let label = wide(ctx_menu_label(m.itemID as usize));
                    let n = label.len().saturating_sub(1);
                    let hdc = GetDC(Some(hwnd));
                    let old = SelectObject(hdc, HGDIOBJ(gui_font().0));
                    let mut sz = SIZE::default();
                    let _ = GetTextExtentPoint32W(hdc, &label[..n], &mut sz);
                    SelectObject(hdc, old);
                    ReleaseDC(Some(hwnd), hdc);
                    m.itemWidth = (sz.cx + 30) as u32;
                    m.itemHeight = 26;
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            WM_DRAWITEM => {
                let d = &*(lparam.0 as *const DRAWITEMSTRUCT);
                if d.CtlType == ODT_MENU {
                    let selected = (d.itemState.0 & ODS_SELECTED.0) != 0;
                    let bg = if selected { dark_menu_sel_brush() } else { dark_menu_brush() };
                    FillRect(d.hDC, &d.rcItem, bg);
                    SetBkMode(d.hDC, TRANSPARENT);
                    SetTextColor(d.hDC, DARK_TEXT);
                    SelectObject(d.hDC, HGDIOBJ(gui_font().0));
                    let mut label = wide(ctx_menu_label(d.itemID as usize));
                    let n = label.len().saturating_sub(1);
                    let mut rc = d.rcItem;
                    rc.left += 14;
                    DrawTextW(d.hDC, &mut label[..n], &mut rc, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wparam, lparam)
                }
            }
            // Hand cursor over the clickable banner (so it reads as clickable).
            WM_SETCURSOR if HWND(wparam.0 as *mut c_void) == GetDlgItem(Some(hwnd), ID_BANNER).unwrap_or_default() => {
                let _ = SetCursor(LoadCursorW(None, IDC_HAND).ok());
                LRESULT(1)
            }
            // The ad feed arrived from the download thread: take ownership, show
            // the first ad (replacing the placeholder), and start the timers.
            WM_APP_ADS => {
                if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
                    let rot = lparam.0 as *mut AdRotator;
                    if !rot.is_null() {
                        // Swap in the new feed, freeing any prior one.
                        let prev = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut AdRotator;
                        let _ = KillTimer(Some(hwnd), TIMER_ROTATE);
                        SetWindowLongPtrW(banner, GWLP_USERDATA, rot as isize);
                        let r = &*rot;
                        // First swap frees the embedded placeholder bitmap.
                        show_current_image(hwnd, banner, r, true);
                        if r.rotates() {
                            let _ = SetTimer(Some(hwnd), TIMER_ROTATE, r.rotate_ms, None);
                        }
                        if !prev.is_null() {
                            drop_ad_rotator(prev);
                        }
                    }
                } else {
                    drop_ad_rotator(lparam.0 as *mut AdRotator); // window gone
                }
                LRESULT(0)
            }
            // Advance the current image's GIF animation one frame (frames are reused
            // each loop, so don't free the prior one; WM_DESTROY frees them all).
            WM_TIMER if wparam.0 == TIMER_BANNER => {
                if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
                    let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut AdRotator;
                    if !rot.is_null() {
                        let r = &mut *rot;
                        let (cur, imgi) = (r.cur, r.img);
                        let nframes = r.ads.get(cur).and_then(|a| a.images.get(imgi)).map_or(0, |im| im.frames.len());
                        if nframes > 1 {
                            r.frame = (r.frame + 1) % nframes;
                            let f = r.ads[cur].images[imgi].frames[r.frame];
                            SendMessageW(banner, STM_SETIMAGE, Some(WPARAM(IMAGE_BITMAP.0 as usize)), Some(LPARAM(f)));
                        }
                    }
                }
                LRESULT(0)
            }
            // Rotate to the next company / image: advance the rotator, then show the
            // new art (raw STM_SETIMAGE so the prior bitmap survives — the rotator
            // still owns it). The tooltip pulls the fresh text on the next hover.
            WM_TIMER if wparam.0 == TIMER_ROTATE => {
                if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
                    let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut AdRotator;
                    if !rot.is_null() {
                        (*rot).advance();
                        show_current_image(hwnd, banner, &*rot, false);
                    }
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                // Stop + free the ad rotation (both timers + every ad's bitmaps).
                if let Ok(banner) = GetDlgItem(Some(hwnd), ID_BANNER) {
                    let rot = GetWindowLongPtrW(banner, GWLP_USERDATA) as *mut AdRotator;
                    if !rot.is_null() {
                        let _ = KillTimer(Some(hwnd), TIMER_BANNER);
                        let _ = KillTimer(Some(hwnd), TIMER_ROTATE);
                        SetWindowLongPtrW(banner, GWLP_USERDATA, 0);
                        drop_ad_rotator(rot);
                    }
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            // Dark-mode coloring for the parts the visual style doesn't theme:
            // static labels and the numeric edit boxes (light text on dark).
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_BG);
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(dark_bg_brush().0 as isize)
            }
            WM_CTLCOLOREDIT | WM_CTLCOLORLISTBOX if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_CTL_BG);
                LRESULT(dark_ctl_brush().0 as isize)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ---- Small control helpers ---------------------------------------------

// Button control messages (CheckDlgButton/IsDlgButtonChecked aren't in this
// windows-rs metadata, so drive the BUTTON control directly).
const BM_GETCHECK_MSG: u32 = 0x00F0;
const BM_SETCHECK_MSG: u32 = 0x00F1;
const BST_CHECKED: isize = 1;

unsafe fn check(hwnd: HWND, id: i32, on: bool) {
    if let Ok(h) = GetDlgItem(Some(hwnd), id) {
        SendMessageW(h, BM_SETCHECK_MSG, Some(WPARAM(on as usize)), Some(LPARAM(0)));
    }
}
unsafe fn checked(hwnd: HWND, id: i32) -> bool {
    match GetDlgItem(Some(hwnd), id) {
        Ok(h) => SendMessageW(h, BM_GETCHECK_MSG, None, None).0 == BST_CHECKED,
        Err(_) => false,
    }
}

unsafe fn set_check(list: HWND, item: i32, on: bool) {
    let st = LVITEMW {
        state: LIST_VIEW_ITEM_STATE_FLAGS(if on { CHECKED } else { UNCHECKED }),
        stateMask: LVIS_STATEIMAGEMASK,
        ..Default::default()
    };
    SendMessageW(
        list,
        LVM_SETITEMSTATE,
        Some(WPARAM(item as usize)),
        Some(LPARAM(&st as *const _ as isize)),
    );
}
unsafe fn is_checked(list: HWND, item: i32) -> bool {
    let st = SendMessageW(
        list,
        LVM_GETITEMSTATE,
        Some(WPARAM(item as usize)),
        Some(LPARAM(LVIS_STATEIMAGEMASK.0 as isize)),
    );
    (st.0 as u32 & 0x3000) == CHECKED
}

// =========================== ListView subclass ===========================
// A single subclass on the ListView does the three things SetWindowTheme can't:
//   * dark HEADER text — the header is a child of the ListView, so its
//     NM_CUSTOMDRAW arrives here (the theme darkens the header fill but leaves
//     the text drawn black; only custom-draw overrides the per-item color);
//   * SPACE bulk-toggles the checkboxes of every selected row (the control would
//     otherwise toggle only the focused one);
//   * right-click / Shift+F10 opens a Check / Uncheck / Toggle-selected menu.

unsafe fn list_header(list: HWND) -> HWND {
    HWND(SendMessageW(list, LVM_GETHEADER, None, None).0 as *mut c_void)
}

unsafe fn lv_next(list: HWND, start: i32, flags: u32) -> i32 {
    SendMessageW(list, LVM_GETNEXTITEM, Some(WPARAM(start as usize)), Some(LPARAM(flags as isize))).0 as i32
}

unsafe fn bulk_set_selected(list: HWND, target: bool) {
    let mut i = lv_next(list, -1, LVNI_SELECTED);
    while i >= 0 {
        set_check(list, i, target);
        i = lv_next(list, i, LVNI_SELECTED);
    }
}

/// Toggle the checkboxes of all selected rows to a single uniform state — the
/// inverse of the focused row — so a mixed selection collapses predictably.
unsafe fn bulk_toggle_selected(list: HWND) {
    let focus = lv_next(list, -1, LVNI_FOCUSED);
    let target = if focus >= 0 { !is_checked(list, focus) } else { true };
    bulk_set_selected(list, target);
}

/// Label for an owner-drawn format-list context-menu item.
fn ctx_menu_label(id: usize) -> &'static str {
    match id {
        1 => t("ctx_check_selected"),
        2 => t("ctx_uncheck_selected"),
        3 => t("ctx_toggle_selected"),
        _ => "",
    }
}

unsafe fn list_context_menu(list: HWND, owner: HWND, l: LPARAM) {
    // Keyboard invocation (Shift+F10 / Apps key) sets BOTH coords to -1 — not the
    // whole lParam — and real multi-monitor coords can be negative, so test the
    // sign-extended halves separately.
    let x = (l.0 & 0xFFFF) as u16 as i16 as i32;
    let y = ((l.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    let (px, py) = if x == -1 && y == -1 {
        let mut r = RECT::default();
        let _ = GetWindowRect(list, &mut r);
        (r.left + 8, r.top + 8)
    } else {
        (x, y)
    };
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    // Dark mode: owner-draw the items. A normal menu renders black text on the
    // immersive dark background (unreadable until the row is highlighted), so we
    // draw the text light ourselves in WM_DRAWITEM. Light mode uses a normal menu.
    let (s1, s2, s3);
    if is_dark() {
        let _ = AppendMenuW(menu, MF_OWNERDRAW, 1, PCWSTR(1 as *const u16));
        let _ = AppendMenuW(menu, MF_OWNERDRAW, 2, PCWSTR(2 as *const u16));
        let _ = AppendMenuW(menu, MF_OWNERDRAW, 3, PCWSTR(3 as *const u16));
    } else {
        s1 = wide(t("ctx_check_selected"));
        s2 = wide(t("ctx_uncheck_selected"));
        s3 = wide(t("ctx_toggle_selected"));
        let _ = AppendMenuW(menu, MF_STRING, 1, PCWSTR(s1.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, 2, PCWSTR(s2.as_ptr()));
        let _ = AppendMenuW(menu, MF_STRING, 3, PCWSTR(s3.as_ptr()));
    }
    // Foreground + WM_NULL bracket: the documented fix for the "menu shows then
    // immediately vanishes" quirk. Owner is the top-level dialog, not the list.
    let _ = SetForegroundWindow(owner);
    let cmd = TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY, px, py, Some(0), owner, None);
    let _ = PostMessageW(Some(owner), WM_NULL, WPARAM(0), LPARAM(0));
    let _ = DestroyMenu(menu);
    match cmd.0 {
        1 => bulk_set_selected(list, true),
        2 => bulk_set_selected(list, false),
        3 => bulk_toggle_selected(list),
        _ => {}
    }
}

unsafe extern "system" fn list_subclass(
    h: HWND,
    msg: u32,
    w: WPARAM,
    l: LPARAM,
    uid: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(h, Some(list_subclass), uid);
        }
        WM_NOTIFY if is_dark() => {
            let nmhdr = l.0 as *const NMHDR;
            if (*nmhdr).code == NM_CUSTOMDRAW && (*nmhdr).hwndFrom == list_header(h) {
                let nmcd = l.0 as *const NMCUSTOMDRAW;
                let stage = (*nmcd).dwDrawStage;
                if stage == CDDS_PREPAINT {
                    return LRESULT(CDRF_NOTIFYITEMDRAW as isize);
                } else if stage == CDDS_ITEMPREPAINT {
                    SetTextColor((*nmcd).hdc, DARK_TEXT);
                    return LRESULT(CDRF_DODEFAULT as isize);
                }
            }
        }
        WM_KEYDOWN if w.0 as u16 == VK_SPACE.0 => {
            if SendMessageW(h, LVM_GETSELECTEDCOUNT, None, None).0 > 1 {
                bulk_toggle_selected(h);
                return LRESULT(0); // eat the key so the control doesn't single-toggle too
            }
        }
        // WM_CONTEXTMENU is handled in the dialog proc (it bubbles to the parent).
        _ => {}
    }
    DefSubclassProc(h, msg, w, l)
}

/// Dark-theme a CBS_DROPDOWNLIST combo. The combo HWND needs the dark
/// common-file-dialog theme (`DarkMode_CFD`) — NOT `DarkMode_Explorer`, which is
/// the tree/list class and leaves a light closed face — while the popup list
/// (a separate child window) gets `DarkMode_Explorer`.
unsafe fn dark_theme_combo(combo: HWND) {
    if !is_dark() {
        return;
    }
    let mut cbi = COMBOBOXINFO {
        cbSize: std::mem::size_of::<COMBOBOXINFO>() as u32,
        ..Default::default()
    };
    if GetComboBoxInfo(combo, &mut cbi).is_ok() && !cbi.hwndList.is_invalid() {
        let _ = SetWindowTheme(cbi.hwndList, w!("DarkMode_Explorer"), PCWSTR::null());
    }
    dark_control(combo, w!("DarkMode_CFD")); // AllowDarkModeForWindow + SetWindowTheme
}

// ========================= Files to folder ==========================
// A name-prompt dialog for the DLL's "Files to folder" verb on a multi-file
// selection (`--files-to-folder <listfile>`). Single-file selections are handled
// in the DLL with no prompt. The actual create-folder-and-move lives in the lib
// (`sagethumbs2k::files_to_folder`), shared with the DLL's single-file path.

const CID_F2F_NAME: i32 = 5001;
/// Edit-control "select text" message (not in the windows-rs metadata).
const EM_SETSEL: u32 = 0x00B1;
static F2F_FILES: OnceLock<Vec<String>> = OnceLock::new();

unsafe fn run_files_to_folder_dialog(hinst: HINSTANCE, listfile: &str) {
    let files: Vec<String> = std::fs::read_to_string(listfile)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let _ = std::fs::remove_file(listfile);
    if files.is_empty() {
        return;
    }
    let _ = F2F_FILES.set(files);

    let class = w!("SageThumbs2KFilesToFolder");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(f2f_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if is_dark() { dark_bg_brush() } else { HBRUSH(16isize as *mut c_void) },
        ..Default::default()
    };
    RegisterClassW(&wc);

    if let Ok(hwnd) = CreateWindowExW(
        WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
        class,
        w!("Files to folder \u{2014} SageThumbs 2K"),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        392,
        168,
        None,
        None,
        Some(hinst),
        None,
    ) {
        if is_dark() {
            dark_control(hwnd, w!("DarkMode_Explorer"));
            dark_titlebar(hwnd);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

extern "system" fn f2f_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                let n = F2F_FILES.get().map(|f| f.len()).unwrap_or(0);
                let lbl = WINDOW_STYLE(0);
                ctl(hwnd, STATIC, &format!("Move {n} item(s) into a new folder named:"), lbl, 16, 16, 344, 18, -1, hinst);
                let edit = ctl(
                    hwnd,
                    EDIT,
                    "New Folder",
                    WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP,
                    16, 44, 344, 26, CID_F2F_NAME, hinst,
                );
                // Select-all + focus so the suggested name is replaced on first type.
                SendMessageW(edit, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
                let _ = SetFocus(Some(edit));
                ctl(hwnd, BUTTON, "Create folder", WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 176, 92, 104, 30, IDOK, hinst);
                ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 286, 92, 88, 30, IDCANCEL, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    IDOK => {
                        let mut name = get_edit_text(hwnd, CID_F2F_NAME).trim().to_string();
                        if name.is_empty() {
                            name = "New Folder".to_string();
                        }
                        if let Some(files) = F2F_FILES.get() {
                            let _ = sagethumbs2k::files_to_folder(files, &name);
                        }
                        let _ = DestroyWindow(hwnd);
                    }
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_BG);
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(dark_bg_brush().0 as isize)
            }
            WM_CTLCOLOREDIT if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_CTL_BG);
                LRESULT(dark_ctl_brush().0 as isize)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ========================= Tags to folders ==========================
// The DLL's "Sort into folders ▸ By audio tag" verb on an audio selection
// (`--tags-to-folders <listfile>`). Dialog: destination, a `$artist - $album`
// folder-name template, and copy-vs-move. The sort engine is in the lib
// (`sagethumbs2k::tags_to_folders`).

const CID_TTF_DEST: i32 = 5101;
const CID_TTF_BROWSE: i32 = 5102;
const CID_TTF_TEMPLATE: i32 = 5103;
const CID_TTF_MISSING: i32 = 5104;
const CID_TTF_MOVE: i32 = 5105;
const CID_TTF_COPY: i32 = 5106;
static TTF_FILES: OnceLock<Vec<String>> = OnceLock::new();

unsafe fn run_tags_to_folders_dialog(hinst: HINSTANCE, listfile: &str) {
    let files: Vec<String> = std::fs::read_to_string(listfile)
        .unwrap_or_default()
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let _ = std::fs::remove_file(listfile);
    if files.is_empty() {
        return;
    }
    let _ = TTF_FILES.set(files);

    let class = w!("SageThumbs2KTagsToFolders");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(ttf_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: if is_dark() { dark_bg_brush() } else { HBRUSH(16isize as *mut c_void) },
        ..Default::default()
    };
    RegisterClassW(&wc);

    if let Ok(hwnd) = CreateWindowExW(
        WS_EX_CONTROLPARENT | WS_EX_DLGMODALFRAME,
        class,
        w!("Sort into folders by tag \u{2014} SageThumbs 2K"),
        WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        452,
        252,
        None,
        None,
        Some(hinst),
        None,
    ) {
        if is_dark() {
            dark_control(hwnd, w!("DarkMode_Explorer"));
            dark_titlebar(hwnd);
        }
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0).0;
            if r == 0 || r == -1 {
                break;
            }
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}

extern "system" fn ttf_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let hinst: HINSTANCE = GetModuleHandleW(None).unwrap().into();
                let lbl = WINDOW_STYLE(0);
                // Default destination = the first file's folder.
                let default_dest = TTF_FILES
                    .get()
                    .and_then(|f| f.first())
                    .and_then(|p| std::path::Path::new(p).parent())
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();

                ctl(hwnd, STATIC, "Destination:", lbl, 16, 18, 90, 18, -1, hinst);
                let dest = ctl(hwnd, EDIT, &default_dest, WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 110, 16, 268, 24, CID_TTF_DEST, hinst);
                let _ = dest;
                ctl(hwnd, BUTTON, "\u{2026}", WS_TABSTOP, 384, 15, 44, 26, CID_TTF_BROWSE, hinst);

                ctl(hwnd, STATIC, "Folder template:", lbl, 16, 56, 90, 18, -1, hinst);
                ctl(hwnd, EDIT, "$artist - $album", WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 110, 54, 318, 24, CID_TTF_TEMPLATE, hinst);
                ctl(hwnd, STATIC, "Tokens:  $artist   $album   $title   $track   (use \\ to nest)", lbl, 110, 82, 318, 16, -1, hinst);

                ctl(hwnd, STATIC, "Missing tag:", lbl, 16, 112, 90, 18, -1, hinst);
                ctl(hwnd, EDIT, "Unknown", WINDOW_STYLE(ES_AUTOHSCROLL as u32) | WS_BORDER | WS_TABSTOP, 110, 110, 160, 24, CID_TTF_MISSING, hinst);

                let mv = ctl(hwnd, BUTTON, "Move files", WINDOW_STYLE(BS_AUTORADIOBUTTON as u32) | WS_GROUP | WS_TABSTOP, 110, 146, 110, 22, CID_TTF_MOVE, hinst);
                ctl(hwnd, BUTTON, "Copy files", WINDOW_STYLE(BS_AUTORADIOBUTTON as u32) | WS_TABSTOP, 230, 146, 110, 22, CID_TTF_COPY, hinst);
                SendMessageW(mv, BM_SETCHECK_MSG, Some(WPARAM(1)), Some(LPARAM(0))); // default: Move

                ctl(hwnd, BUTTON, "Sort", WINDOW_STYLE(BS_DEFPUSHBUTTON as u32) | WS_TABSTOP, 244, 188, 92, 30, IDOK, hinst);
                ctl(hwnd, BUTTON, t("btn_cancel"), WS_TABSTOP, 342, 188, 88, 30, IDCANCEL, hinst);
                LRESULT(0)
            }
            WM_COMMAND => {
                let id = (wparam.0 & 0xFFFF) as i32;
                match id {
                    CID_TTF_BROWSE => {
                        if let Some(dir) = pick_folder(hwnd) {
                            set_edit_text(hwnd, CID_TTF_DEST, &dir);
                        }
                    }
                    IDOK => {
                        let mut dest = get_edit_text(hwnd, CID_TTF_DEST).trim().to_string();
                        if dest.is_empty() {
                            dest = TTF_FILES
                                .get()
                                .and_then(|f| f.first())
                                .and_then(|p| std::path::Path::new(p).parent())
                                .map(|p| p.to_string_lossy().into_owned())
                                .unwrap_or_else(|| ".".to_string());
                        }
                        let mut template = get_edit_text(hwnd, CID_TTF_TEMPLATE).trim().to_string();
                        if template.is_empty() {
                            template = "$artist - $album".to_string();
                        }
                        let mut missing = get_edit_text(hwnd, CID_TTF_MISSING).trim().to_string();
                        if missing.is_empty() {
                            missing = "Unknown".to_string();
                        }
                        let move_files = checked(hwnd, CID_TTF_MOVE);
                        let (done, skipped) = if let Some(files) = TTF_FILES.get() {
                            sagethumbs2k::tags_to_folders(files, std::path::Path::new(&dest), &template, &missing, move_files)
                        } else {
                            (0, 0)
                        };
                        let verb = if move_files { "Moved" } else { "Copied" };
                        let m = wide(&format!("{verb} {done} file(s) into tag folders.\n{skipped} skipped."));
                        let cap = wide("SageThumbs 2K");
                        MessageBoxW(Some(hwnd), PCWSTR(m.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | MB_ICONINFORMATION);
                        let _ = DestroyWindow(hwnd);
                    }
                    IDCANCEL => {
                        let _ = DestroyWindow(hwnd);
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLORBTN if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_BG);
                SetBkMode(hdc, TRANSPARENT);
                LRESULT(dark_bg_brush().0 as isize)
            }
            WM_CTLCOLOREDIT if is_dark() => {
                let hdc = HDC(wparam.0 as *mut c_void);
                SetTextColor(hdc, DARK_TEXT);
                SetBkColor(hdc, DARK_CTL_BG);
                LRESULT(dark_ctl_brush().0 as isize)
            }
            WM_CLOSE => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

// ============================ Eyedropper =============================
// A SYSTEM-WIDE screen color picker (launched by the DLL's "Pick color" verb as
// `--eyedropper`). It freezes a snapshot of the whole (virtual) screen in a
// fullscreen topmost window, follows the cursor with a magnifier loupe, and on a
// click samples the pixel under the cursor and copies its #RRGGBB to the
// clipboard. Esc cancels. The selected file is irrelevant — this picks a color
// from anywhere on screen (by design; the old image-window version
// was replaced).

const EYE_K: i32 = 7; // half-window: a (2K+1)² block of screen pixels in the loupe
const EYE_SPAN: i32 = 2 * EYE_K + 1; // 15 px sampled across
const EYE_MAG: i32 = 150; // magnified loupe size (px) → 10× zoom
const EYE_LBL: i32 = 46; // loupe label strip (px): hex row + hint row

/// The frozen screen snapshot: a memory DC (with its bitmap selected) we BitBlt
/// to display, StretchBlt for the loupe, and GetPixel for sampling.
static EYE_SHOT: OnceLock<usize> = OnceLock::new(); // HDC
static EYE_SHOT_BMP: OnceLock<usize> = OnceLock::new(); // HBITMAP (freed on close)
static EYE_VW: AtomicI32 = AtomicI32::new(0); // snapshot / window size
static EYE_VH: AtomicI32 = AtomicI32::new(0);
/// Last cursor client position (drives the loupe; starts off-screen).
static EYE_LAST_X: AtomicI32 = AtomicI32::new(-10000);
static EYE_LAST_Y: AtomicI32 = AtomicI32::new(-10000);

unsafe fn run_eyedropper(hinst: HINSTANCE) {
    // Snapshot the whole virtual screen into a memory DC.
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
    SelectObject(mem, HGDIOBJ(bmp.0)); // keep selected → mem is a readable copy of the screen
    let _ = BitBlt(mem, 0, 0, vw, vh, Some(screen), vx, vy, SRCCOPY);
    ReleaseDC(None, screen);
    let _ = EYE_SHOT.set(mem.0 as usize);
    let _ = EYE_SHOT_BMP.set(bmp.0 as usize);
    EYE_VW.store(vw, Ordering::Relaxed);
    EYE_VH.store(vh, Ordering::Relaxed);

    let class = w!("SageThumbs2KEyedropper");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(eyedropper_wndproc),
        hInstance: hinst,
        lpszClassName: class,
        hIcon: app_icon().unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&wc);

    // Fullscreen, borderless, topmost — covers the whole virtual screen so the
    // cursor is always over us (no global hook needed to catch clicks).
    if let Ok(hwnd) = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        class,
        w!("Pick color"),
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
}

/// Sample the screen-snapshot pixel at (x, y) as (r, g, b) via GetPixel.
fn eye_sample(x: i32, y: i32) -> (u8, u8, u8) {
    let Some(&dc) = EYE_SHOT.get() else {
        return (0, 0, 0);
    };
    let (vw, vh) = (EYE_VW.load(Ordering::Relaxed), EYE_VH.load(Ordering::Relaxed));
    let x = x.clamp(0, (vw - 1).max(0));
    let y = y.clamp(0, (vh - 1).max(0));
    let c = unsafe { GetPixel(HDC(dc as *mut c_void), x, y) }.0; // 0x00BBGGRR, or CLR_INVALID
    if c == 0xFFFF_FFFF {
        return (0, 0, 0);
    }
    ((c & 0xFF) as u8, ((c >> 8) & 0xFF) as u8, ((c >> 16) & 0xFF) as u8)
}

/// The loupe's box rect for a cursor at (cx, cy), nudged to stay on-screen.
fn eye_loupe_box(cx: i32, cy: i32) -> RECT {
    let (vw, vh) = (EYE_VW.load(Ordering::Relaxed), EYE_VH.load(Ordering::Relaxed));
    let (bw, bh) = (EYE_MAG, EYE_MAG + EYE_LBL);
    let gap = 18;
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

extern "system" fn eyedropper_wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1), // the snapshot covers every pixel
            WM_MOUSEMOVE => {
                let mx = (lparam.0 & 0xffff) as u16 as i16 as i32;
                let my = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
                let ox = EYE_LAST_X.swap(mx, Ordering::Relaxed);
                let oy = EYE_LAST_Y.swap(my, Ordering::Relaxed);
                // Repaint the old + new loupe boxes (erase old, draw new).
                let old = eye_loupe_box(ox, oy);
                let new = eye_loupe_box(mx, my);
                let _ = InvalidateRect(Some(hwnd), Some(&old), false);
                let _ = InvalidateRect(Some(hwnd), Some(&new), false);
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                let mx = (lparam.0 & 0xffff) as u16 as i16 as i32;
                let my = ((lparam.0 >> 16) & 0xffff) as u16 as i16 as i32;
                let (r, g, b) = eye_sample(mx, my);
                set_clipboard_text(&format!("#{r:02X}{g:02X}{b:02X}"));
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            // Space picks the pixel under the cursor (a steadier alternative to a
            // click — your hand doesn't move).
            WM_KEYDOWN if wparam.0 == VK_SPACE.0 as usize => {
                let cx = EYE_LAST_X.load(Ordering::Relaxed);
                let cy = EYE_LAST_Y.load(Ordering::Relaxed);
                if cx > -10000 {
                    let (r, g, b) = eye_sample(cx, cy);
                    set_clipboard_text(&format!("#{r:02X}{g:02X}{b:02X}"));
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_KEYDOWN if wparam.0 == VK_ESCAPE.0 as usize => {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_PAINT => {
                eye_paint(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                if let Some(&dc) = EYE_SHOT.get() {
                    let _ = DeleteDC(HDC(dc as *mut c_void));
                }
                if let Some(&bmp) = EYE_SHOT_BMP.get() {
                    let _ = DeleteObject(HGDIOBJ(bmp as *mut c_void));
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}

unsafe fn eye_paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    if let Some(&shot) = EYE_SHOT.get() {
        let shotdc = HDC(shot as *mut c_void);
        let pr = ps.rcPaint;
        // Restore the snapshot under the invalid region (erasing the old loupe).
        let _ = BitBlt(hdc, pr.left, pr.top, pr.right - pr.left, pr.bottom - pr.top, Some(shotdc), pr.left, pr.top, SRCCOPY);
        // Draw the loupe at the current cursor.
        let cx = EYE_LAST_X.load(Ordering::Relaxed);
        let cy = EYE_LAST_Y.load(Ordering::Relaxed);
        if cx > -10000 {
            eye_draw_loupe(hdc, shotdc, cx, cy);
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Draw the magnifier loupe (zoomed pixels + crosshair + hex label) near the
/// cursor, sampling from the frozen `shotdc`.
unsafe fn eye_draw_loupe(hdc: HDC, shotdc: HDC, cx: i32, cy: i32) {
    let lb = eye_loupe_box(cx, cy);
    let (bx, by) = (lb.left, lb.top);

    // Magnified pixels — nearest-neighbor so each screen pixel is a crisp block.
    SetStretchBltMode(hdc, COLORONCOLOR);
    let _ = StretchBlt(hdc, bx, by, EYE_MAG, EYE_MAG, Some(shotdc), cx - EYE_K, cy - EYE_K, EYE_SPAN, EYE_SPAN, SRCCOPY);

    // Crosshair on the center cell (the pixel that gets picked).
    let cell = EYE_MAG / EYE_SPAN;
    let cc = RECT {
        left: bx + EYE_K * cell,
        top: by + EYE_K * cell,
        right: bx + EYE_K * cell + cell,
        bottom: by + EYE_K * cell + cell,
    };
    let red = CreateSolidBrush(rgb(255, 40, 40));
    FrameRect(hdc, &cc, red);
    let _ = DeleteObject(red.into());

    // Label strip: swatch + hex (top row), then a "Press Space to copy" hint.
    let (r, g, b) = eye_sample(cx, cy);
    let lbl = RECT { left: bx, top: by + EYE_MAG, right: bx + EYE_MAG, bottom: by + EYE_MAG + EYE_LBL };
    let lbg = CreateSolidBrush(rgb(24, 24, 24));
    FillRect(hdc, &lbl, lbg);
    let _ = DeleteObject(lbg.into());
    let sw = RECT { left: bx + 5, top: by + EYE_MAG + 5, right: bx + 21, bottom: by + EYE_MAG + 21 };
    let swb = CreateSolidBrush(rgb(r, g, b));
    FillRect(hdc, &sw, swb);
    let _ = DeleteObject(swb.into());

    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    // Hex (row 1).
    SetTextColor(hdc, rgb(240, 240, 240));
    let mut hex = wide(&format!("#{r:02X}{g:02X}{b:02X}"));
    let hn = hex.len().saturating_sub(1);
    let mut hr = RECT { left: bx + 28, top: by + EYE_MAG + 2, right: bx + EYE_MAG, bottom: by + EYE_MAG + 24 };
    DrawTextW(hdc, &mut hex[..hn], &mut hr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
    // Hint (row 2).
    SetTextColor(hdc, rgb(150, 150, 150));
    let mut hint = wide("Press Space (or click) to copy · Esc");
    let hin = hint.len().saturating_sub(1);
    let mut hir = RECT { left: bx + 6, top: by + EYE_MAG + 24, right: bx + EYE_MAG, bottom: by + EYE_MAG + EYE_LBL };
    DrawTextW(hdc, &mut hint[..hin], &mut hir, DT_LEFT | DT_VCENTER | DT_SINGLELINE);

    // Outer + magnifier borders.
    let border = CreateSolidBrush(rgb(0, 0, 0));
    let outer = RECT { left: bx, top: by, right: bx + EYE_MAG, bottom: by + EYE_MAG + EYE_LBL };
    FrameRect(hdc, &outer, border);
    let mag = RECT { left: bx, top: by, right: bx + EYE_MAG, bottom: by + EYE_MAG };
    FrameRect(hdc, &mag, border);
    let _ = DeleteObject(border.into());
}

/// Put `text` on the clipboard as Unicode text. Best-effort.
unsafe fn set_clipboard_text(text: &str) -> bool {
    const CF_UNICODETEXT: u32 = 13;
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) else {
        return false;
    };
    let p = GlobalLock(hmem) as *mut u16;
    if p.is_null() {
        let _ = GlobalFree(Some(hmem));
        return false;
    }
    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
    let _ = GlobalUnlock(hmem);
    if OpenClipboard(None).is_err() {
        let _ = GlobalFree(Some(hmem));
        return false;
    }
    let _ = EmptyClipboard();
    if SetClipboardData(CF_UNICODETEXT, Some(HANDLE(hmem.0))).is_err() {
        let _ = CloseClipboard();
        let _ = GlobalFree(Some(hmem));
        return false;
    }
    let _ = CloseClipboard();
    true
}
