//! Settles, in code, the Win32 teardown-order question the Settings menu-items popup rests on.
//!
//! WHY THIS EXISTS. A 2026-08-15 code review claimed (rated P0 by one reviewer, and confirmed
//! by a verifier) that `settings_dlg/menuitems.rs` permanently destroys the menu-items
//! checklist: it re-parents that ListView from the Settings window into a popup, and hands it
//! back in the popup's `WM_DESTROY`. The claim was that `DestroyWindow` destroys CHILDREN
//! FIRST, so by the time `WM_DESTROY` runs the list is already gone and every later open finds
//! nothing.
//!
//! Microsoft documents the opposite: `WM_DESTROY` "is sent first to the window being destroyed
//! and then to the child windows as they are destroyed", and "during the processing of the
//! message, it can be assumed that all child windows still exist".
//!
//! Documentation is not evidence about the machine in front of us, and "open Settings and
//! click it" is not a check that can run in CI. So this asserts the ACTUAL behaviour of the
//! real API on the real OS, in the exact shape `menuitems.rs` uses: parent owns a child by
//! control id, child is re-parented into a popup, popup is destroyed, and the popup's own
//! `WM_DESTROY` hands the child back.
//!
//! If this test ever fails, the reviewers were right, `menuitems.rs` has a real bug, and the
//! fix is to re-parent in the IDOK/WM_CLOSE handlers BEFORE calling `DestroyWindow`.

#![cfg(windows)]

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetDlgItem, IsWindow, RegisterClassW,
    SetParent, HMENU, WINDOW_EX_STYLE, WNDCLASSW, WS_CHILD, WS_OVERLAPPED, WS_POPUP,
};

/// The control id the child is created with, mirroring `ID_MENU_ITEMS_LIST`.
const CHILD_ID: isize = 4242;

/// Set by the popup's `WM_DESTROY` to the parent it handed the child back to, or 0 if
/// `GetDlgItem` could not find the child at that moment. That second case IS the bug.
static HANDED_BACK_TO: AtomicIsize = AtomicIsize::new(0);
/// Whether `GetDlgItem` still resolved the child during the popup's `WM_DESTROY`.
static CHILD_VISIBLE_IN_WM_DESTROY: AtomicBool = AtomicBool::new(false);
/// Where the child should be handed back to, published before the popup is destroyed.
static REAL_PARENT: AtomicIsize = AtomicIsize::new(0);

const WM_DESTROY: u32 = 0x0002;

extern "system" fn popup_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_DESTROY {
            // EXACTLY what menuitems.rs does in its own WM_DESTROY arm.
            if let Ok(child) = GetDlgItem(Some(hwnd), CHILD_ID as i32) {
                CHILD_VISIBLE_IN_WM_DESTROY.store(true, Ordering::SeqCst);
                let back = HWND(REAL_PARENT.load(Ordering::SeqCst) as *mut core::ffi::c_void);
                if SetParent(child, Some(back)).is_ok() {
                    HANDED_BACK_TO.store(back.0 as isize, Ordering::SeqCst);
                }
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

extern "system" fn plain_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn register(
    class: windows::core::PCWSTR,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
) {
    unsafe {
        let hinst = GetModuleHandleW(None).expect("module handle");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: hinst.into(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

/// A window re-parented out of a popup, and handed back in that popup's `WM_DESTROY`,
/// SURVIVES the popup's destruction.
#[test]
fn a_reparented_child_survives_its_temporary_popup_being_destroyed() {
    unsafe {
        register(w!("St2kTeardownOwner"), plain_proc);
        register(w!("St2kTeardownPopup"), popup_proc);
        let hinst = GetModuleHandleW(None).expect("module handle");

        // The stand-in for the Settings window, and its child control.
        let owner = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("St2kTeardownOwner"),
            w!(""),
            WS_OVERLAPPED,
            0,
            0,
            200,
            200,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("owner window");
        let child = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("St2kTeardownOwner"),
            w!(""),
            WS_CHILD,
            0,
            0,
            50,
            50,
            Some(owner),
            Some(HMENU(CHILD_ID as *mut core::ffi::c_void)),
            Some(hinst.into()),
            None,
        )
        .expect("child control");
        REAL_PARENT.store(owner.0 as isize, Ordering::SeqCst);

        assert!(
            GetDlgItem(Some(owner), CHILD_ID as i32).is_ok(),
            "precondition: the owner must resolve its child by control id"
        );

        // The stand-in for the menu-items popup, borrowing the child.
        let popup = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("St2kTeardownPopup"),
            w!(""),
            WS_POPUP,
            0,
            0,
            200,
            200,
            None,
            None,
            Some(hinst.into()),
            None,
        )
        .expect("popup window");
        SetParent(child, Some(popup)).expect("re-parent into the popup");
        assert!(
            GetDlgItem(Some(popup), CHILD_ID as i32).is_ok(),
            "precondition: the popup must now own the child"
        );

        // The moment under test.
        DestroyWindow(popup).expect("destroy the popup");

        assert!(
            CHILD_VISIBLE_IN_WM_DESTROY.load(Ordering::SeqCst),
            "the popup's WM_DESTROY could NOT resolve the child: children really are \
             destroyed first on this OS, so menuitems.rs has a real bug and must re-parent \
             in its IDOK/WM_CLOSE handlers BEFORE calling DestroyWindow"
        );
        assert!(
            IsWindow(Some(child)).as_bool(),
            "the child did not survive its temporary popup, which is exactly the reported \
             'menu-items checklist never comes back' failure"
        );
        assert_eq!(
            HANDED_BACK_TO.load(Ordering::SeqCst),
            owner.0 as isize,
            "the child was not handed back to the owner window"
        );
        assert!(
            GetDlgItem(Some(owner), CHILD_ID as i32).is_ok(),
            "the owner cannot resolve the child by control id again, so a second open would \
             find nothing even though the window technically still exists"
        );

        let _ = DestroyWindow(owner);
    }
}
