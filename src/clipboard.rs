//! One audited clipboard write, shared by the DLL verbs (`verbs::copy_to_clipboard`,
//! `ocr`) and the app's screenshot/OCR paths. The unsafe `HGLOBAL` ownership dance
//! used to be hand-copied in four places; centralizing it means a hardening fix (or
//! a leak/double-free bug) lands once, not in copies that can drift apart.

use windows::Win32::Foundation::{GlobalFree, HANDLE};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

/// Standard clipboard format: a packed device-independent bitmap.
pub const CF_DIB: u32 = 8;
/// Standard clipboard format: NUL-terminated UTF-16 text.
pub const CF_UNICODETEXT: u32 = 13;

/// Copy `bytes` onto the clipboard under `format`, via a moveable `HGLOBAL`.
/// Returns whether it succeeded. `bytes` must already be the EXACT payload the
/// format expects (a packed CF_DIB, or little-endian UTF-16 + NUL for text); it is
/// copied, so it need not outlive the call.
///
/// Owns the whole ownership dance: `GlobalAlloc(GMEM_MOVEABLE)` → lock (+ null
/// check) → copy → unlock → `OpenClipboard`/`EmptyClipboard`/`SetClipboardData`. On
/// ANY failure before `SetClipboardData` succeeds the block is `GlobalFree`d; on
/// success the system takes ownership and must NOT be freed.
///
/// # Safety
/// Calls Win32 clipboard / global-heap APIs and must run on a thread allowed to
/// open the clipboard (the foreground UI / verb thread, as all callers do).
pub unsafe fn set_clipboard(format: u32, bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let Ok(hmem) = GlobalAlloc(GMEM_MOVEABLE, bytes.len()) else {
        return false;
    };
    let base = GlobalLock(hmem) as *mut u8;
    if base.is_null() {
        let _ = GlobalFree(Some(hmem));
        return false;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), base, bytes.len());
    let _ = GlobalUnlock(hmem); // returns Err with NO_ERROR when fully unlocked — ignore

    if OpenClipboard(None).is_err() {
        let _ = GlobalFree(Some(hmem));
        return false;
    }
    let _ = EmptyClipboard();
    // On success the clipboard OWNS hmem; on a SetClipboardData failure the system
    // does NOT take ownership, so we free it ourselves.
    if SetClipboardData(format, Some(HANDLE(hmem.0))).is_err() {
        let _ = CloseClipboard();
        let _ = GlobalFree(Some(hmem));
        return false;
    }
    let _ = CloseClipboard();
    true
}

/// UTF-16 (LE) + NUL bytes for `text`, ready to hand to `set_clipboard(CF_UNICODETEXT, …)`.
pub fn utf16_nul_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}
