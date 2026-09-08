//! C FFI bindings for djvu-rs.
//!
//! Exposes a stable C API for opening DjVu documents, querying metadata,
//! rendering pages, and extracting text. All functions are `extern "C"`
//! with no-panic guarantees via `catch_unwind`.
//!
//! The actual open→size→render→text flow and the `(code, message)` error
//! taxonomy live in [`crate::foreign`]; this module is the C-specific cap over
//! it — `CString` lifecycle, raw-pointer handles, and the [`guard`] panic
//! boundary. Each fallible entry point is one [`guard`] call wrapping one core
//! call.

use std::ffi::CString;
use std::os::raw::c_char;
use std::slice;

use crate::djvu_document::DjVuDocument;
use crate::foreign::{self, ForeignError};
use crate::pixmap::Pixmap;

/// Opaque document handle.
pub struct DjvuDoc {
    inner: DjVuDocument,
}

/// Opaque pixmap handle.
pub struct DjvuPixmap {
    inner: Pixmap,
}

/// Error information. Caller provides a pointer; callee fills it.
#[repr(C)]
pub struct DjvuError {
    /// Error code: 0 = success, 1 = parse error, 2 = render error, 3 = out of range
    pub code: i32,
    /// Null-terminated error message (owned by the error struct, freed by djvu_error_free).
    pub message: *mut c_char,
}

fn set_error(err: *mut DjvuError, code: i32, msg: &str) {
    if err.is_null() {
        return;
    }
    let c_msg = CString::new(msg).unwrap_or_else(|_| CString::new("unknown error").unwrap());
    unsafe {
        (*err).code = code;
        (*err).message = c_msg.into_raw();
    }
}

fn clear_error(err: *mut DjvuError) {
    if err.is_null() {
        return;
    }
    unsafe {
        (*err).code = 0;
        (*err).message = std::ptr::null_mut();
    }
}

/// Run `f` behind the C panic/error boundary: clear `err`, catch any panic, and
/// translate a [`ForeignError`] into `(code, message)` on `err`, returning
/// `fallback` on either failure. This is the single place the C ABI maps the
/// shared error taxonomy onto its `DjvuError` out-parameter.
///
/// `panic_code` / `panic_msg` are used only for the (should-never-happen) case
/// where the core itself panics.
fn guard<T>(
    err: *mut DjvuError,
    fallback: T,
    panic_code: i32,
    panic_msg: &str,
    f: impl FnOnce() -> Result<T, ForeignError>,
) -> T {
    clear_error(err);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(Ok(value)) => value,
        Ok(Err(e)) => {
            set_error(err, e.code(), &e.to_string());
            fallback
        }
        Err(_) => {
            set_error(err, panic_code, panic_msg);
            fallback
        }
    }
}

/// Borrow the document behind a handle, or an out-of-range error for a null
/// handle (a null handle is an invalid reference, code `3`, like a bad index).
///
/// # Safety
///
/// `doc` must be null or a valid pointer returned by `djvu_doc_open` that has
/// not yet been freed.
unsafe fn doc_ref<'a>(doc: *const DjvuDoc) -> Result<&'a DjVuDocument, ForeignError> {
    if doc.is_null() {
        return Err(ForeignError::OutOfRange("null document".to_string()));
    }
    Ok(unsafe { &(*doc).inner })
}

/// Free an error message string.
///
/// # Safety
///
/// `err` must be null or point to a valid `DjvuError` whose `message` field
/// was allocated by this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_error_free(err: *mut DjvuError) {
    if err.is_null() {
        return;
    }
    unsafe {
        if !(*err).message.is_null() {
            drop(CString::from_raw((*err).message));
            (*err).message = std::ptr::null_mut();
        }
    }
}

/// Open a DjVu document from a byte buffer.
///
/// Returns NULL on error. Caller must free with `djvu_doc_free`.
///
/// # Safety
///
/// `data` must point to at least `len` readable bytes, or be null (in which
/// case the function returns NULL and sets `err`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_doc_open(
    data: *const u8,
    len: usize,
    err: *mut DjvuError,
) -> *mut DjvuDoc {
    guard(
        err,
        std::ptr::null_mut(),
        1,
        "panic during document open",
        || {
            if data.is_null() || len == 0 {
                return Err(ForeignError::Parse("null or empty input".to_string()));
            }
            let bytes = unsafe { slice::from_raw_parts(data, len) };
            let doc = foreign::open(bytes)?;
            Ok(Box::into_raw(Box::new(DjvuDoc { inner: doc })))
        },
    )
}

/// Free a document handle.
///
/// # Safety
///
/// `doc` must be null or a pointer returned by `djvu_doc_open` that has not
/// yet been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_doc_free(doc: *mut DjvuDoc) {
    if !doc.is_null() {
        unsafe { drop(Box::from_raw(doc)) };
    }
}

/// Get the number of pages in the document.
///
/// # Safety
///
/// `doc` must be null or a valid pointer returned by `djvu_doc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_doc_page_count(doc: *const DjvuDoc) -> usize {
    if doc.is_null() {
        return 0;
    }
    unsafe { (*doc).inner.page_count() }
}

/// Get page width in pixels.
///
/// # Safety
///
/// `doc` must be a valid pointer returned by `djvu_doc_open`.
/// `err` must be null or point to a valid `DjvuError`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_page_width(
    doc: *const DjvuDoc,
    page: usize,
    err: *mut DjvuError,
) -> u32 {
    guard(err, 0, 3, "panic", || {
        let doc = unsafe { doc_ref(doc)? };
        foreign::page_width(doc, page)
    })
}

/// Get page height in pixels.
///
/// # Safety
///
/// `doc` must be a valid pointer returned by `djvu_doc_open`.
/// `err` must be null or point to a valid `DjvuError`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_page_height(
    doc: *const DjvuDoc,
    page: usize,
    err: *mut DjvuError,
) -> u32 {
    guard(err, 0, 3, "panic", || {
        let doc = unsafe { doc_ref(doc)? };
        foreign::page_height(doc, page)
    })
}

/// Get page DPI.
///
/// # Safety
///
/// `doc` must be a valid pointer returned by `djvu_doc_open`.
/// `err` must be null or point to a valid `DjvuError`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_page_dpi(
    doc: *const DjvuDoc,
    page: usize,
    err: *mut DjvuError,
) -> u32 {
    guard(err, 0, 3, "panic", || {
        let doc = unsafe { doc_ref(doc)? };
        foreign::page_dpi(doc, page)
    })
}

/// Render a page at the given DPI. Returns NULL on error.
/// Caller must free with `djvu_pixmap_free`.
///
/// # Safety
///
/// `doc` must be a valid pointer returned by `djvu_doc_open`.
/// `err` must be null or point to a valid `DjvuError`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_page_render(
    doc: *const DjvuDoc,
    page: usize,
    dpi: f32,
    err: *mut DjvuError,
) -> *mut DjvuPixmap {
    guard(err, std::ptr::null_mut(), 2, "panic during render", || {
        let doc = unsafe { doc_ref(doc)? };
        let pixmap = foreign::render_at_dpi(doc, page, dpi)?;
        Ok(Box::into_raw(Box::new(DjvuPixmap { inner: pixmap })))
    })
}

/// Free a pixmap handle.
///
/// # Safety
///
/// `pm` must be null or a pointer returned by `djvu_page_render` that has not
/// yet been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_pixmap_free(pm: *mut DjvuPixmap) {
    if !pm.is_null() {
        unsafe { drop(Box::from_raw(pm)) };
    }
}

/// Get pixmap width.
///
/// # Safety
///
/// `pm` must be null or a valid pointer returned by `djvu_page_render`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_pixmap_width(pm: *const DjvuPixmap) -> u32 {
    if pm.is_null() {
        return 0;
    }
    unsafe { (*pm).inner.width }
}

/// Get pixmap height.
///
/// # Safety
///
/// `pm` must be null or a valid pointer returned by `djvu_page_render`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_pixmap_height(pm: *const DjvuPixmap) -> u32 {
    if pm.is_null() {
        return 0;
    }
    unsafe { (*pm).inner.height }
}

/// Get pointer to RGBA pixel data. Length = width * height * 4.
/// The pointer is valid until `djvu_pixmap_free` is called.
///
/// # Safety
///
/// `pm` must be null or a valid pointer returned by `djvu_page_render`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_pixmap_data(pm: *const DjvuPixmap) -> *const u8 {
    if pm.is_null() {
        return std::ptr::null();
    }
    unsafe { (*pm).inner.data.as_ptr() }
}

/// Get the length of the pixmap data buffer in bytes.
///
/// # Safety
///
/// `pm` must be null or a valid pointer returned by `djvu_page_render`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_pixmap_data_len(pm: *const DjvuPixmap) -> usize {
    if pm.is_null() {
        return 0;
    }
    unsafe { (*pm).inner.data.len() }
}

/// Extract text from a page. Returns NULL if no text layer.
/// Caller must free with `djvu_text_free`.
///
/// # Safety
///
/// `doc` must be a valid pointer returned by `djvu_doc_open`.
/// `err` must be null or point to a valid `DjvuError`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_page_text(
    doc: *const DjvuDoc,
    page: usize,
    err: *mut DjvuError,
) -> *mut c_char {
    guard(
        err,
        std::ptr::null_mut(),
        2,
        "panic during text extraction",
        || {
            let doc = unsafe { doc_ref(doc)? };
            match foreign::text(doc, page)? {
                Some(text) => {
                    let c = CString::new(text).map_err(|e| ForeignError::Decode(e.to_string()))?;
                    Ok(c.into_raw())
                }
                None => Ok(std::ptr::null_mut()),
            }
        },
    )
}

/// Free a text string returned by `djvu_page_text`.
///
/// # Safety
///
/// `text` must be null or a pointer returned by `djvu_page_text` that has not
/// yet been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn djvu_text_free(text: *mut c_char) {
    if !text.is_null() {
        unsafe { drop(CString::from_raw(text)) };
    }
}
