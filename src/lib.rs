//! SageThumbs 2K — a modern, crash-isolated Rust shell extension.
//!
//! Phase 1: in-proc COM thumbnail provider (IThumbnailProvider +
//! IInitializeWithStream) decoding via the `image` crate, with a WIC fallback
//! coming in M2. See ROADMAP.md.

#![allow(non_snake_case)]

mod command;
mod container;
mod contextmenu;
mod decode;
mod dib;
pub mod cli;
mod factory;
pub mod formats;
mod guids;
mod jpegtran;
pub mod mcp;
mod ocr;
mod pdf;
mod strip;
mod topdf;
pub mod i18n;
pub mod register;
mod safety;
pub mod settings;
mod thumbprovider;
mod verbs;

/// Conversion API surfaced for the companion app's Convert… dialog.
pub use topdf::combine_to_pdf;
pub use verbs::{
    convert_file_opts, convert_to_magick, files_to_folder, tags_to_folders, ConvertOpts, Resize,
    Target,
};

/// Is ImageMagick available? Gates the magick-backed Convert targets (PSD/DDS/…),
/// which are hidden on a compact install.
pub fn magick_available() -> bool {
    decode::magick_available()
}

use core::ffi::c_void;
use std::sync::atomic::{AtomicI64, AtomicIsize, Ordering};

use windows::core::{Error, Interface, BOOL, GUID, HRESULT};
use windows::Win32::Foundation::{CLASS_E_CLASSNOTAVAILABLE, E_FAIL, E_POINTER, HMODULE, S_FALSE, S_OK};
use windows::Win32::System::Com::IClassFactory;
use windows::Win32::System::LibraryLoader::GetModuleFileNameW;

/// Live-object + lock count. `DllCanUnloadNow` returns S_OK only at zero.
static MODULE_REFS: AtomicI64 = AtomicI64::new(0);
/// This DLL's HMODULE, captured in DllMain, used to resolve our own path.
static HMODULE_PTR: AtomicIsize = AtomicIsize::new(0);

const DLL_PROCESS_ATTACH: u32 = 1;

pub fn dll_add_ref() {
    MODULE_REFS.fetch_add(1, Ordering::SeqCst);
}

pub fn dll_release() {
    let prev = MODULE_REFS.fetch_sub(1, Ordering::SeqCst);
    debug_assert!(prev > 0, "MODULE_REFS underflow: unbalanced LockServer(FALSE)/release");
}

/// Test/diagnostics hook: decode a file's bytes the same way the thumbnail
/// provider does (incl. ebook/comic cover extraction) and report the size.
#[doc(hidden)]
pub fn probe_cover(bytes: &[u8]) -> Option<(u32, u32)> {
    // decode_preview, not decode_full: this probes the THUMBNAIL path (container
    // covers included) — full fidelity would bypass the container tier for PSD.
    decode::decode_preview(bytes).ok().map(|img| (img.width(), img.height()))
}

/// Diagnostics: render the right-click menu preview for `path` to a PNG exactly
/// as the owner-draw paints it. `bg` = `None` for the live menu color, or
/// `Some(0x00RRGGBB)` to preview a chosen menu background.
#[doc(hidden)]
pub fn render_preview_png(path: &str, out_png: &str, bg: Option<u32>) -> bool {
    contextmenu::render_preview_png(path, out_png, bg)
}

/// Test/diagnostics hook: OCR an image file to text (the same path the "Copy
/// text" verb uses, minus the clipboard write). None if no OCR pack / no text.
#[doc(hidden)]
pub fn ocr_probe(path: &str) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    ocr::recognize_bytes(&bytes).ok().filter(|t| !t.trim().is_empty())
}

/// Decode image bytes into a premultiplied 32-bpp DIB-section HBITMAP, returned
/// as a raw handle (`isize`). For the Options dialog's logo/banner artwork. Uses
/// the `image` crate directly (PNG/JPEG/…) rather than the full thumbnail
/// pipeline, so the app binary doesn't pull in resvg/WIC/etc. None on failure.
pub fn image_to_hbitmap(bytes: &[u8]) -> Option<isize> {
    let rgba = image::load_from_memory(bytes).ok()?.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    let hbmp = unsafe { dib::create_premultiplied_dib(w, h, rgba.as_raw()) }.ok()?;
    Some(hbmp.0 as isize)
}

/// Decode any supported image through the FULL pipeline (image → WIC →
/// ImageMagick → containers/SVG, same as the thumbnail provider) into raw
/// top-to-bottom RGBA8 plus its dimensions. For the companion app's eyedropper,
/// which samples colors and needs to handle every format the menu offers (HEIC,
/// RAW, ebook covers, …), not just what `image::load_from_memory` reads. None on
/// failure. (The app already links `decode_full` via the Convert dialog, so this
/// adds no new dependency weight.)
pub fn decode_to_rgba8(bytes: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let rgba = decode::decode_full(bytes).ok()?.to_rgba8();
    Some((rgba.width(), rgba.height(), rgba.into_raw()))
}

/// Raw straight-RGBA pixels (top row first) → premultiplied 32-bpp DIB-section
/// HBITMAP handle. For app artwork the caller composites itself (e.g. the About
/// box's light-mode logo chip). None on failure or size mismatch.
pub fn rgba_to_hbitmap(w: u32, h: u32, rgba: &[u8]) -> Option<isize> {
    if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let hbmp = unsafe { dib::create_premultiplied_dib(w as i32, h as i32, rgba) }.ok()?;
    Some(hbmp.0 as isize)
}

/// Like [`image_to_hbitmap`] but resized to exactly `w`x`h` (for the fixed-size
/// logo / banner controls; also decodes a remote-downloaded image). The first
/// frame is used for animated formats (GIF).
pub fn image_to_hbitmap_sized(bytes: &[u8], w: u32, h: u32) -> Option<isize> {
    if w == 0 || h == 0 {
        return None;
    }
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img.resize_exact(w, h, image::imageops::FilterType::Lanczos3).to_rgba8();
    let hbmp = unsafe { dib::create_premultiplied_dib(w as i32, h as i32, rgba.as_raw()) }.ok()?;
    Some(hbmp.0 as isize)
}

/// Decode an animated GIF into one `w`x`h` HBITMAP per frame plus the inter-frame
/// delay in ms (so the Options banner can animate it). Returns None if `bytes`
/// isn't a multi-frame GIF — the caller then uses the single-image path.
pub fn decode_gif_frames_sized(bytes: &[u8], w: u32, h: u32) -> Option<(Vec<isize>, u32)> {
    use image::codecs::gif::GifDecoder;
    use image::AnimationDecoder;
    use std::io::Cursor;

    if w == 0 || h == 0 {
        return None;
    }
    let decoder = GifDecoder::new(Cursor::new(bytes)).ok()?;
    let frames = decoder.into_frames().collect_frames().ok()?;
    if frames.len() < 2 {
        return None; // single frame — not animated
    }
    let (n, d) = frames[0].delay().numer_denom_ms();
    let delay_ms = if d == 0 { 100 } else { (n / d).clamp(20, 1000) };

    let mut handles = Vec::with_capacity(frames.len());
    for frame in &frames {
        let img = image::DynamicImage::ImageRgba8(frame.buffer().clone());
        let rgba = img.resize_exact(w, h, image::imageops::FilterType::Triangle).to_rgba8();
        let hbmp = unsafe { dib::create_premultiplied_dib(w as i32, h as i32, rgba.as_raw()) }.ok()?;
        handles.push(hbmp.0 as isize);
    }
    Some((handles, delay_ms))
}

/// RAII module-reference guard. Constructing one (via `Default`) bumps the
/// live-object count; dropping it releases. Each COM coclass carries a
/// `_ref: ModuleRef` field instead of hand-writing an add-ref in its
/// constructor and a matching `impl Drop` — six identical pairs collapse to
/// this one type. (The factory's `LockServer` count is a separate add/release
/// path and intentionally does NOT use this.)
pub struct ModuleRef;

impl Default for ModuleRef {
    fn default() -> Self {
        dll_add_ref();
        ModuleRef
    }
}

impl Drop for ModuleRef {
    fn drop(&mut self) {
        dll_release();
    }
}

#[no_mangle]
pub extern "system" fn DllMain(hmodule: HMODULE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        HMODULE_PTR.store(hmodule.0 as isize, Ordering::SeqCst);
    }
    BOOL(1)
}

#[no_mangle]
pub extern "system" fn DllCanUnloadNow() -> HRESULT {
    // <= 0 (not == 0) so a stray unbalanced LockServer(FALSE) can never pin the DLL.
    if MODULE_REFS.load(Ordering::SeqCst) <= 0 {
        S_OK
    } else {
        S_FALSE
    }
}

// COM ABI export: the Windows loader calls this by name (it cannot observe a
// Rust `unsafe` marker), and we null-check every pointer before use under the
// panic guard. The clippy correctness lint that wants raw-pointer-deref exports
// marked `unsafe` doesn't apply to a `#[no_mangle] extern` entry point.
#[allow(clippy::not_unsafe_ptr_arg_deref)]
#[no_mangle]
pub extern "system" fn DllGetClassObject(
    rclsid: *const GUID,
    riid: *const GUID,
    ppv: *mut *mut c_void,
) -> HRESULT {
    safety::guard_hr(|| unsafe {
        if rclsid.is_null() || riid.is_null() || ppv.is_null() {
            return E_POINTER;
        }
        *ppv = core::ptr::null_mut();
        let clsid = *rclsid;
        if clsid != guids::CLSID_THUMBNAIL_PROVIDER
            && clsid != guids::CLSID_EXPLORER_COMMAND
            && clsid != guids::CLSID_CONTEXT_MENU
        {
            return CLASS_E_CLASSNOTAVAILABLE;
        }
        let factory: IClassFactory = factory::ClassFactory::new(clsid).into();
        factory.query(riid, ppv)
    })
}

#[no_mangle]
pub extern "system" fn DllRegisterServer() -> HRESULT {
    safety::guard_hr(|| match module_path().and_then(|p| register::register(&p)) {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    })
}

#[no_mangle]
pub extern "system" fn DllUnregisterServer() -> HRESULT {
    safety::guard_hr(|| match register::unregister() {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    })
}

#[cfg(test)]
mod decode_rgba_tests {
    /// The eyedropper's color readout hinges on `decode_to_rgba8` returning bytes
    /// in **RGBA order, top row first**. Verify with a 2×2 image of four known,
    /// distinct colors so a channel swap or vertical flip would be caught.
    #[test]
    fn decode_to_rgba8_order_and_orientation() {
        let mut img = image::RgbaImage::new(2, 2);
        img.put_pixel(0, 0, image::Rgba([200, 40, 30, 255])); // top-left red-ish
        img.put_pixel(1, 0, image::Rgba([20, 180, 90, 255])); // top-right green-ish
        img.put_pixel(0, 1, image::Rgba([30, 60, 210, 255])); // bottom-left blue-ish
        img.put_pixel(1, 1, image::Rgba([240, 230, 10, 255])); // bottom-right yellow
        let mut bytes = Vec::new();
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut bytes), image::ImageFormat::Png)
            .unwrap();

        let (w, h, rgba) = super::decode_to_rgba8(&bytes).unwrap();
        assert_eq!((w, h), (2, 2));
        // Row 0 first (top-down), each pixel RGBA in order.
        assert_eq!(&rgba[0..4], &[200, 40, 30, 255], "top-left");
        assert_eq!(&rgba[4..8], &[20, 180, 90, 255], "top-right");
        assert_eq!(&rgba[8..12], &[30, 60, 210, 255], "bottom-left (top-down row order)");
        assert_eq!(&rgba[12..16], &[240, 230, 10, 255], "bottom-right");
    }
}

pub(crate) fn module_path() -> windows::core::Result<String> {
    unsafe {
        let h = HMODULE(HMODULE_PTR.load(Ordering::SeqCst) as *mut c_void);
        let mut buf = vec![0u16; 260];
        loop {
            let n = GetModuleFileNameW(Some(h), &mut buf) as usize;
            if n == 0 {
                return Err(Error::from_thread()); // GetLastError; includes HMODULE-missing
            }
            // n < len is the documented "it fit" signal; n == len means truncated.
            if n < buf.len() {
                return Ok(String::from_utf16_lossy(&buf[..n]));
            }
            if buf.len() >= 32_768 {
                return Err(Error::from(E_FAIL));
            }
            buf.resize((buf.len() * 2).min(32_768), 0);
        }
    }
}
