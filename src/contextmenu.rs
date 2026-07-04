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
    AlphaBlend, CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, CreateSolidBrush,
    DeleteDC, DeleteObject, DrawTextW, FillRect, GdiFlush, GetStockObject, GetSysColor,
    GetTextExtentPoint32W, SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER,
    BITMAPINFO, BITMAPINFOHEADER, BLENDFUNCTION, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_MENU,
    COLOR_MENUTEXT, DEFAULT_GUI_FONT, DIB_RGB_COLORS, DT_CENTER, DT_END_ELLIPSIS, DT_SINGLELINE,
    HBITMAP, HDC, HFONT, HGDIOBJ, TRANSPARENT,
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
    // ModuleRef::default()'s side effect (live-object add-ref) must run; keep the Default call.
    #[allow(clippy::default_constructed_unit_structs)]
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
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    };
    let mut medium = obj.GetData(&fmt)?;
    // GetData can hand back a different storage medium than we asked for; using
    // the hGlobal union field on a non-HGLOBAL medium would read a bogus handle.
    if medium.tymed != TYMED_HGLOBAL.0 as u32 {
        ReleaseStgMedium(&mut medium);
        return Err(E_FAIL.into());
    }
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

/// Cheap pre-gate (metadata only, NO read/decode): the file exists and is within
/// the preview size budget. `QueryContextMenu` calls this to decide whether to
/// RESERVE the owner-draw preview slot, deferring the actual decode to the first
/// `WM_MEASUREITEM` (see [`ContextMenu::ensure_preview`]) — so a slow/large file
/// never blocks the menu from painting (the confirmed right-click stall).
fn preview_size_ok(path: &str) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() <= PREVIEW_MAX_BYTES && m.len() <= settings::max_file_size_bytes())
        .unwrap_or(false)
}

/// The `Send` result of the off-thread menu-preview decode: the scaled RGBA thumbnail (the GDI
/// DIB is created on the caller's UI thread) plus the file's true source dimensions.
struct MenuThumb {
    rgba: Vec<u8>,
    w: i32,
    h: i32,
    ow: u32,
    oh: u32,
}

/// Wall-clock budget for the off-thread menu-preview decode. Normal files decode well under
/// this; the cap exists so a slow-but-in-cap image (complex HEIC/AVIF, a 16384² file) can't
/// freeze the menu's first paint for longer than this before falling back to caption-only.
const MENU_PREVIEW_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);

/// Read + decode `path` to a scaled menu thumbnail on a DETACHED worker under a budget, so the
/// decode never blocks explorer.exe's menu paint thread past [`MENU_PREVIEW_BUDGET`]. Mirrors
/// `propstore::probe_budgeted` / `decode_svg`: the worker holds a `crate::ModuleRef` and inits
/// COM (the WIC HEIC/AVIF/RAW tier needs an apartment); on timeout it finishes + exits on its
/// own and the caller degrades to a caption-only tile. Uses ONLY the cheap in-process tiers
/// (`decode_menu_preview` — no magick/video/pdf/svg), so the worker is fast and bundled-byte-free.
fn decode_menu_thumb_budgeted(path: &str) -> Option<MenuThumb> {
    let path = path.to_string();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        #[allow(clippy::default_constructed_unit_structs)]
        let _module = crate::ModuleRef::default();
        let inited = unsafe {
            windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            )
        }
        .is_ok();
        let out = (|| {
            let bytes = std::fs::read(&path).ok()?;
            let img = crate::decode::decode_menu_preview(&bytes).ok()?;
            let (ow, oh) =
                crate::container::real_dims(&bytes).unwrap_or((img.width(), img.height()));
            // Width up to PREVIEW_WIDE, height up to PREVIEW_BOX: wide images render wide,
            // normal/tall ones stay capped at the 88px height.
            let thumb = img.thumbnail(PREVIEW_WIDE, PREVIEW_BOX);
            let rgba = thumb.to_rgba8();
            let (w, h) = (rgba.width() as i32, rgba.height() as i32);
            Some(MenuThumb { rgba: rgba.into_raw(), w, h, ow, oh })
        })();
        if inited {
            unsafe { windows::Win32::System::Com::CoUninitialize() };
        }
        let _ = tx.send(out);
    });
    rx.recv_timeout(MENU_PREVIEW_BUDGET).ok().flatten()
}

/// Decode `path` into the menu-preview payload (thumbnail DIB + caption lines).
/// Called LAZILY from the owner-draw measure/draw path, not from `QueryContextMenu`,
/// so the decode happens as the (sub)menu paints rather than before it opens.
fn build_preview(path: &str) -> Option<Preview> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > PREVIEW_MAX_BYTES || meta.len() > settings::max_file_size_bytes() {
        return None;
    }
    let name: Vec<u16> = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .encode_utf16()
        .collect();
    let kb = meta.len() as f64 / 1024.0;
    let size_txt = if kb >= 1024.0 {
        format!("{:.1} MB", kb / 1024.0)
    } else {
        format!("{kb:.0} KB")
    };

    // Preview fidelity ONLY. The classic context menu loads in explorer.exe (it is
    // NOT process-isolated the way the thumbnail provider is), so we keep this
    // bounded: the baked-in container preview is plenty for an ~88px menu
    // thumbnail and avoids spawning ImageMagick / full-fidelity decode inside the
    // shell. Caption dimensions come from a cheap header probe (PSD/PSB real
    // canvas) so a 4700×800 PSD doesn't read "160 × 26 px" from its thumbnail.
    //
    // On decode failure (a corrupt or in-practice-undecodable file) fall back to a
    // CAPTION-ONLY tile (name + size, no thumbnail): the owner-draw slot was already
    // reserved in QueryContextMenu, so a name+size row degrades more gracefully than
    // a blank gap. `null` hbm + 0×0 are handled by `paint_preview`.
    // Decode OFF explorer's menu paint thread under a wall-clock budget (the in-proc-COM rule):
    // the cheap tiers are fast on normal files, but a large HEIC/RAW or a 16384² in-cap image
    // has no internal TIME bound and this would otherwise run on the menu's own paint thread.
    // The DIB (a GDI object) is created HERE from the worker's plain-RGBA result; only the
    // decode (the slow part) is offloaded. On timeout -> caption-only tile (handled below).
    let decoded = decode_menu_thumb_budgeted(path).and_then(|t| {
        let hbm = unsafe { crate::dib::create_premultiplied_dib(t.w, t.h, &t.rgba).ok()? };
        Some((hbm, t.w, t.h, t.ow, t.oh))
    });
    let (hbm, w, h, info) = match decoded {
        Some((hbm, w, h, ow, oh)) => (hbm, w, h, format!("{ow} \u{00d7} {oh} px  \u{2013}  {size_txt}")),
        None => (HBITMAP::default(), 0, 0, size_txt),
    };
    Some(Preview {
        hbm,
        w,
        h,
        name,
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
    vis: &settings::MenuVisibility,
) {
    for it in items {
        // Per-item visibility: a hidden top-level item is skipped from the drawn
        // menu but still advances the leaf counter, so command ids stay aligned
        // with the full tree. (Child keys are never in the toggle set, so they
        // always pass — only top-level toggles can hide; separators have an empty
        // title which is never hidden.) `vis` is a single snapshot of the subkey,
        // so this is one read per item, not a key-open.
        if !vis.shown(it.title()) {
            *next_leaf += verbs::count_leaves(it);
            continue;
        }
        match it {
            verbs::MenuItem::Group(title, children) => {
                let Ok(sub) = CreatePopupMenu() else { continue };
                build_menu_into(sub, children, idcmdfirst, next_leaf, budget, vis);
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
                // The leaf's command id is its global leaf index, mapped through
                // the central id_for() so the offset convention lives in one place.
                let cmd = verbs::id_for(verbs::CmdSlot::Leaf(verbs::LeafId(*next_leaf)), idcmdfirst);
                let _ = AppendMenuW(
                    parent,
                    MF_STRING,
                    cmd as usize,
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
    /// Build the preview on first demand. The decode is DEFERRED out of
    /// `QueryContextMenu` to here (the owner-draw measure/draw path), so it never
    /// blocks the menu's first paint — in submenu mode the file isn't even read
    /// unless the user opens the SageThumbs flyout. Idempotent: builds at most once,
    /// caching into `self.preview`. Returns whether a preview is now available.
    unsafe fn ensure_preview(&self) -> bool {
        if self.preview.borrow().is_some() {
            return true;
        }
        let path = self.paths.borrow().first().cloned();
        if let Some(path) = path {
            if let Some(p) = build_preview(&path) {
                *self.preview.borrow_mut() = Some(p);
                return true;
            }
        }
        false
    }

    /// Handle WM_MEASUREITEM / WM_DRAWITEM forwarded by the shell for our
    /// owner-drawn preview item. Returns true when the message was ours.
    ///
    /// The preview is owner-drawn (the ONLY way Windows lets a menu item be tall
    /// enough for an image). The trade — confirmed empirically — is that an
    /// owner-drawn item makes Win11 render the whole "Show more options" popup in
    /// the classic LIGHT style. So the preview is opt-in (MenuPreview != 0); with
    /// it off, we add no owner-drawn item and the menu stays dark/native.
    unsafe fn menu_msg(&self, umsg: u32, lparam: LPARAM) -> bool {
        let Some(cmd) = self.preview_cmd.get() else {
            return false;
        };
        // The shell always passes a valid struct pointer here, but guard anyway:
        // a null lparam would make the &*/&mut * casts below instant UB.
        if lparam.0 == 0 {
            return false;
        }
        match umsg {
            WM_MEASUREITEM => {
                let mis = &mut *(lparam.0 as *mut MEASUREITEMSTRUCT);
                if mis.CtlType != ODT_MENU || mis.itemID != cmd {
                    return false;
                }
                // Lazily decode now (first measure, as the menu paints). If it fails
                // — the file vanished/changed between QueryContextMenu and the paint —
                // measure a minimal slot so the reserved item has a valid size and
                // simply draws blank (rare edge).
                if !self.ensure_preview() {
                    mis.itemWidth = 1;
                    mis.itemHeight = 1;
                    return true;
                }
                let preview = self.preview.borrow();
                let Some(p) = preview.as_ref() else {
                    return false;
                };
                // Width fits the thumbnail and the (capped) caption; height adds
                // two text rows under the image.
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
                // Measure runs before draw and builds the preview; ensure it anyway.
                if !self.ensure_preview() {
                    return true; // nothing to draw (rare: lazy build failed)
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

/// The menu font, created once per process and never freed — the owner-draw
/// callbacks (WM_MEASUREITEM/WM_DRAWITEM) hit this on every paint, and a live
/// menu may reference it for the host's lifetime. Same never-free rationale as
/// [`menu_logo`]; the classic-menu host is short-lived. Falls back to
/// [`menu_font`] on the cache-miss path and to the stock GUI font if even that
/// fails. Returns an HFONT the caller must NOT delete.
fn menu_font_cached() -> HFONT {
    use std::sync::OnceLock;
    static FONT: OnceLock<isize> = OnceLock::new();
    let h = *FONT.get_or_init(|| {
        unsafe { menu_font() }
            .map(|f| f.0 as isize)
            .unwrap_or_else(|| unsafe { GetStockObject(DEFAULT_GUI_FONT) }.0 as isize)
    });
    HFONT(h as *mut core::ffi::c_void)
}

/// Select the cached menu font into `hdc`; returns the prior font to restore.
/// The font is process-cached (never freed), so there is nothing to delete —
/// unlike the old per-call font, callers must NOT delete the returned font.
unsafe fn select_menu_font(hdc: windows::Win32::Graphics::Gdi::HDC) -> HGDIOBJ {
    SelectObject(hdc, HGDIOBJ(menu_font_cached().0))
}

/// Widest caption line in px (measured with the real menu font), capped.
unsafe fn caption_width_of(p: &Preview) -> i32 {
    let hdc = CreateCompatibleDC(None);
    let old = select_menu_font(hdc);
    let mut max_w = 0i32;
    for line in [&p.name, &p.info] {
        let mut sz = SIZE::default();
        if !line.is_empty() && GetTextExtentPoint32W(hdc, line, &mut sz).as_bool() {
            max_w = max_w.max(sz.cx);
        }
    }
    SelectObject(hdc, old);
    let _ = DeleteDC(hdc);
    max_w.min(CAPTION_MAX) // cap so an absurdly long name can't blow the menu up
}

/// Diagnostics: render the preview tile to a PNG via the SAME compositing path
/// the menu uses (`paint_preview`), so it can be eyeballed without driving a real
/// menu. `bg` overrides the background (so light/dark menus can both be
/// previewed); pass `None` to use the live menu theme colors.
#[doc(hidden)]
pub fn render_preview_png(path: &str, out_png: &str, bg: Option<u32>) -> bool {
    unsafe {
        let Some(p) = build_preview(path) else {
            return false;
        };
        let text_w = caption_width_of(&p);
        let iw = p.w.max(text_w).max(72) + 12;
        let ih = p.h + 48;

        let (cbg, cfg) = match bg {
            Some(c) => {
                // Contrasting text for the chosen bg so we can preview both modes.
                let bright = ((c & 0xFF) + ((c >> 8) & 0xFF) + ((c >> 16) & 0xFF)) / 3;
                let fg = if bright > 128 { 0x0020_2020 } else { 0x00E0_E0E0 };
                (c, fg)
            }
            None => menu_theme_colors(),
        };

        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader.biSize = core::mem::size_of::<BITMAPINFOHEADER>() as u32;
        bmi.bmiHeader.biWidth = iw;
        bmi.bmiHeader.biHeight = -ih; // top-down
        bmi.bmiHeader.biPlanes = 1;
        bmi.bmiHeader.biBitCount = 32;
        let mut bits: *mut core::ffi::c_void = core::ptr::null_mut();
        let Ok(dib) = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
            return false;
        };
        if bits.is_null() {
            let _ = DeleteObject(dib.into());
            return false;
        }
        let memdc = CreateCompatibleDC(None);
        let oldbmp = SelectObject(memdc, dib.into());

        paint_preview(memdc, RECT { left: 0, top: 0, right: iw, bottom: ih }, &p, cbg, cfg);
        let _ = GdiFlush();

        // Compute the byte count in usize with checked math — `(iw * ih * 4) as usize` multiplies
        // as i32 first and could overflow into an undersized length for the `from_raw_parts` below
        // (unsound). In practice the dims are tiny preview sizes, but bail cleanly on anything absurd.
        let Some(n) = (iw as usize).checked_mul(ih as usize).and_then(|p| p.checked_mul(4)) else {
            SelectObject(memdc, oldbmp);
            let _ = DeleteDC(memdc);
            let _ = DeleteObject(dib.into());
            return false;
        };
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

/// The (bg, fg) baked into the preview tile so it matches the surrounding menu.
/// The Win11 dark flyout is ~#2B2B2B with near-white text; light menus use the
/// system menu colors. We bake an OPAQUE tile (we can't read the live acrylic
/// tone, so a flat match), which is the trade for not owner-drawing.
unsafe fn menu_theme_colors() -> (u32, u32) {
    if menu_dark() {
        (0x002B_2B2B, 0x00E0_E0E0) // Win11 dark flyout bg + light text
    } else {
        (GetSysColor(COLOR_MENU), GetSysColor(COLOR_MENUTEXT))
    }
}

/// Two subtle checkerboard shades from a base menu colour: the base nudged a few
/// levels darker and a few lighter. Their average stays ≈ `bg` (so the menu tone
/// doesn't shift) and they sit only ~16 levels apart — enough to read as
/// "transparency here" without competing with the menu. Follows light/dark/accent
/// automatically since it's derived from whatever `bg` is passed.
fn checker_shades(bg: u32) -> (u32, u32) {
    let ch = |shift: u32| (bg >> shift) & 0xFF; // COLORREF is 0x00BBGGRR
    let (r, g, b) = (ch(0), ch(8), ch(16));
    let darker = |c: u32| c.saturating_sub(8);
    let lighter = |c: u32| (c + 8).min(255);
    let pack = |r: u32, g: u32, b: u32| r | (g << 8) | (b << 16);
    (pack(darker(r), darker(g), darker(b)), pack(lighter(r), lighter(g), lighter(b)))
}

/// Fill the rect `(left,top)`–`(left+w,top+h)` with an 8px two-tone checkerboard
/// — the backdrop the preview thumbnail is alpha-blended onto, so transparent
/// pixels reveal the pattern instead of disappearing into the flat menu colour.
unsafe fn fill_checker(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    left: i32,
    top: i32,
    w: i32,
    h: i32,
    c0: u32,
    c1: u32,
) {
    const CELL: i32 = 8;
    let b0 = CreateSolidBrush(COLORREF(c0));
    let b1 = CreateSolidBrush(COLORREF(c1));
    FillRect(hdc, &RECT { left, top, right: left + w, bottom: top + h }, b0);
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            if ((x / CELL) + (y / CELL)) & 1 == 1 {
                let r = RECT {
                    left: left + x,
                    top: top + y,
                    right: left + (x + CELL).min(w),
                    bottom: top + (y + CELL).min(h),
                };
                FillRect(hdc, &r, b1);
            }
            x += CELL;
        }
        y += CELL;
    }
    let _ = DeleteObject(b0.into());
    let _ = DeleteObject(b1.into());
}

/// Paint the preview into `rc` of `hdc`: thumbnail centered on top, name + info
/// lines under, with explicit `bg`/`fg` colors. Used both by the off-screen
/// compositor ([`preview_hbitmap`]) and the diagnostic PNG renderer.
unsafe fn paint_preview(hdc: HDC, rc: RECT, p: &Preview, bg: u32, fg: u32) {
    let brush = CreateSolidBrush(COLORREF(bg));
    FillRect(hdc, &rc, brush);
    let _ = DeleteObject(brush.into());

    // Thumbnail, horizontally centered. Skipped entirely for the caption-only
    // fallback tile (null bitmap / 0×0 — a file that passed the size gate but failed
    // to decode), which shows just the name + size rows below.
    let bx = rc.left + ((rc.right - rc.left) - p.w) / 2;
    let by = rc.top + 4;
    if !p.hbm.is_invalid() && p.w > 0 && p.h > 0 {
        // Subtle checkerboard behind the thumbnail so transparent images stay visible
        // against the flat menu colour (default on; toggleable in Settings).
        if settings::preview_checker() {
            let (c0, c1) = checker_shades(bg);
            fill_checker(hdc, bx, by, p.w, p.h, c0, c1);
        }
        let mem = CreateCompatibleDC(Some(hdc));
        let old = SelectObject(mem, p.hbm.into());
        let bf = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        let _ = AlphaBlend(hdc, bx, by, p.w, p.h, mem, 0, 0, p.w, p.h, bf);
        SelectObject(mem, old);
        let _ = DeleteDC(mem);
    }

    // Caption lines, in the menu's own font + text color so they match the
    // surrounding items (both legible — no dim grey).
    SetBkMode(hdc, TRANSPARENT);
    let oldf = select_menu_font(hdc);
    SetTextColor(hdc, COLORREF(fg));

    let mut name = p.name.clone();
    let mut line1 = RECT { left: rc.left + 6, top: by + p.h + 2, right: rc.right - 6, bottom: by + p.h + 20 };
    DrawTextW(hdc, &mut name, &mut line1, DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS);

    let mut info = p.info.clone();
    let mut line2 = RECT { left: rc.left + 6, top: line1.bottom + 1, right: rc.right - 6, bottom: line1.bottom + 19 };
    DrawTextW(hdc, &mut info, &mut line2, DT_CENTER | DT_SINGLELINE | DT_END_ELLIPSIS);

    SelectObject(hdc, oldf);
}

/// Owner-draw painter for the preview item (WM_DRAWITEM). The menu follows the
/// system theme, so we paint the tile to match: the Win11 dark flyout colors when
/// the system is dark (legacy `GetSysColor(COLOR_MENU)` would wrongly stay light),
/// the system menu colors when light, and the accent when hovered.
unsafe fn draw_preview(dis: &DRAWITEMSTRUCT, p: &Preview) {
    let selected = (dis.itemState.0 & ODS_SELECTED.0) != 0;
    let (bg, fg) = if selected {
        (GetSysColor(COLOR_HIGHLIGHT), GetSysColor(COLOR_HIGHLIGHTTEXT))
    } else if menu_dark() {
        (0x002B_2B2B, 0x00E0_E0E0) // Win11 dark flyout bg + light text
    } else {
        (GetSysColor(COLOR_MENU), GetSysColor(COLOR_MENUTEXT))
    };
    paint_preview(dis.hDC, dis.rcItem, p, bg, fg);
}

/// Open the file with its default app (the preview item's click action).
fn open_with_default(path: &str) {
    let wide = crate::wide(path);
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
            // Computed once and reused for the single-image preview gate below (for a
            // 1-file selection this `.any` IS `is_image(paths[0])`), so we don't probe
            // the same path's extension twice per right-click.
            let any_image = paths.iter().any(|p| verbs::is_image(p));
            // "Show on all file types": on an UNSUPPORTED selection, fall through to a
            // CONDENSED menu (file-agnostic utilities only) when the user opted in;
            // otherwise add nothing, as before.
            let condensed = !any_image;
            if condensed && !settings::menu_all_file_types() {
                return S_OK; // nothing for non-image selections
            }
            // An AUDIO-only selection (supported, so not condensed, but every file is a
            // music file) gets the audio view: the image-only verbs (Convert/Resize/
            // Rotate/Wallpaper/…) no-op or produce garbage on sound, so we drop them and
            // show only the audio-relevant set (Files to folder · Rename ▸ · Sort ▸ +
            // Settings). `all()` is false for an empty selection, but `condensed` already
            // is true then, so `!condensed` guards that.
            let audio_only = !condensed && paths.iter().all(|p| verbs::is_audio(p));
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
            // For a 1-file selection, `any_image` already == is_image(paths[0]).
            let single = paths.len() == 1 && any_image;
            // Reserve the owner-drawn preview slot when one is wanted and the file
            // passes the CHEAP metadata size gate — but DEFER the actual decode to the
            // first WM_MEASUREITEM (see `ensure_preview`). Decoding here is what blocked
            // the menu from painting (the confirmed right-click stall); in mode 1 the
            // file isn't read at all unless the user opens the SageThumbs submenu.
            if mode != 0 && single && avail > leaves_n && preview_size_ok(&paths[0]) {
                // The preview occupies the slot just past the last leaf;
                // id_for(Preview) encapsulates that "== leaves.len()" convention.
                self.preview_cmd.set(Some(verbs::id_for(verbs::CmdSlot::Preview, idcmdfirst)));
            }

            unsafe {
                // One snapshot of the menu-item visibility subkey for this whole
                // build (the quick-verb loop + every build_menu_into node share it),
                // so a right-click does ONE key-open instead of one per item.
                let vis = settings::menu_visibility();

                // Our items grow downward from `indexmenu`: [preview?] [quick groups?]
                // [the "SageThumbs 2K" submenu] — all cohesive, in one place. (We ship
                // ONLY this classic handler now, not the packaged modern command, so the
                // menu can't double-list "SageThumbs 2K" — see AppxManifest.xml / register.rs.)
                let mut pos = indexmenu;

                // 1) Preview directly on the main menu (mode 2), topmost. Owner-drawn,
                //    so the tile can be tall enough for the image — the trade is that
                //    an owner-drawn item makes Win11 render this popup in the classic
                //    LIGHT style. That's why the preview is opt-in (MenuPreview); with
                //    it off (mode 0) we add no owner-drawn item and the menu stays dark.
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
                // The quick verbs are Convert/Resize/Rotate — all image-only, so they're
                // suppressed for an audio-only selection too (same reason the audio view
                // drops them from the submenu below).
                //    SUPPRESSED when the signed sparse package is active: it declares the SAME
                //    quick verbs as modern IExplorerCommand commands, and Windows bridges those
                //    DOWN into this legacy "Show more options" menu. Emitting our copies too
                //    would double-list Convert/Resize/Rotate here, so we defer to the bridged
                //    packaged verbs (settings::modern_menu_active + packaging/AppxManifest.xml).
                //    The full "SageThumbs 2K" flyout below is unaffected — still listed once.
                if settings::menu_quick_verbs()
                    && !condensed
                    && !audio_only
                    && !settings::modern_menu_active()
                {
                    for item in verbs::quick_items() {
                        // Honor per-item visibility: a hidden top-level item drops
                        // its quick-verb copy from the main menu too.
                        let qtitle = match &item {
                            verbs::QuickItem::Group(t, _, _) => *t,
                            verbs::QuickItem::Leaf(t, _) => *t,
                        };
                        if !vis.shown(qtitle) {
                            continue;
                        }
                        match item {
                            verbs::QuickItem::Group(title, children, start) => {
                                let Ok(qsub) = CreatePopupMenu() else { continue };
                                let mut n = start;
                                build_menu_into(qsub, children, idcmdfirst, &mut n, budget, &vis);
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
                                // A top-level leaf reusing its submenu command id:
                                // same global leaf index → same id_for() id → same action.
                                if idx < budget {
                                    let cmd = verbs::id_for(
                                        verbs::CmdSlot::Leaf(verbs::LeafId(idx)),
                                        idcmdfirst,
                                    );
                                    let _ = InsertMenuW(
                                        hmenu,
                                        pos,
                                        MF_BYPOSITION | MF_STRING,
                                        cmd as usize,
                                        &HSTRING::from(crate::i18n::t(title)),
                                    );
                                    pos += 1;
                                }
                            }
                        }
                    }
                    // No divider here: the quick verbs flow straight into the
                    // "SageThumbs 2K" entry so the whole group reads as one block.
                    // The separator goes BELOW that entry instead (see section 3).
                }

                // 3) The full "SageThumbs 2K" submenu, directly below the preview +
                // quick verbs (preview at its top in mode 1). This is the brand entry
                // with every verb + Settings — kept cohesive with the preview above it,
                // never "off on its own." We ship ONLY this classic handler (no packaged
                // modern command), so "SageThumbs 2K" is listed exactly once.
                if let Ok(hsub) = CreatePopupMenu() {
                    if let Some(cmd) = self.preview_cmd.get() {
                        if mode == 1 {
                            // Owner-drawn preview at the top of the flyout, then a
                            // divider before the verbs. (Owner-draw → light menu;
                            // see the mode-2 note above and menu_msg.)
                            let _ = AppendMenuW(hsub, MF_OWNERDRAW, cmd as usize, PCWSTR::null());
                            let _ = AppendMenuW(hsub, MF_SEPARATOR, 0, PCWSTR::null());
                        }
                    }
                    // Build the top-level items in the user's saved order (drag-to-
                    // reorder in Settings). Each item keeps its ORIGINAL leaf-start
                    // index, so command ids stay stable — the dispatch side reads the
                    // default leaves()/slot_for, so only the insertion order changes.
                    // Full custom-ordered tree for a supported image selection; the
                    // audio-only set for a music selection; the condensed file-agnostic
                    // set for an unsupported one (show-on-all-file-types).
                    let top = if condensed {
                        verbs::condensed_top_level()
                    } else if audio_only {
                        verbs::audio_top_level()
                    } else {
                        verbs::ordered_top_level()
                    };
                    for (item, start_leaf) in top {
                        let mut leaf = start_leaf;
                        build_menu_into(hsub, std::slice::from_ref(item), idcmdfirst, &mut leaf, budget, &vis);
                    }
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
                    pos += 1;

                    // The single divider for our whole block goes BELOW the
                    // "SageThumbs 2K" entry, so the preview + quick verbs + this
                    // entry read as one cohesive "SageThumbs" group, fenced off from
                    // the rest of the menu (owner request).
                    let _ = InsertMenuW(hmenu, pos, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
                }
                // Command ids consumed: the preview slot (offset = leaf count) when a
                // preview was added, else the leaves the submenu used (0 when skipped).
                // Claiming the leaf range is harmless when only the preview is present.
                // Every leaf is drawn (the reorder only changes display order, not the
                // 0..leaves_n id range), so the id span consumed is leaves_n (+1 for the
                // preview slot just past the last leaf, when present).
                let consumed = if self.preview_cmd.get().is_some() {
                    leaves_n as u32 + 1
                } else {
                    leaves_n as u32
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
            let offset = (lp & 0xFFFF) as u32;
            let leaves = verbs::leaves();
            // Map the raw offset back to a typed slot through the central slot_for(),
            // so the "preview == leaves.len()" convention isn't re-derived here.
            let action = match verbs::slot_for(offset, leaves.len() as u32) {
                Some(verbs::CmdSlot::Preview) if self.preview_cmd.get().is_some() => {
                    // The preview thumbnail itself: open the image.
                    if let Some(p) = self.paths.borrow().first() {
                        open_with_default(p);
                    }
                    return Ok(());
                }
                Some(verbs::CmdSlot::Leaf(verbs::LeafId(i))) => {
                    leaves.get(i as usize).ok_or_else(|| Error::from(E_FAIL))?.1
                }
                // Preview slot but no preview added, or out of our range entirely.
                _ => return Err(Error::from(E_FAIL)),
            };
            let paths = self.paths.borrow().clone();
            // Run the (possibly multi-file, multi-second) batch on a DETACHED worker so
            // this Invoke returns immediately instead of freezing explorer.exe's UI
            // thread; the worker surfaces errors + reveals new-folder output itself. The
            // shell window is the natural parent for any error dialog.
            verbs::run_action_detached(action, paths, Some(pici.hwnd.0 as isize));
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

// The shell forwards WM_MEASUREITEM/WM_DRAWITEM here for our owner-drawn preview
// item (only present when MenuPreview != 0). menu_msg paints it; see its doc.
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
