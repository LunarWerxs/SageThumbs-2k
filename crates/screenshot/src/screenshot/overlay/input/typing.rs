//! WM_CHAR into the Text tool's buffer, surrogate pairs included.

use super::*;

/// `WM_CHAR`: only meaningful while an annotation is being typed. Everything else
/// (tool shortcuts) goes through `WM_KEYDOWN`/[`handle_key`] instead.
pub(super) unsafe fn on_char(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let s = &mut *shot_ptr(hwnd);
    if s.typing.is_none() {
        return LRESULT(0);
    }
    // WM_CHAR carries one UTF-16 code unit — decode it (not a single ASCII byte) so
    // accented and other Unicode characters type correctly. A non-BMP character arrives
    // as a high+low surrogate pair across two messages; buffer the high half until its
    // low half lands.
    let u = (wparam.0 & 0xFFFF) as u16;
    push_typed_char(s, u);
    let _ = InvalidateRect(Some(hwnd), None, false);
    LRESULT(0)
}

/// Append one UTF-16 code unit typed into the active text buffer, handling surrogate
/// pairs, backspace, Enter, and the DEL-as-tofu-glyph exclusion. Split out of
/// [`on_char`] so the surrogate-pair nesting doesn't pile onto the message-dispatch
/// function's own complexity.
pub(super) fn push_typed_char(s: &mut Shot, u: u16) {
    if let Some(hi) = s.pending_hi.take() {
        // Expecting the low half of a surrogate pair.
        if (0xDC00..=0xDFFF).contains(&u) {
            push_surrogate_pair(s, hi, u);
            return;
        }
        // Stray high surrogate without a matching low half — drop it and fall through
        // to process `u` on its own.
    }
    if (0xD800..=0xDBFF).contains(&u) {
        s.pending_hi = Some(u); // high surrogate — wait for its low half
    } else if u == 0x08 {
        pop_typed_char(s);
    } else if u == 0x0D {
        // Enter mid-annotation: handle_key's VK_RETURN branch defers to here instead of
        // committing/closing while typing (see there), so this is where the literal
        // newline actually lands.
        push_char_into_typing(s, '\n');
    } else if u >= 0x20 && u != 0x7F {
        // A BMP character (lone surrogates were handled above), excluding DEL (0x7F,
        // sent by Ctrl+Backspace on some layouts) — it renders as a tofu glyph instead
        // of doing anything useful, so drop it rather than insert it. Lossy path so an
        // unexpected unpaired surrogate can't panic.
        push_str_into_typing(s, &String::from_utf16_lossy(&[u]));
    }
}

/// Decode a buffered high surrogate plus its low half and append the resulting char.
pub(super) fn push_surrogate_pair(s: &mut Shot, hi: u16, lo: u16) {
    if let Some(ch) = char::decode_utf16([hi, lo]).next().and_then(|r| r.ok()) {
        push_char_into_typing(s, ch);
    }
}

/// Backspace: drop the last char of the active text buffer.
pub(super) fn pop_typed_char(s: &mut Shot) {
    if let Some((_, buf)) = s.typing.as_mut() {
        buf.pop();
    }
}

/// Append `ch` to the active text buffer (if any).
pub(super) fn push_char_into_typing(s: &mut Shot, ch: char) {
    if let Some((_, buf)) = s.typing.as_mut() {
        buf.push(ch);
    }
}

/// Append `text` to the active text buffer (if any).
pub(super) fn push_str_into_typing(s: &mut Shot, text: &str) {
    if let Some((_, buf)) = s.typing.as_mut() {
        buf.push_str(text);
    }
}
