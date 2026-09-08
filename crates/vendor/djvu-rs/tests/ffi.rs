//! C FFI surface tests (`src/ffi.rs`).
//!
//! The extern "C" API is what C consumers link against, yet it had 0 % coverage
//! — a public contract that could break silently. These tests drive the whole
//! lifecycle (open → metadata → render → text → free) and the error / null
//! paths directly through the C entry points.

use std::os::raw::c_char;

use djvu_rs::ffi::{
    DjvuError, djvu_doc_free, djvu_doc_open, djvu_doc_page_count, djvu_error_free, djvu_page_dpi,
    djvu_page_height, djvu_page_render, djvu_page_text, djvu_page_width, djvu_pixmap_data,
    djvu_pixmap_data_len, djvu_pixmap_free, djvu_pixmap_height, djvu_pixmap_width, djvu_text_free,
};

const FIXTURE: &str = "tests/fixtures/boy_jb2.djvu";

fn blank_err() -> DjvuError {
    DjvuError {
        code: 0,
        message: std::ptr::null_mut(),
    }
}

#[test]
fn ffi_full_lifecycle() {
    let bytes = std::fs::read(FIXTURE).expect("fixture present");
    let mut err = blank_err();

    // open
    let doc = unsafe { djvu_doc_open(bytes.as_ptr(), bytes.len(), &mut err) };
    assert!(!doc.is_null(), "open returned null (code {})", err.code);
    assert_eq!(err.code, 0, "open set an error code");

    // page count
    let n = unsafe { djvu_doc_page_count(doc) };
    assert!(n >= 1, "expected at least one page");

    // metadata for page 0
    let w = unsafe { djvu_page_width(doc, 0, &mut err) };
    let h = unsafe { djvu_page_height(doc, 0, &mut err) };
    let dpi = unsafe { djvu_page_dpi(doc, 0, &mut err) };
    assert!(w > 0 && h > 0, "non-positive dimensions {w}x{h}");
    assert!(dpi > 0, "non-positive dpi {dpi}");
    assert_eq!(err.code, 0);

    // render at a low dpi (cheap)
    let pm = unsafe { djvu_page_render(doc, 0, 72.0, &mut err) };
    assert!(!pm.is_null(), "render returned null (code {})", err.code);
    let pw = unsafe { djvu_pixmap_width(pm) };
    let ph = unsafe { djvu_pixmap_height(pm) };
    let len = unsafe { djvu_pixmap_data_len(pm) };
    let data = unsafe { djvu_pixmap_data(pm) };
    assert!(pw > 0 && ph > 0);
    assert!(!data.is_null());
    assert_eq!(
        len,
        (pw as usize) * (ph as usize) * 4,
        "RGBA data length must be w*h*4"
    );

    // text (may legitimately be null for this image); must not crash either way
    let text = unsafe { djvu_page_text(doc, 0, &mut err) };
    assert_eq!(err.code, 0, "text extraction set an error");
    unsafe { djvu_text_free(text) }; // null-safe

    // free everything
    unsafe { djvu_pixmap_free(pm) };
    unsafe { djvu_doc_free(doc) };
}

#[test]
fn ffi_open_rejects_null_and_garbage() {
    // null input
    let mut err = blank_err();
    let doc = unsafe { djvu_doc_open(std::ptr::null(), 0, &mut err) };
    assert!(doc.is_null());
    assert_ne!(err.code, 0, "null open should set an error code");
    assert!(!err.message.is_null(), "error should carry a message");
    unsafe { djvu_error_free(&mut err) };
    assert!(err.message.is_null(), "error_free must null the message");

    // garbage bytes
    let mut err = blank_err();
    let junk = [0xDEu8, 0xAD, 0xBE, 0xEF, 0x00, 0x11, 0x22, 0x33];
    let doc = unsafe { djvu_doc_open(junk.as_ptr(), junk.len(), &mut err) };
    assert!(doc.is_null());
    assert_ne!(err.code, 0);
    unsafe { djvu_error_free(&mut err) };
}

#[test]
fn ffi_null_and_out_of_range_handles() {
    // null document handle: count is 0, metadata sets out-of-range error
    assert_eq!(unsafe { djvu_doc_page_count(std::ptr::null()) }, 0);

    let mut err = blank_err();
    let w = unsafe { djvu_page_width(std::ptr::null(), 0, &mut err) };
    assert_eq!(w, 0);
    assert_eq!(err.code, 3, "null doc should be out-of-range (3)");
    unsafe { djvu_error_free(&mut err) };

    // valid doc, out-of-range page index
    let bytes = std::fs::read(FIXTURE).expect("fixture present");
    let mut err = blank_err();
    let doc = unsafe { djvu_doc_open(bytes.as_ptr(), bytes.len(), &mut err) };
    assert!(!doc.is_null());
    let mut err = blank_err();
    let h = unsafe { djvu_page_height(doc, 9999, &mut err) };
    assert_eq!(h, 0);
    assert_ne!(err.code, 0, "out-of-range page should error");
    unsafe { djvu_error_free(&mut err) };

    // render out of range → null
    let mut err = blank_err();
    let pm = unsafe { djvu_page_render(doc, 9999, 72.0, &mut err) };
    assert!(pm.is_null());
    assert_ne!(err.code, 0);
    unsafe { djvu_error_free(&mut err) };

    unsafe { djvu_doc_free(doc) };
}

#[test]
fn ffi_null_accessors_and_frees_are_safe() {
    // pixmap accessors on null
    assert_eq!(unsafe { djvu_pixmap_width(std::ptr::null()) }, 0);
    assert_eq!(unsafe { djvu_pixmap_height(std::ptr::null()) }, 0);
    assert_eq!(unsafe { djvu_pixmap_data_len(std::ptr::null()) }, 0);
    assert!(unsafe { djvu_pixmap_data(std::ptr::null()) }.is_null());

    // all free functions must accept null without crashing
    unsafe { djvu_doc_free(std::ptr::null_mut()) };
    unsafe { djvu_pixmap_free(std::ptr::null_mut()) };
    unsafe { djvu_text_free(std::ptr::null_mut()) };
    unsafe { djvu_error_free(std::ptr::null_mut()) };
    let null_text: *mut c_char = std::ptr::null_mut();
    unsafe { djvu_text_free(null_text) };
}
