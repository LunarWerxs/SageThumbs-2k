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

    // The clipboard is one globally-locked resource: OpenClipboard fails whenever ANY other
    // app momentarily holds it — and clipboard managers, Office, browsers, and the Win+V
    // history poller (which re-opens it after every copy) collide constantly. A single
    // attempt therefore silently loses the user's copy on a millisecond-scale collision
    // (screenshot Ctrl+C → nothing on the clipboard, no error). Retry briefly, bounded.
    let mut opened = false;
    for attempt in 0..10 {
        if OpenClipboard(None).is_ok() {
            opened = true;
            break;
        }
        if attempt < 9 {
            std::thread::sleep(std::time::Duration::from_millis(30));
        }
    }
    if !opened {
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

/// Any mix of CRLF / LF / lone CR → **CRLF**, the line ending Windows clipboard text and plain
/// Win32 `EDIT` controls both expect (a bare LF pastes as a box glyph, or loses the break
/// outright, in older apps and in every non-rich edit box).
///
/// One pass, one allocation — it copies the runs *between* line breaks wholesale rather than
/// per-char, and borrows without allocating at all when there is no break to fix. That matters
/// because the Quick preview's Ctrl+C hands whole documents through here, which can be megabytes.
/// Idempotent, so it is safe to apply on top of text that is already CRLF.
pub fn to_crlf(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains(['\r', '\n']) {
        return std::borrow::Cow::Borrowed(text);
    }
    let b = text.as_bytes();
    // `\r` and `\n` are ASCII, so they never appear inside a multi-byte UTF-8 sequence: every
    // index we slice at is a char boundary.
    let mut out = String::with_capacity(text.len() + text.len() / 16 + 16);
    let (mut start, mut i) = (0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'\r' | b'\n' => {
                out.push_str(&text[start..i]);
                out.push_str("\r\n");
                // Consume a CRLF pair as ONE break (otherwise it would become "\r\n\r\n").
                i += if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') {
                    2
                } else {
                    1
                };
                start = i;
            }
            _ => i += 1,
        }
    }
    out.push_str(&text[start..]);
    std::borrow::Cow::Owned(out)
}

/// UTF-16 (LE) + NUL bytes for `text`, ready to hand to `set_clipboard(CF_UNICODETEXT, …)`.
///
/// Line endings are normalized to CRLF first via [`to_crlf`]. This is the single payload builder
/// every text copy in the product goes through (OCR, preview selections, Image info, upload
/// links, hex colours), so normalizing here fixes the whole class once — callers must NOT
/// pre-normalize, that just allocates a second copy of the same string.
pub fn utf16_nul_bytes(text: &str) -> Vec<u8> {
    to_crlf(text)
        .encode_utf16()
        .chain(std::iter::once(0))
        .flat_map(u16::to_le_bytes)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(text: &str) -> String {
        let bytes = utf16_nul_bytes(text);
        let units: Vec<u16> = bytes
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(units.last(), Some(&0), "payload must be NUL-terminated");
        String::from_utf16_lossy(&units[..units.len() - 1])
    }

    /// Multi-line clipboard text must arrive as CRLF whichever way it was produced, and
    /// re-normalizing must not double the breaks (both matter now that OCR returns one line
    /// per recognized line, and its two copy paths feed this from raw text and from an EDIT
    /// control's already-CRLF contents).
    #[test]
    fn clipboard_text_normalizes_to_crlf_and_is_idempotent() {
        assert_eq!(round_trip("one\ntwo"), "one\r\ntwo");
        assert_eq!(round_trip("one\r\ntwo"), "one\r\ntwo");
        assert_eq!(round_trip("one\r\ntwo\nthree"), "one\r\ntwo\r\nthree");
        assert_eq!(round_trip("a\n\nb"), "a\r\n\r\nb");
        // No line break: byte-for-byte the old behaviour (the hex-colour / URL case).
        assert_eq!(round_trip("#FF8800"), "#FF8800");
    }

    /// A lone CR is a line break too. Classic-Mac text is rare, but an OCR line or a metadata
    /// field carrying one used to slip through un-normalized and paste as a single run-on line
    /// (or a box glyph) — the exact failure the CRLF pass exists to stop.
    #[test]
    fn lone_cr_counts_as_a_line_break() {
        assert_eq!(round_trip("one\rtwo"), "one\r\ntwo");
        assert_eq!(round_trip("a\r\rb"), "a\r\n\r\nb");
        assert_eq!(round_trip("a\r\nb\rc\nd"), "a\r\nb\r\nc\r\nd");
    }

    /// Text with nothing to fix must be BORROWED, not copied — the Quick preview's Ctrl+C
    /// hands whole documents through here.
    #[test]
    fn to_crlf_borrows_when_there_is_nothing_to_normalize() {
        assert!(matches!(
            to_crlf("no breaks here"),
            std::borrow::Cow::Borrowed(_)
        ));
        assert!(matches!(to_crlf(""), std::borrow::Cow::Borrowed(_)));
        assert!(matches!(to_crlf("has\nbreak"), std::borrow::Cow::Owned(_)));
    }

    /// Line breaks are ASCII, so slicing around them must never split a multi-byte char.
    #[test]
    fn to_crlf_preserves_non_ascii_text() {
        assert_eq!(to_crlf("日本語\nテキスト"), "日本語\r\nテキスト");
        assert_eq!(to_crlf("émoji 🎉\rnext"), "émoji 🎉\r\nnext");
        assert_eq!(to_crlf("日本語"), "日本語");
    }
}
