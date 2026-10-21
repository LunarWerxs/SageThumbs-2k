//! With "Show quick actions in the menu" on, Convert, Resize and Rotate sit on the main
//! right-click menu and nowhere else: not again inside the SageThumbs submenu (classic) or
//! the SageThumbs flyout (Windows 11). Through 3.7.4 both copies showed (Discord, 2026-10-21:
//! "a user probably doesn't need those options twice in the context-menu").
//!
//! Drives the real COM surfaces Explorer drives, under a scratch settings key with the
//! toggle on. The key is resolved once per process, so every test in this file uses it.
#![cfg(windows)]

mod common;

use std::ffi::c_void;

use windows::core::{s, Error, Interface, GUID, HRESULT, PCWSTR};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::System::Com::{
    CoInitializeEx, CoTaskMemFree, IClassFactory, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::UI::Shell::IExplorerCommand;
use windows::Win32::UI::WindowsAndMessaging::{
    CreatePopupMenu, DestroyMenu, GetMenuItemCount, GetMenuStringW, GetSubMenu, HMENU,
    MF_BYPOSITION,
};
use windows_registry::CURRENT_USER;

use st2k_actions::verbs::QUICK_KEYS;

const CLSID_EXPLORER_COMMAND: GUID = GUID::from_u128(0xD4F1C8A2_3E7B_4A96_8C0F_6B1E2D9A4C57);

fn settings_root() -> String {
    format!(
        r"Software\SageThumbs2K\__test_quick_verbs_{}",
        std::process::id()
    )
}

/// The scratch key with quick actions on, removed however the test ends.
struct QuickOn(String);

impl QuickOn {
    fn new() -> Self {
        let root = settings_root();
        CURRENT_USER
            .create(&root)
            .and_then(|k| k.set_u32("MenuQuickVerbs", 1))
            .expect("write the scratch MenuQuickVerbs=1");
        unsafe { common::set_test_env("ST2K_SETTINGS_ROOT", &root) };
        Self(root)
    }
}

impl Drop for QuickOn {
    fn drop(&mut self) {
        let _ = CURRENT_USER.remove_tree(&self.0);
    }
}

fn photo(case: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("st2k_quick_verbs_{}_{case}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("photo.png");
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(64, 48))
        .save_with_format(&png, image::ImageFormat::Png)
        .unwrap();
    (dir, png)
}

fn quick_labels() -> Vec<String> {
    QUICK_KEYS
        .iter()
        .map(|k| st2k_base::i18n::t(k).to_string())
        .collect()
}

unsafe fn labels(menu: HMENU) -> Vec<String> {
    (0..unsafe { GetMenuItemCount(Some(menu)) }.max(0) as u32)
        .map(|i| {
            let mut buf = [0u16; 256];
            let n = unsafe { GetMenuStringW(menu, i, Some(&mut buf), MF_BYPOSITION) };
            String::from_utf16_lossy(&buf[..n.max(0) as usize])
        })
        .collect()
}

/// One test for both surfaces: they share the scratch key, and a second test's cleanup would
/// delete it while this one still reads it.
#[test]
fn each_quick_action_is_listed_once_with_quick_actions_on() {
    let _key = QuickOn::new();
    unsafe { classic_menu_lists_each_quick_action_once() };
    unsafe { flyout_leaves_the_quick_actions_to_the_main_menu() };
}

unsafe fn classic_menu_lists_each_quick_action_once() {
    let (dir, png) = photo("classic");
    unsafe {
        let menu = common::classic_context_menu_for(&png, &settings_root())
            .expect("create and initialize classic context menu");
        let popup = CreatePopupMenu().expect("CreatePopupMenu");
        menu.QueryContextMenu(popup, 0, 1, 0x7fff, 0)
            .ok()
            .expect("QueryContextMenu");
        let main = labels(popup);
        let at = main
            .iter()
            .position(|l| l == "SageThumbs 2K")
            .unwrap_or_else(|| panic!("no SageThumbs submenu on {main:?}"));
        let sub = labels(GetSubMenu(popup, at as i32));
        for quick in quick_labels() {
            assert!(
                main.contains(&quick),
                "{quick} is not on the main menu: {main:?}"
            );
            assert!(
                !sub.contains(&quick),
                "{quick} is listed again in the submenu: {sub:?}"
            );
        }
        assert!(
            sub.contains(&st2k_base::i18n::t("menu_rename").to_string()),
            "the rest of the submenu stays: {sub:?}"
        );
        let _ = DestroyMenu(popup);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

unsafe fn flyout_leaves_the_quick_actions_to_the_main_menu() {
    type DllGetClassObjectFn =
        unsafe extern "system" fn(*const GUID, *const GUID, *mut *mut c_void) -> HRESULT;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let wide = common::to_wide(common::dll_path().as_os_str());
        let module = LoadLibraryW(PCWSTR(wide.as_ptr())).expect("load the built DLL");
        let proc = GetProcAddress(module, s!("DllGetClassObject"))
            .ok_or_else(|| Error::from(E_FAIL))
            .expect("DllGetClassObject");
        let get_class: DllGetClassObjectFn = std::mem::transmute(proc);
        let mut factory_ptr = std::ptr::null_mut();
        get_class(
            &CLSID_EXPLORER_COMMAND,
            &IClassFactory::IID,
            &mut factory_ptr,
        )
        .ok()
        .expect("class factory");
        let factory = IClassFactory::from_raw(factory_ptr);
        let root: IExplorerCommand = factory.CreateInstance(None).expect("root command");

        let e = root.EnumSubCommands().expect("EnumSubCommands");
        let mut titles = Vec::new();
        loop {
            let mut buf: [Option<IExplorerCommand>; 1] = [None];
            let mut fetched = 0u32;
            if e.Next(&mut buf, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            if let Some(c) = buf[0].take() {
                let pw = c.GetTitle(None).expect("GetTitle");
                titles.push(pw.to_string().unwrap_or_default());
                CoTaskMemFree(Some(pw.0 as *const c_void));
            }
        }
        for quick in quick_labels() {
            assert!(
                !titles.contains(&quick),
                "{quick} is listed again in the flyout: {titles:?}"
            );
        }
        assert!(
            titles.contains(&st2k_base::i18n::t("menu_rename").to_string()),
            "the rest of the flyout stays: {titles:?}"
        );
    }
}
