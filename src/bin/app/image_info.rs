//! The "Image info" window — a verbose, copyable metadata dump for the right-click
//! Tools verb. Launched standalone via `SageThumbs2K.exe --image-info <path>`: a
//! scrollable read-only edit with every file/image/EXIF field, plus a Copy button.

use core::cell::RefCell;

use windows::core::w;
use windows::Win32::Foundation::{HINSTANCE, HWND};
use windows::Win32::UI::WindowsAndMessaging::{ES_MULTILINE, ES_READONLY, WINDOW_STYLE};

use st2k_appkit::win::{
    result_buttons, result_edit, result_layout, result_window_proc, run_dialog, t, ResultWindow,
};

const ID_EDIT: i32 = 100;

thread_local! {
    /// The metadata text to show — set just before `run_dialog`, read in WM_CREATE.
    static INFO: RefCell<String> = const { RefCell::new(String::new()) };
}

/// Gather verbose metadata for `path` and show it in a scrollable, copyable window.
pub fn run_image_info(path: &str) {
    let text = st2k_codecs::strip::read_info_verbose(path);
    INFO.with(|i| *i.borrow_mut() = text);
    unsafe {
        // Title reuses the context-menu verb's key — same phrase, already translated
        // in every shipped locale.
        run_dialog(
            w!("SageThumbs2KImageInfo"),
            Some(result_window_proc::<ImageInfo>),
            t("menu_image_info"),
            480,
            470,
            None,
        );
    }
}

/// The plain result-window shape: a read-only, word-wrapped, scrollable dump above Copy and
/// Close, with Copy putting the whole stored dump on the clipboard.
struct ImageInfo;

impl ResultWindow for ImageInfo {
    unsafe fn build(hwnd: HWND, hinst: HINSTANCE) {
        // See `win::result_layout` for why this has to come off the real client rect rather
        // than the design size.
        let l = result_layout(hwnd);
        let style = WINDOW_STYLE((ES_MULTILINE | ES_READONLY) as u32);
        INFO.with(|i| result_edit(hwnd, hinst, &l, l.m, style, ID_EDIT, &i.borrow()));
        result_buttons(hwnd, hinst, &l);
    }

    unsafe fn copy_source(_hwnd: HWND) -> String {
        INFO.with(|i| i.borrow().clone())
    }
}
