//! In-process COM test for the IExplorerCommand verb + flyout, driven the way
//! the shell does: DllGetClassObject -> CreateInstance -> IExplorerCommand ->
//! EnumSubCommands -> Invoke. The verb-invoke test builds a real IShellItemArray
//! over a temp file and confirms the conversion actually runs end-to-end.
//!
//! (Whether the verb renders in the Win11 menu is a packaging matter only real
//! Explorer can confirm; this proves the COM object + verb logic.)
//!
//! Run via `scripts/test.ps1` (build before test) so LoadLibrary gets a fresh
//! cdylib — plain `cargo test` does not refresh target/<profile>/*.dll.
#![cfg(windows)]

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;

use image::ImageFormat;
use windows::core::{s, Error, Interface, Result, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::{E_FAIL, HANDLE, HGLOBAL, HMODULE};
use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, IClassFactory, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::UI::Shell::{
    SHCreateItemFromParsingName, SHCreateShellItemArrayFromShellItem, IExplorerCommand, IShellItem,
    IShellItemArray, ECF_HASSUBCOMMANDS, ECS_ENABLED, ECS_HIDDEN,
};

use sagethumbs2k_core::settings;

const CLSID_EXPLORER_COMMAND: GUID = GUID::from_u128(0xD4F1C8A2_3E7B_4A96_8C0F_6B1E2D9A4C57);
// The four modern-menu quick-verb coclasses (must match guids.rs / AppxManifest.xml).
const CLSID_QUICK_CONVERT_INTO: GUID = GUID::from_u128(0x1C7F4E2A_9D63_4B85_A0F1_7E2C5B9D4A60);
const CLSID_QUICK_CONVERT_DIALOG: GUID = GUID::from_u128(0x2D8A5F3B_0E74_4C96_B1A2_8F3D6CAE5B71);
const CLSID_QUICK_RESIZE: GUID = GUID::from_u128(0x3E9B6A4C_1F85_4DA7_C2B3_9A4E7DBF6C82);
const CLSID_QUICK_ROTATE: GUID = GUID::from_u128(0x4FAC7B5D_2096_4EB8_D3C4_AB5F8ECA7D93);

type DllGetClassObjectFn =
    unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;

fn dll_path() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap();
    exe.parent().unwrap().parent().unwrap().join("sagethumbs2k.dll")
}

unsafe fn create_for(clsid: GUID) -> Result<IExplorerCommand> {
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    let path = dll_path();
    assert!(path.exists(), "cdylib not built at {path:?}");
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let module: HMODULE = LoadLibraryW(PCWSTR(wide.as_ptr()))?;
    let proc = GetProcAddress(module, s!("DllGetClassObject")).ok_or_else(|| Error::from(E_FAIL))?;
    let dll_get_class_object: DllGetClassObjectFn = std::mem::transmute(proc);

    let mut factory_ptr: *mut c_void = std::ptr::null_mut();
    dll_get_class_object(&clsid, &IClassFactory::IID, &mut factory_ptr).ok()?;
    let factory = IClassFactory::from_raw(factory_ptr);
    factory.CreateInstance(None)
}

unsafe fn create_command() -> Result<IExplorerCommand> {
    create_for(CLSID_EXPLORER_COMMAND)
}

/// A single-file IShellItemArray over `path` (the file must exist), like the shell passes.
unsafe fn item_array(path: &std::path::Path) -> IShellItemArray {
    let pw: Vec<u16> = path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let item: IShellItem =
        SHCreateItemFromParsingName(PCWSTR(pw.as_ptr()), None).expect("shell item");
    SHCreateShellItemArrayFromShellItem(&item).expect("item array")
}

unsafe fn title_of(c: &IExplorerCommand) -> String {
    let pw = c.GetTitle(None).expect("GetTitle");
    let s = pw.to_string().unwrap_or_default();
    CoTaskMemFree(Some(pw.0 as *const c_void));
    s
}

unsafe fn collect_subcommands(cmd: &IExplorerCommand) -> Vec<IExplorerCommand> {
    let e = cmd.EnumSubCommands().expect("EnumSubCommands");
    let mut out = Vec::new();
    loop {
        let mut buf: [Option<IExplorerCommand>; 1] = [None];
        let mut fetched = 0u32;
        if e.Next(&mut buf, Some(&mut fetched)).is_err() {
            break;
        }
        if fetched == 0 {
            break;
        }
        if let Some(c) = buf[0].take() {
            out.push(c);
        }
    }
    out
}

#[test]
fn root_has_title_and_subcommands() {
    unsafe {
        let cmd = create_command().expect("create IExplorerCommand");
        let pw = cmd.GetTitle(None).expect("GetTitle");
        let title = pw.to_string().expect("utf16");
        CoTaskMemFree(Some(pw.0 as *const c_void));
        assert_eq!(title, "SageThumbs 2K");
        assert!(
            cmd.GetFlags().expect("GetFlags") & ECF_HASSUBCOMMANDS.0 as u32 != 0,
            "root should advertise sub-commands"
        );
    }
}

#[test]
fn enumerates_the_menu_tree() {
    unsafe {
        let cmd = create_command().expect("create");
        // Top level: the groups + the Copy leaf.
        let top: Vec<String> = collect_subcommands(&cmd).iter().map(|c| title_of(c)).collect();
        for want in ["Convert into", "Rotate / flip", "Copy to clipboard", "Set as wallpaper"] {
            assert!(top.iter().any(|t| t == want), "missing top-level {want} in {top:?}");
        }
        // "Convert into" is a submenu carrying the format leaves.
        let subs = collect_subcommands(&cmd);
        let convert = subs.iter().find(|c| title_of(c) == "Convert into").expect("Convert into group");
        assert!(
            convert.GetFlags().expect("GetFlags") & ECF_HASSUBCOMMANDS.0 as u32 != 0,
            "Convert into should be a submenu"
        );
        let fmts: Vec<String> = collect_subcommands(convert).iter().map(|c| title_of(c)).collect();
        for want in ["PNG", "JPG", "WebP", "TIFF", "Icon (.ico)"] {
            assert!(fmts.iter().any(|t| t == want), "missing format {want} in {fmts:?}");
        }
    }
}

#[test]
fn convert_verb_invoke_creates_file() {
    unsafe {
        // Don't pop an Explorer window during the test — the Convert verb's success
        // path calls ActionReport::reveal() (explorer /select,<out>); this gates it.
        std::env::set_var("ST2K_NO_REVEAL", "1");
        let cmd = create_command().expect("create");
        // Navigate root -> "Convert into" -> "JPG".
        let subs = collect_subcommands(&cmd);
        let convert = subs.iter().find(|c| title_of(c) == "Convert into").expect("Convert into group");
        let fmts = collect_subcommands(convert);
        let jpg = fmts.iter().find(|c| title_of(c) == "JPG").expect("jpg verb");

        let dir = std::env::temp_dir().join("st2k_verb_invoke");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("v.png");
        let mut img = image::RgbaImage::new(16, 16);
        for p in img.pixels_mut() {
            *p = image::Rgba([10, 200, 30, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&png, ImageFormat::Png)
            .unwrap();

        // Build a real IShellItemArray over the temp file, like the shell would.
        let pw: Vec<u16> = png.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(pw.as_ptr()), None).expect("shell item");
        let arr: IShellItemArray = SHCreateShellItemArrayFromShellItem(&item).expect("item array");

        jpg.Invoke(&arr, None).expect("Invoke");
        // Invoke now dispatches the verb to a DETACHED worker (so the shell thread no longer
        // blocks on the batch), so poll for the output rather than assuming it's done.
        let out = dir.join("v.jpg");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while !out.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        assert!(out.exists(), "Invoke should have created v.jpg");

        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[test]
fn clipboard_verb_copies_image_to_clipboard() {
    unsafe {
        let cmd = create_command().expect("create");
        let subs = collect_subcommands(&cmd);
        let clip = subs
            .iter()
            .find(|c| title_of(c) == "Copy to clipboard")
            .expect("clipboard verb");

        let dir = std::env::temp_dir().join("st2k_clip_invoke");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("c.png");
        let mut img = image::RgbaImage::new(24, 18);
        for p in img.pixels_mut() {
            *p = image::Rgba([30, 60, 200, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&png, image::ImageFormat::Png)
            .unwrap();

        let pw: Vec<u16> = png.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
        let item: IShellItem =
            SHCreateItemFromParsingName(PCWSTR(pw.as_ptr()), None).expect("shell item");
        let arr: IShellItemArray = SHCreateShellItemArrayFromShellItem(&item).expect("item array");

        clip.Invoke(&arr, None).expect("Invoke");

        // Read CF_DIB back off the clipboard and check the header. Invoke runs the verb on a
        // DETACHED worker now, so wait for the clipboard to actually be populated first (the
        // raw SetClipboardData handle persists after the worker thread exits).
        const CF_DIB: u32 = 8;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        while windows::Win32::System::DataExchange::IsClipboardFormatAvailable(CF_DIB).is_err()
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        OpenClipboard(None).expect("OpenClipboard");
        let h: HANDLE = GetClipboardData(CF_DIB).expect("GetClipboardData(CF_DIB)");
        let p = GlobalLock(HGLOBAL(h.0)) as *const BITMAPINFOHEADER;
        assert!(!p.is_null(), "clipboard DIB lock failed");
        let bih = *p;
        let _ = GlobalUnlock(HGLOBAL(h.0));
        let _ = CloseClipboard();

        assert_eq!(bih.biWidth, 24);
        assert_eq!(bih.biHeight.abs(), 18);
        assert_eq!(bih.biBitCount, 32);

        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// The modern-menu QUICK verbs are their own top-level coclasses (each its own CLSID):
/// the right structure (Convert into ▸ / Resize ▸ / Rotate ▸ are groups; Convert… is a
/// leaf) and the right GetState gate (an image is enabled iff the menu + quick-verbs
/// Options are on; an audio-only selection is always hidden, regardless of the toggle).
#[test]
fn quick_verbs_structure_and_gate() {
    unsafe {
        // Convert into ▸ — a group carrying the format leaves.
        let conv = create_for(CLSID_QUICK_CONVERT_INTO).expect("create convert-into");
        assert_eq!(title_of(&conv), "Convert into");
        assert!(
            conv.GetFlags().expect("GetFlags") & ECF_HASSUBCOMMANDS.0 as u32 != 0,
            "Convert into should advertise sub-commands"
        );
        let fmts: Vec<String> = collect_subcommands(&conv).iter().map(|c| title_of(c)).collect();
        for want in ["PNG", "JPG", "WebP"] {
            assert!(fmts.iter().any(|t| t == want), "missing format {want} in {fmts:?}");
        }

        // Convert… — a leaf (no sub-commands).
        let dlg = create_for(CLSID_QUICK_CONVERT_DIALOG).expect("create convert-dialog");
        assert_eq!(
            dlg.GetFlags().expect("GetFlags") & ECF_HASSUBCOMMANDS.0 as u32,
            0,
            "Convert\u{2026} is a leaf, not a submenu"
        );

        // Resize ▸ and Rotate ▸ — groups with children.
        for clsid in [CLSID_QUICK_RESIZE, CLSID_QUICK_ROTATE] {
            let cmd = create_for(clsid).expect("create group");
            assert!(
                cmd.GetFlags().expect("GetFlags") & ECF_HASSUBCOMMANDS.0 as u32 != 0,
                "quick group should advertise sub-commands"
            );
            assert!(!collect_subcommands(&cmd).is_empty(), "quick group should have children");
        }

        // Gating. Build a real PNG and an (extension-only) audio file.
        let dir = std::env::temp_dir().join("st2k_quick_gate");
        std::fs::create_dir_all(&dir).unwrap();
        let png = dir.join("q.png");
        let mut img = image::RgbaImage::new(8, 8);
        for p in img.pixels_mut() {
            *p = image::Rgba([1, 2, 3, 255]);
        }
        image::DynamicImage::ImageRgba8(img)
            .save_with_format(&png, ImageFormat::Png)
            .unwrap();
        let mp3 = dir.join("q.mp3");
        std::fs::write(&mp3, b"").unwrap(); // is_convertible_image gates on extension only

        let cmd = create_for(CLSID_QUICK_RESIZE).expect("create");
        // Image selection → ECS_ENABLED iff the menu + quick-verbs Options are both on.
        let want_enabled = settings::menu_enabled() && settings::menu_quick_verbs();
        let png_state = cmd.GetState(&item_array(&png), false).expect("GetState png");
        assert_eq!(
            png_state == ECS_ENABLED.0 as u32,
            want_enabled,
            "image quick verb should be enabled iff EnableMenu && MenuQuickVerbs"
        );
        // Audio-only selection → ECS_HIDDEN regardless of the toggle.
        let mp3_state = cmd.GetState(&item_array(&mp3), false).expect("GetState mp3");
        assert_eq!(mp3_state, ECS_HIDDEN.0 as u32, "audio-only must hide the quick verb");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
