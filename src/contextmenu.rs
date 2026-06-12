//! Classic IContextMenu + IShellExtInit handler.
//!
//! The modern IExplorerCommand verb (command.rs) only shows in the stock Win11
//! menu. Many machines replace that with the classic menu (StartAllBack,
//! ExplorerPatcher, or the {86ca1aa0…} registry tweak), where only classic
//! IContextMenu handlers appear. This handler covers those machines, surfacing
//! the same verbs (verbs.rs) as a "SageThumbs 2K" submenu.
//!
//! It also draws the signature SageThumbs/XnShell **menu preview**: an
//! owner-drawn item showing the image's thumbnail + name + dimensions/size,
//! either at the top of our submenu or directly on the main menu (Options).
//! Owner-draw messages reach us via IContextMenu2/3's HandleMenuMsg(2) — the
//! same mechanism the original SageThumbs used. (The stock Win11 modern menu
//! cannot host owner-drawn items, so the preview appears in the classic menu /
//! "Show more options" path.)

use core::cell::{Cell, RefCell};

use windows_implement::implement;
use windows::core::{Error, Ref, Result, HRESULT, HSTRING, PCWSTR, PSTR};
use windows::Win32::Foundation::{COLORREF, E_FAIL, E_NOTIMPL, LPARAM, LRESULT, RECT, SIZE, S_OK, WPARAM};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleDC, CreateFontIndirectW, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, FillRect, GetStockObject, GetSysColor, GetTextExtentPoint32W, SelectObject,
    SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, BLENDFUNCTION, COLOR_HIGHLIGHT,
    COLOR_HIGHLIGHTTEXT, COLOR_MENU, COLOR_MENUTEXT, DEFAULT_GUI_FONT, DT_CENTER, DT_END_ELLIPSIS,
    DT_SINGLELINE, HBITMAP, HFONT, HGDIOBJ, TRANSPARENT,
};
use windows::Win32::System::Com::{IDataObject, DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows::Win32::System::Ole::ReleaseStgMedium;
use windows::Win32::System::Registry::HKEY;
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_SELECTED, ODT_MENU};
use windows::Win32::UI::Shell::Common::ITEMIDLIST;
use windows::Win32::UI::Shell::{
    DragQueryFileW, IContextMenu3, IContextMenu2_Impl, IContextMenu3_Impl, IContextMenu_Impl,
    IShellExtInit, IShellExtInit_Impl, ShellExecuteW, CMINVOKECOMMANDINFO, HDROP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, GetSystemMetrics, InsertMenuW, SetMenuItemInfoW,
    SystemParametersInfoW, HMENU, MENUITEMINFOW, MF_BYPOSITION, MF_OWNERDRAW, MF_POPUP,
    MF_SEPARATOR, MF_STRING, MIIM_BITMAP, NONCLIENTMETRICSW, SM_CXMENUCHECK, SM_CYMENUCHECK,
    SPI_GETNONCLIENTMETRICS, SW_SHOWNORMAL, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_DRAWITEM,
    WM_MEASUREITEM,
};

use crate::{safety, settings, verbs};

const CF_HDROP: u16 = 15;
const CMF_DEFAULTONLY: u32 = 0x0000_0001;

/// Max HEIGHT of the in-menu preview thumbnail, px — kept small so the menu item
/// stays XnView-narrow.
const PREVIEW_BOX: u32 = 88;
/// Max WIDTH of the preview thumbnail. Wider than [`PREVIEW_BOX`] so a panorama /
/// wide image isn't squashed into a tiny sliver — it gets up to this much width
/// (the menu item grows to fit). Normal-aspect images stay height-limited at 88.
const PREVIEW_WIDE: u32 = 220;
/// Cap on the caption text width so a long filename can't widen the whole menu.
const CAPTION_MAX: i32 = 156;
/// Don't decode huge files just for a menu preview (keeps the menu snappy).
const PREVIEW_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// The decoded preview for the current selection (single image only).
struct Preview {
    hbm: HBITMAP,
    w: i32,
    h: i32,
    name: Vec<u16>, // file name, UTF-16 (no NUL — DrawTextW takes a slice)
    info: Vec<u16>, // "1500 × 1500 px – 96 KB"
}

impl Drop for Preview {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.hbm.into());
        }
    }
}

#[implement(IShellExtInit, IContextMenu3)]
pub struct ContextMenu {
    _ref: crate::ModuleRef,
    paths: RefCell<Vec<String>>,
    preview: RefCell<Option<Preview>>,
    /// Absolute menu command id of the preview item (set in QueryContextMenu).
    preview_cmd: Cell<Option<u32>>,
}

impl Default for ContextMenu {
    fn default() -> Self {
        Self {
            _ref: crate::ModuleRef::default(),
            paths: RefCell::new(Vec::new()),
            preview: RefCell::new(None),
            preview_cmd: Cell::new(None),
        }
    }
}

/// Pull the selected file paths out of the shell's IDataObject (CF_HDROP).
unsafe fn hdrop_paths(obj: &IDataObject) -> Result<Vec<String>> {
    let fmt = FORMATETC {
        cfFormat: CF_HDROP,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0 as u32,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = obj.GetData(&fmt)?;
    let hdrop = HDROP(medium.u.hGlobal.0);
    let count = DragQueryFileW(hdrop, 0xFFFF_FFFF, None);
    let mut paths = Vec::new();
    for i in 0..count {
        let len = DragQueryFileW(hdrop, i, None) as usize;
        let mut buf = vec![0u16; len + 1];
        let got = DragQueryFileW(hdrop, i, Some(&mut buf)) as usize;
        paths.push(String::from_utf16_lossy(&buf[..got]));
    }
    ReleaseStgMedium(&mut medium);
    Ok(paths)
}

/// Decode `path` into the menu-preview payload (thumbnail DIB + caption lines).
fn build_preview(path: &str) -> Option<Preview> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > PREVIEW_MAX_BYTES || meta.len() > settings::max_file_size_bytes() {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    let img = crate::decode::decode_full(&bytes).ok()?;
    let (ow, oh) = (img.width(), img.height());
    // Width up to PREVIEW_WIDE, height up to PREVIEW_BOX: wide images render wide,
    // normal/tall ones stay capped at the 88px height (unchanged from before).
    let thumb = img.thumbnail(PREVIEW_WIDE, PREVIEW_BOX);
    let rgba = thumb.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    let hbm = unsafe { crate::dib::create_premultiplied_dib(w, h, rgba.as_raw()).ok()? };

    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);
    let kb = meta.len() as f64 / 1024.0;
    let size_txt = if kb >= 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{kb:.0} KB")
    };
    let info = format!("{ow} \u{00d7} {oh} px  \u{2013}  {size_txt}");
    Some(Preview {
        hbm,
        w,
        h,
        name: name.encode_utf16().collect(),
        info: info.encode_utf16().collect(),
    })
}

/// The app logo (256×256 PNG), embedded so the classic menu can show a brand
/// icon in front of the "SageThumbs 2K" submenu anchor.
const MENU_LOGO_PNG: &[u8] = include_bytes!("../assets/logo.png");

/// The logo as a 32-bpp premultiplied-alpha bitmap at the system menu-check
/// size (DPI-aware) — Vista+ menus alpha-blend such `hbmpItem` bitmaps natively.
/// Created once per process and never freed: live menus may reference it for
/// the host's lifetime, and it's a single small bitmap.
fn menu_logo() -> HBITMAP {
    use std::sync::OnceLock;
    static LOGO: OnceLock<isize> = OnceLock::new();
    let h = *LOGO.get_or_init(|| {
        let cx = unsafe { GetSystemMetrics(SM_CXMENUCHECK) }.max(16);
        let cy = unsafe { GetSystemMetrics(SM_CYMENUCHECK) }.max(16);
        let Ok(img) = image::load_from_memory(MENU_LOGO_PNG) else {
            return 0;
        };
        let rgba = img
            .resize_exact(cx as u32, cy as u32, image::imageops::FilterType::Lanczos3)
            .to_rgba8();
        unsafe { crate::dib::create_premultiplied_dib(cx, cy, rgba.as_raw()) }
            .map(|b| b.0 as isize)
            .unwrap_or(0)
    });
    HBITMAP(h as *mut core::ffi::c_void)
}

/// Recursively append the verb tree into `parent`, assigning command ids in
/// depth-first leaf order from `idcmdfirst`, stopping after `budget` leaves.
unsafe fn build_menu_into(
    parent: HMENU,
    items: &[verbs::MenuItem],
    idcmdfirst: u32,
    next_leaf: &mut u32,
    budget: u32,
) {
    for it in items {
        match it {
            verbs::MenuItem::Group(title, children) => {
                let Ok(sub) = CreatePopupMenu() else { continue };
                build_menu_into(sub, children, idcmdfirst, next_leaf, budget);
                let _ = AppendMenuW(
                    parent,
                    MF_POPUP | MF_STRING,
                    sub.0 as usize,
                    &HSTRING::from(crate::i18n::t(title)),
                );
            }
            verbs::MenuItem::Verb(title, _) => {
                if *next_leaf >= budget {
                    return;
                }
                let _ = AppendMenuW(
                    parent,
                    MF_STRING,
                    (idcmdfirst + *next_leaf) as usize,
                    &HSTRING::from(crate::i18n::t(title)),
                );
                *next_leaf += 1;
            }
            verbs::MenuItem::Separator => {
                // A divider — consumes no command id. (Skip a leading/trailing one
                // so we never start or end a (sub)menu with a stray separator.)
                let _ = AppendMenuW(parent, MF_SEPARATOR, 0, PCWSTR::null());
            }
        }
    }
}

impl ContextMenu {
    /// Handle WM_MEASUREITEM / WM_DRAWITEM forwarded by the shell for our
    /// owner-drawn preview item. Returns true when the message was ours.
    unsafe fn menu_msg(&self, umsg: u32, lparam: LPARAM) -> bool {
        let Some(cmd) = self.preview_cmd.get() else {
            return false;
        };
        match umsg {
            WM_MEASUREITEM => {
                let mis = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
                if mis.CtlType != ODT_MENU || mis.itemID != cmd {
                    return false;
                }
                let preview = self.preview.borrow();
                let Some(p) = preview.as_ref() else {
                    return false;
                };
                // Width fits the thumbnail and the (capped) caption; height adds
                // two text rows under the image. Kept tight so the menu doesn't
                // balloon (feedback: was far too wide).
                let text_w = caption_width_of(p);
                mis.itemWidth = (p.w.max(text_w).max(72) + 12) as u32;
                mis.itemHeight = (p.h + 48) as u32;
                true
            }
            WM_DRAWITEM => {
                let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
                if dis.CtlType != ODT_MENU || dis.itemID != cmd {
                    return false;
                }
                let preview = self.preview.borrow();
                let Some(p) = preview.as_ref() else {
                    return false;
                };
                draw_preview(dis, p);
                true
            }
            _ => false,
        }
    }

}

/// The actual menu font (`SPI_GETNONCLIENTMETRICS.lfMenuFont`, e.g. Segoe UI on
/// Win11), so the caption matches the surrounding menu items exactly — the stock
/// `DEFAULT_GUI_FONT` is an old, mismatched typeface. `Some` must be deleted by
/// the caller; `None` means "fall back to the stock GUI font" (do NOT delete).
unsafe fn menu_font() -> Option<HFONT> {
    let mut ncm = NONCLIENTMETRICSW {
        cbSize: core::mem::size_of::<NONCLIENTMETRICSW>() as u32,
        ..Default::default()
    };
    let ok = SystemParametersInfoW(
        SPI_GETNONCLIENTMETRICS,
        ncm.cbSize,
        Some(&mut ncm as *mut _ as *mut core::ffi::c_void),
        SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
    )
    .is_ok();
    if ok {
        let f = CreateFontIndirectW(&ncm.lfMenuFont);
        if !f.is_invalid() {
            return Some(f);
        }
    }
    None
}

/// Select the menu font (or the stock fallback) into `hdc`; returns the prior
/// font and the created font to delete (if any) after restoring.
unsafe fn select_menu_font(hdc: windows::Win32::Graphics::Gdi::HDC) -> (HGDIOBJ, Option<HFONT>) {
    let mf = menu_font();
    let h = mf.map(|f| f.0).unwrap_or_else(|| GetStockObject(DEFAULT_GUI_FONT).0);
    (SelectObject(hdc, HGDIOBJ(h)), mf)
}

/// Widest caption line in px (measured with the real menu font), capped.
unsafe fn caption_width_of(p: &Preview) -> i32 {
    let hdc = CreateCompatibleDC(None);
    let (old, mf) = select_menu_font(hdc);
    let mut max_w = 0i32;
    for line in [&p.name, &p.info] {
        let mut sz = SIZE::default();
        if !line.is_empty() && GetTextExtentPoint32W(hdc, line, &mut sz).as_bool() {
            max_w = max_w.max(sz.cx);
        }
    }
    SelectObject(hdc, old);
    if let Some(f) = mf {
        let _ = DeleteObject(f.into());
    }
    let _ = DeleteDC(hdc);
    max_w.min(CAPTION_MAX) // cap so an absurdly long name can't blow the menu up
}

/// Diagnostics: render the preview item to a PNG via the SAME `draw_preview`
/// path the menu uses, so it can be eyeballed without driving a real menu.
/// `bg` overrides the background (so light/dark menus can both be previewed);
/// pass `None` to use the live menu system color.
#[doc(hidden)]
pub fn render_preview_png(path: &str, out_png: &str, bg: Option<u32>) -> bool {
    unsafe {
        let Some(p) = build_preview(path) else {
            return false;
        };
        let text_w = caption_width_of(&p);
        let iw = p.w.max(text_w).max(72) + 12;
        let ih = p.h + 48;

        let mut bmi = windows::Win32::Graphics::Gdi::BITMAPINFO::default();
        bmi.bmiHeader.biSize = core::mem::size_of::<windows::Win32::Graphics::Gdi::BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = iw;
        bmi.bmiHeader.biHeight = -ih; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
        let Ok(dib) = windows::Win32::Graphics::Gdi::CreateDIBSection(
            None,
            &bmi,
            windows::Win32::Graphics::Gdi::DIB_RGB_COLORS,
            &mut bits,
            None,
            0,
        ) else {
            return false;
        };
        if bits.is_null() {
            let _ = DeleteObject(dib.into());
            return false;
        }
        let memdc = CreateCompatibleDC(None);
        let oldbmp = SelectObject(memdc, dib.into());

        let mut dis = DRAWITEMSTRUCT { hDC: memdc, ..Default::default() };
        dis.CtlType = ODT_MENU;
        dis.rcItem = RECT { left: 0, top: 0, right: iw, bottom: ih };
        match bg {
            Some(c) => {
                // Contrasting text for the chosen bg so we can preview both modes.
                let bright = ((c & 0xFF) + ((c >> 8) & 0xFF) + ((c >> 16) & 0xFF)) / 3;
                let fg = if bright > 128 { 0x0020_2020 } else { 0x00E0_E0E0 };
                paint_preview(&dis, &p, c, fg);
            }
            None => draw_preview(&dis, &p),
        }
        let _ = windows::Win32::Graphics::Gdi::GdiFlush();

        let n = (iw * ih * 4) as usize;
        let src = core::slice::from_raw_parts(bits as *const u8, n);
        let mut rgba = vec![0u8; n];
        for i in 0..(iw * ih) as usize {
            rgba[i * 4] = src[i * 4 + 2]; // R
            rgba[i * 4 + 1] = src[i * 4 + 1]; // G
            rgba[i * 4 + 2] = src[i * 4]; // B
            rgba[i * 4 + 3] = 255;
        }
        SelectObject(memdc, oldbmp);
        let _ = DeleteDC(memdc);
        let _ = DeleteObject(dib.into());

        image::RgbaImage::from_raw(iw as u32, ih as u32, rgba)
            .map(|b| b.save(out_png).is_ok())
            .unwrap_or(false)
    }
}

/// The Win11 classic context menu follows the **System** (Windows) theme — and
/// legacy `GetSysColor(COLOR_MENU)` does NOT update for dark mode (it always
/// returns the light gray), so a dark menu would get a glaring white preview
/// block. Detect the real menu theme from the registry instead.
fn menu_dark() -> bool {
    windows_registry::CURRENT_USER
        .open(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize")
        .and_then(|k| k.get_u32("SystemUsesLightTheme"))
        .map(|v| v == 0)
        .unwrap_or(false)
}

/// Paint using colors that match the actual menu (light or dark), with the
/// accent highlight when hovered.
unsafe fn draw_preview(dis: &DRAWITEMSTRUCT, p: &Preview) {
    let selected = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    let (bg, fg) = if selected {
        (GetSysColor(COLOR_HIGHLIGHT), GetSysColor(COLOR_HIGHLIGHTTEXT))
    } else if menu_dark() {
        (0x002B_2B2B, 0x00E0_E0E0) // Win11 dark flyout bg + light text
    } else {
        (GetSysColor(COLOR_MENU), GetSysColor(COLOR_MENUTEXT))
    };
    paint_preview(dis, p, bg, fg);
}

/// Paint the preview item: thumbnail centered on top, name + info lines under,
/// with explicit `bg`/`fg` colors (so the probe can preview any palette).
unsafe fn paint_preview(dis: &DRAWITEMSTRUCT, p: &Preview, bg: u32, fg: u32) {
    let rc = dis.rcItem;
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(dis.hDC, &rc, brush);
    let _ = DeleteObject(brush.into());

    // Thumbnail, horizontally centered.
    let bx = rc.left + ((rc.right - rc.left) - p.w) / 2;
    let by = rc.top + 4;
    let mem = CreateCompatibleDC(Some(dis.hDC));
    let old = SelectObject(mem, p.hbm.into());
    let bf = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    let _ = AlphaBlend(dis.hDC, bx, by, p.w, p.h, mem, 0, 0, p.w, p.h, bf);
    SelectObject(mem, old);
    let _ = DeleteDC(mem);

    // Caption lines, in the menu's own font + text color so they match the
    // surrounding items (both legible — no dim grey).
    SetBkMode(dis.hDC, TRANSPARENT);
    let (oldf, mf) = select_menu_font(dis.hDC);
    SetTextColor(dis.hDC, COLORREF(fg));

    let mut name = p.name.clone();
    let mut line1 = RECT { left: rc.left + 6, top: by + p.h + 2, right: rc.right - 6, bottom: by + p.h + 20 };
    DrawTextW(dis.hDC, &mut name, &mut line1, DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS);

    let mut info = p.info.clone();
    let mut line2 = RECT { left: rc.left + 6, top: line1.bottom + 1, right: rc.right - 6, bottom: line1.bottom + 19 };
    DrawTextW(dis.hDC, &mut info, &mut line2, DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS);

    SelectObject(dis.hDC, oldf);
    if let Some(f) = mf {
        let _ = DeleteObject(f.into());
    }
}

/// Open the file with its default app (the preview item's click action).
fn open_with_default(path: &str) {
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        );
    }
}

impl IShellExtInit_Impl for ContextMenu_Impl {
    fn Initialize(
        &self,
        _pidlfolder: *const ITEMIDLIST,
        pdtobj: Ref<'_, IDataObject>,
        _hkeyprogid: HKEY,
    ) -> Result<()> {
        safety::guard(|| {
            let obj = pdtobj.ok()?;
            let paths = unsafe { hdrop_paths(obj)? };
            *self.paths.borrow_mut() = paths;
            Ok(())
        })
    }
}

impl IContextMenu_Impl for ContextMenu_Impl {
    fn QueryContextMenu(
        &self,
        hmenu: HMENU,
        indexmenu: u32,
        idcmdfirst: u32,
        idcmdlast: u32,
        uflags: u32,
    ) -> HRESULT {
        safety::guard_hr(|| {
            if uflags & CMF_DEFAULTONLY != 0 {
                return S_OK; // no default action to add
            }
            if !settings::menu_enabled() {
                return S_OK; // menu disabled in Options
            }
            let paths = self.paths.borrow();
            if !paths.iter().any(|p| verbs::is_image(p)) {
                return S_OK; // nothing for non-image selections
            }
            // Honor the shell's allotted command-id range [idcmdfirst, idcmdlast]
            // (inclusive). Each leaf consumes one id; popup parents do not.
            // Overflowing the range can collide with a neighboring handler's ids
            // and misdispatch a click — so clamp to what we're allowed.
            let avail = (idcmdlast as usize)
                .saturating_sub(idcmdfirst as usize)
                .saturating_add(1);
            let leaves_n = verbs::leaves().len();
            let budget = leaves_n.min(avail) as u32;
            if budget == 0 {
                return S_OK;
            }

            // Menu preview: single image selection, enabled in Options, and the
            // id range has room for one extra command (offset = leaves_n, so the
            // InvokeCommand mapping stays stable even if leaves were clamped).
            self.preview_cmd.set(None);
            *self.preview.borrow_mut() = None;
            let mode = settings::menu_preview();
            let single = paths.len() == 1 && verbs::is_image(&paths[0]);
            if mode != 0 && single && avail > leaves_n {
                if let Some(p) = build_preview(&paths[0]) {
                    *self.preview.borrow_mut() = Some(p);
                    self.preview_cmd.set(Some(idcmdfirst + leaves_n as u32));
                }
            }

            unsafe {
                // Our items grow downward from `indexmenu`: [preview?] [quick groups?]
                // [the "SageThumbs 2K" submenu].
                let mut pos = indexmenu;

                // 1) Preview directly on the main menu (mode 2), topmost.
                if let Some(cmd) = self.preview_cmd.get() {
                    if mode == 2 {
                        let _ = InsertMenuW(hmenu, pos, MF_BYPOSITION | MF_OWNERDRAW, cmd as usize, PCWSTR::null());
                        pos += 1;
                    }
                }

                // 2) Quick-verb groups directly on the main menu (Options toggle),
                //    below the preview. Each is built starting at its GLOBAL leaf
                //    index, so it reuses the submenu's command ids — a click on
                //    either copy invokes the same action and we claim no extra ids.
                if settings::menu_quick_verbs() {
                    for item in verbs::quick_items() {
                        match item {
                            verbs::QuickItem::Group(title, children, start) => {
                                let Ok(qsub) = CreatePopupMenu() else { continue };
                                let mut n = start;
                                build_menu_into(qsub, children, idcmdfirst, &mut n, budget);
                                let _ = InsertMenuW(
                                    hmenu,
                                    pos,
                                    MF_BYPOSITION | MF_POPUP | MF_STRING,
                                    qsub.0 as usize,
                                    &HSTRING::from(crate::i18n::t(title)),
                                );
                                pos += 1;
                            }
                            verbs::QuickItem::Leaf(title, idx) => {
                                // A top-level leaf reusing its submenu command id.
                                if idx < budget {
                                    let _ = InsertMenuW(
                                        hmenu,
                                        pos,
                                        MF_BYPOSITION | MF_STRING,
                                        (idcmdfirst + idx) as usize,
                                        &HSTRING::from(crate::i18n::t(title)),
                                    );
                                    pos += 1;
                                }
                            }
                        }
                    }
                }

                // 3) The full "SageThumbs 2K" submenu (preview at its top in mode 1).
                let Ok(hsub) = CreatePopupMenu() else {
                    return E_FAIL;
                };
                if let Some(cmd) = self.preview_cmd.get() {
                    if mode == 1 {
                        let _ = AppendMenuW(hsub, MF_OWNERDRAW, cmd as usize, PCWSTR::null());
                        let _ = AppendMenuW(hsub, MF_SEPARATOR, 0, PCWSTR::null());
                    }
                }
                let mut next_leaf = 0u32;
                build_menu_into(hsub, verbs::MENU, idcmdfirst, &mut next_leaf, budget);
                let _ = InsertMenuW(
                    hmenu,
                    pos,
                    MF_BYPOSITION | MF_POPUP | MF_STRING,
                    hsub.0 as usize,
                    &HSTRING::from("SageThumbs 2K"),
                );
                // Brand icon in front of "SageThumbs 2K" (hbmpItem, alpha-blended).
                let logo = menu_logo();
                if !logo.is_invalid() {
                    let mii = MENUITEMINFOW {
                        cbSize: core::mem::size_of::<MENUITEMINFOW>() as u32,
                        fMask: MIIM_BITMAP,
                        hbmpItem: logo,
                        ..Default::default()
                    };
                    let _ = SetMenuItemInfoW(hmenu, pos, true, &mii);
                }
                // Command ids consumed: the leaves, plus the preview slot. The quick
                // groups reuse leaf ids, so they add nothing.
                let consumed = if self.preview_cmd.get().is_some() {
                    leaves_n as u32 + 1
                } else {
                    next_leaf
                };
                HRESULT(consumed as i32)
            }
        })
    }

    fn InvokeCommand(&self, pici: *const CMINVOKECOMMANDINFO) -> Result<()> {
        safety::guard(|| {
            let pici = unsafe { pici.as_ref().ok_or_else(|| Error::from(E_FAIL))? };
            let lp = pici.lpVerb.0 as usize;
            if (lp >> 16) != 0 {
                return Err(Error::from(E_FAIL)); // string verb, not the offset form
            }
            let offset = lp & 0xFFFF;
            let leaves = verbs::leaves();
            if offset == leaves.len() && self.preview_cmd.get().is_some() {
                // The preview thumbnail itself: open the image.
                if let Some(p) = self.paths.borrow().first() {
                    open_with_default(p);
                }
                return Ok(());
            }
            let &(_, action) = leaves.get(offset).ok_or_else(|| Error::from(E_FAIL))?;
            let paths = self.paths.borrow().clone();
            verbs::run_action(action, &paths);
            Ok(())
        })
    }

    fn GetCommandString(
        &self,
        _idcmd: usize,
        _utype: u32,
        _reserved: *const u32,
        _pszname: PSTR,
        _cchmax: u32,
    ) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
}

impl IContextMenu2_Impl for ContextMenu_Impl {
    fn HandleMenuMsg(&self, umsg: u32, _wparam: WPARAM, lparam: LPARAM) -> Result<()> {
        safety::guard(|| {
            unsafe { self.menu_msg(umsg, lparam) };
            Ok(())
        })
    }
}

impl IContextMenu3_Impl for ContextMenu_Impl {
    fn HandleMenuMsg2(
        &self,
        umsg: u32,
        _wparam: WPARAM,
        lparam: LPARAM,
        plresult: *mut LRESULT,
    ) -> Result<()> {
        safety::guard(|| {
            let handled = unsafe { self.menu_msg(umsg, lparam) };
            if !plresult.is_null() {
                // Measure/draw return TRUE when handled.
                unsafe { *plresult = LRESULT(handled as isize) };
            }
            Ok(())
        })
    }
}
