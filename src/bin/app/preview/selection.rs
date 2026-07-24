//! Selection for the viewer's text panes: point→offset hit-testing, keyboard (Shift+arrow)
//! selection, and the text Ctrl+C copies.
//!
//! Both panes share ONE coordinate space: raw byte offsets into the *selection document* — the
//! text pane's file text, or the Markdown pane's document (built with the block layout in
//! [`super::markdown`], so you select what you SEE, in reading order — not the raw file, whose
//! byte offsets don't match the rendered flow). Offsets always land on char boundaries.
//!
//! That Markdown document is re-emitted AS Markdown (`#` headings, `-` bullets with their nesting
//! indent, `>` quotes, ``` fences, blank lines between blocks) so a copy pastes with its structure
//! intact; see `markdown::doc_append`. The structural prefixes are copied but not selectable.
//!
//! The two panes hit-test differently because they're laid out differently. The text pane is a
//! mono grid, so it computes offsets analytically (line index + `col_at`). Markdown is a
//! proportional, wrapped, mixed-font flow, so every drawn token records a [`SelHit`] (rect +
//! document slice + font spec) during paint and hit-testing measures inside the picked one.
//! Only VISIBLE tokens get rects (paint culls); the document is always complete, so copying is
//! never limited to what's on screen.

use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    DeleteObject, GetDC, GetTextMetricsW, InvalidateRect, ReleaseDC, SelectObject, UpdateWindow,
    TEXTMETRICW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VK_DOWN, VK_END, VK_HOME, VK_LEFT, VK_NEXT, VK_PRIOR, VK_RIGHT, VK_UP,
};

use super::highlight;
use super::paint::mono_font;
use super::window::{content_rect, set_text_scroll, state, ContentKind, ViewerState};

/// A drawn, selectable text token: where it landed, which slice of the selection document it
/// shows, and how to re-create its font so hit-testing can measure INSIDE it (the `HFONT` itself
/// is freed when the block finishes drawing, so the spec is what survives).
#[derive(Clone, Copy)]
pub(super) struct SelHit {
    pub rect: RECT,
    pub start: usize,
    pub end: usize,
    pub font: FontSpec,
    /// x of the token's first glyph — past an inline-code pill's padding / a code block's
    /// line-number gutter, which `rect.left` includes.
    pub text_x: i32,
}

/// Enough to re-create a drawn token's font (`px` is the unscaled design size, DPI-scaled the
/// same way at both paint and hit-test time).
#[derive(Clone, Copy, PartialEq)]
pub(super) struct FontSpec {
    pub px: i32,
    pub bold: bool,
    pub italic: bool,
    pub mono: bool,
}

/// The selection, normalized to `(start, end)` with `start < end`; `None` when empty. Raw byte
/// offsets into the active document, always on char boundaries.
pub(super) fn sel_range(st: &ViewerState) -> Option<(usize, usize)> {
    st.sel
        .get()
        .map(|(a, b)| (a.min(b), a.max(b)))
        .filter(|(a, b)| a < b)
}

/// Does this content kind have a selectable document?
pub(super) fn selectable(kind: ContentKind) -> bool {
    matches!(kind, ContentKind::Text | ContentKind::Markdown)
}

/// Run `f` over the active selection document; `None` when the pane has none (or it's empty).
pub(super) fn with_doc<R>(st: &ViewerState, f: impl FnOnce(&str) -> R) -> Option<R> {
    match st.kind.get() {
        ContentKind::Text => {
            let t = st.text.borrow();
            t.as_deref().filter(|s| !s.is_empty()).map(f)
        }
        ContentKind::Markdown => {
            let l = st.md_layout.borrow();
            if l.doc.is_empty() {
                None
            } else {
                Some(f(&l.doc))
            }
        }
        _ => None,
    }
}

/// Document length in bytes (what Ctrl+A selects to).
pub(super) unsafe fn doc_len(hwnd: HWND) -> Option<usize> {
    with_doc(&*state(hwnd), |d| d.len())
}

/// The text Ctrl+C puts on the clipboard: the selection, else the whole document. Markdown text
/// is synthesized with bare `\n`, so it's normalized to CRLF for pasting; text-pane slices keep
/// the file's own line endings verbatim.
pub(super) unsafe fn copy_text(hwnd: HWND) -> Option<String> {
    let st = &*state(hwnd);
    let md = st.kind.get() == ContentKind::Markdown;
    let range = sel_range(st);
    with_doc(st, |doc| {
        // Never trust a slice not to panic (panic=abort kills the viewer) — fall back to all.
        let piece = match range {
            Some((a, b)) => doc.get(a..b).unwrap_or(doc),
            None => doc,
        };
        if md {
            crlf(piece)
        } else {
            piece.to_string()
        }
    })
    .filter(|s| !s.is_empty())
}

/// LF (or CRLF) -> CRLF: what every Windows text field expects on paste.
fn crlf(s: &str) -> String {
    s.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Client point -> document offset for the pane under it. `None` when the kind has no document.
pub(super) unsafe fn hit(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    let st = &*state(hwnd);
    match st.kind.get() {
        ContentKind::Text => text_hit(hwnd, x, y),
        ContentKind::Markdown => md_hit(hwnd, x, y),
        _ => None,
    }
}

/// The word around `off` (double-click selection).
pub(super) unsafe fn word_range(hwnd: HWND, off: usize) -> Option<(usize, usize)> {
    with_doc(&*state(hwnd), |d| highlight::word_at(d, off)).filter(|(a, b)| a < b)
}

// ===== text pane =====

/// Build the text pane's line-start index once per document (cleared on load) — hit-testing runs
/// per mouse-move during a drag, so it must not rescan a multi-MB file for line boundaries.
fn ensure_text_starts(st: &ViewerState) {
    let mut s = st.line_starts.borrow_mut();
    if !s.is_empty() {
        return;
    }
    let t = st.text.borrow();
    let Some(t) = t.as_deref() else { return };
    s.push(0);
    s.extend(
        t.bytes()
            .enumerate()
            .filter(|(_, b)| *b == b'\n')
            .map(|(i, _)| i + 1),
    );
}

/// The line index containing `off`.
fn line_of(starts: &[usize], off: usize) -> usize {
    starts.partition_point(|s| *s <= off).saturating_sub(1)
}

/// Line `li`'s text (no trailing `\r`/`\n`).
fn line_at<'a>(t: &'a str, starts: &[usize], li: usize) -> &'a str {
    let s = starts.get(li).copied().unwrap_or(0);
    let e = starts
        .get(li + 1)
        .map(|n| n.saturating_sub(1))
        .unwrap_or(t.len());
    let line = t.get(s..e).unwrap_or("");
    line.strip_suffix('\r').unwrap_or(line)
}

/// Offset of line `li`'s end (before any `\r`/`\n`).
fn line_end(t: &str, starts: &[usize], li: usize) -> usize {
    starts.get(li).copied().unwrap_or(0) + line_at(t, starts, li).len()
}

unsafe fn text_hit(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    let st = &*state(hwnd);
    ensure_text_starts(st);
    let starts = st.line_starts.borrow();
    let text = st.text.borrow();
    let t = text.as_deref()?;
    let rc = content_rect(hwnd);
    let m = crate::win::dpi_scale(hwnd, 12);
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
        return None;
    }
    let font = mono_font(hwnd);
    let off = highlight::hit_test(
        hdc,
        t,
        &starts,
        font,
        rc.left + m,
        rc.top + m - st.text_scroll.get(),
        x,
        y,
    );
    let _ = DeleteObject(font.into());
    ReleaseDC(Some(hwnd), hdc);
    Some(off)
}

/// One text-pane line's height (the mono font's), for line/page math.
unsafe fn mono_line_h(hwnd: HWND) -> i32 {
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
        return 1;
    }
    let f = mono_font(hwnd);
    let old = SelectObject(hdc, f.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    SelectObject(hdc, old);
    let _ = DeleteObject(f.into());
    ReleaseDC(Some(hwnd), hdc);
    (tm.tmHeight + tm.tmExternalLeading).max(1)
}

// ===== markdown pane =====

/// How far `(x, y)` is from `r` — `(vertical, horizontal)`, so a point picks the nearest LINE
/// first and only then the nearest token on it (an editor-style click in the margin).
fn dist(r: &RECT, x: i32, y: i32) -> (i64, i64) {
    let dy = if y < r.top {
        (r.top - y) as i64
    } else if y >= r.bottom {
        (y - r.bottom + 1) as i64
    } else {
        0
    };
    let dx = if x < r.left {
        (r.left - x) as i64
    } else if x >= r.right {
        (x - r.right + 1) as i64
    } else {
        0
    };
    (dy, dx)
}

unsafe fn md_hit(hwnd: HWND, x: i32, y: i32) -> Option<usize> {
    let st = &*state(hwnd);
    let pick = {
        let hits = st.md_hits.borrow();
        // Prefer the token under the point; else the nearest one — clicking in a margin or
        // between blocks should still land the caret somewhere sensible.
        let inside = hits
            .iter()
            .find(|h| x >= h.rect.left && x < h.rect.right && y >= h.rect.top && y < h.rect.bottom);
        match inside {
            Some(h) => *h,
            None => *hits.iter().min_by_key(|h| dist(&h.rect, x, y))?,
        }
    };
    if x <= pick.text_x {
        return Some(pick.start);
    }
    md_col(hwnd, st, &pick, x)
}

/// The offset inside token `h` nearest client-x `x` (measured with the token's own font).
unsafe fn md_col(hwnd: HWND, st: &ViewerState, h: &SelHit, x: i32) -> Option<usize> {
    with_doc(st, |doc| {
        let t = doc.get(h.start..h.end).unwrap_or("");
        let hdc = GetDC(Some(hwnd));
        if hdc.is_invalid() {
            return h.end;
        }
        let f = super::markdown::font_for(hwnd, h.font);
        let old = SelectObject(hdc, f.into());
        let c = highlight::col_at(hdc, t, x - h.text_x);
        SelectObject(hdc, old);
        let _ = DeleteObject(f.into());
        ReleaseDC(Some(hwnd), hdc);
        h.start + c
    })
}

/// The painted token holding (or nearest to) a document offset. Markdown only records rects for
/// what it DREW, so an off-screen offset resolves to the nearest on-screen token.
fn md_focus_hit(st: &ViewerState, off: usize) -> Option<SelHit> {
    let hits = st.md_hits.borrow();
    hits.iter()
        .find(|h| off >= h.start && off <= h.end)
        .copied()
        .or_else(|| hits.iter().min_by_key(|h| h.start.abs_diff(off)).copied())
}

/// Client-x of the caret at `off` within token `h`.
unsafe fn md_caret_x(hwnd: HWND, st: &ViewerState, h: &SelHit, off: usize) -> i32 {
    if off <= h.start {
        return h.text_x;
    }
    if off >= h.end {
        return h.rect.right;
    }
    with_doc(st, |doc| {
        let t = doc.get(h.start..h.end).unwrap_or("");
        let hdc = GetDC(Some(hwnd));
        if hdc.is_invalid() {
            return h.text_x;
        }
        let f = super::markdown::font_for(hwnd, h.font);
        let old = SelectObject(hdc, f.into());
        let x = h.text_x + highlight::disp_extent(hdc, t, off - h.start);
        SelectObject(hdc, old);
        let _ = DeleteObject(f.into());
        ReleaseDC(Some(hwnd), hdc);
        x
    })
    .unwrap_or(h.text_x)
}

// ===== scrolling =====

/// Scroll the pane by `dy` device px (clamped) and repaint SYNCHRONOUSLY — the Markdown hit
/// rects come from the last paint, so they must be refreshed before we hit-test at a new scroll.
pub(super) unsafe fn scroll_by(hwnd: HWND, dy: i32) {
    let st = &*state(hwnd);
    let desired = st.text_scroll.get().saturating_add(dy);
    if !set_text_scroll(hwnd, desired) {
        return;
    }
    st.toc_sel.set(None); // same rule as a wheel scroll: back to the scroll-derived outline mark
    let _ = UpdateWindow(hwnd);
}

/// Scroll so the caret at `off` is on screen (after a keyboard selection move).
unsafe fn ensure_visible(hwnd: HWND, off: usize) {
    let st = &*state(hwnd);
    let c = content_rect(hwnd);
    let m = crate::win::dpi_scale(hwnd, 12);
    match st.kind.get() {
        ContentKind::Text => {
            ensure_text_starts(st);
            let lh = mono_line_h(hwnd);
            let li = {
                let starts = st.line_starts.borrow();
                line_of(&starts, off) as i32
            };
            let y = c.top + m - st.text_scroll.get() + li * lh;
            if y < c.top {
                scroll_by(hwnd, y - c.top);
            } else if y + lh > c.bottom {
                scroll_by(hwnd, y + lh - c.bottom);
            }
        }
        ContentKind::Markdown => {
            // Markdown only has rects for what it PAINTED, so an off-screen target has nothing
            // to aim at — and `md_focus_hit`'s nearest-hit fallback would return an already
            // visible token and "succeed" without scrolling anywhere.
            let len = with_doc(st, |d| d.len()).unwrap_or(0);
            if off == 0 {
                scroll_by(hwnd, -st.text_scroll.get()); // Ctrl+Shift+Home
                return;
            }
            if off >= len {
                scroll_by(hwnd, st.text_h.get()); // Ctrl+Shift+End (scroll_by clamps)
                return;
            }
            // Otherwise the target is at most a line or two away (a char/word hop), so walk
            // toward it: each scroll repaints synchronously, which records fresh rects.
            let step = crate::win::dpi_scale(hwnd, 24);
            for _ in 0..8 {
                let (exact, dir) = {
                    let hits = st.md_hits.borrow();
                    let exact = hits
                        .iter()
                        .find(|h| off >= h.start && off <= h.end)
                        .copied();
                    let dir = match &exact {
                        Some(_) => 0,
                        None if hits.iter().all(|h| h.start > off) => -1, // it's above us
                        None if hits.iter().all(|h| h.end < off) => 1,    // it's below us
                        None => 0, // painted range straddles it: nothing sensible to do
                    };
                    (exact, dir)
                };
                let dy = match exact {
                    Some(h) if h.rect.top < c.top => h.rect.top - c.top,
                    Some(h) if h.rect.bottom > c.bottom => h.rect.bottom - c.bottom,
                    Some(_) => return, // on screen
                    None if dir != 0 => dir * step,
                    None => return,
                };
                let before = st.text_scroll.get();
                scroll_by(hwnd, dy);
                if st.text_scroll.get() == before {
                    return; // already at that end of the document
                }
            }
        }
        _ => {}
    }
}

// ===== keyboard selection =====

/// Handle a Shift+<key> selection keystroke (arrows / Home / End / PgUp / PgDn, plus the Ctrl
/// variants: word-wise left/right and document Home/End). Returns whether it was handled.
pub(super) unsafe fn extend(hwnd: HWND, vk: u16, ctrl: bool) -> bool {
    let st = &*state(hwnd);
    if !selectable(st.kind.get()) {
        return false;
    }
    let Some(len) = doc_len(hwnd) else {
        return false;
    };
    let (anchor, focus) = match st.sel.get() {
        Some(p) => p,
        // Nothing selected yet: start from the top of the VIEW, not the document — yanking a
        // scrolled reader back to line 1 on the first Shift+Down would be nonsense.
        None => {
            let o = first_visible_off(hwnd).unwrap_or(0);
            (o, o)
        }
    };
    let Some(nf) = move_focus(hwnd, vk, ctrl, focus, len) else {
        return false;
    };
    st.sel.set(Some((anchor, nf)));
    ensure_visible(hwnd, nf);
    let c = content_rect(hwnd);
    let _ = InvalidateRect(Some(hwnd), Some(&c), false);
    true
}

/// Where a keyboard selection starts when nothing is selected: the first offset visible at the
/// top of the pane.
unsafe fn first_visible_off(hwnd: HWND) -> Option<usize> {
    let st = &*state(hwnd);
    match st.kind.get() {
        ContentKind::Text => {
            ensure_text_starts(st);
            let lh = mono_line_h(hwnd);
            let starts = st.line_starts.borrow();
            let li = (st.text_scroll.get() / lh).clamp(0, starts.len() as i32 - 1) as usize;
            starts.get(li).copied()
        }
        ContentKind::Markdown => {
            let c = content_rect(hwnd);
            let hits = st.md_hits.borrow();
            hits.iter()
                .filter(|h| h.rect.bottom > c.top)
                .map(|h| h.start)
                .min()
        }
        _ => None,
    }
}

/// The new focus offset for a selection keystroke, or `None` if the key isn't one.
unsafe fn move_focus(hwnd: HWND, vk: u16, ctrl: bool, focus: usize, len: usize) -> Option<usize> {
    let st = &*state(hwnd);
    if ctrl {
        // Document ends + word-wise horizontal movement are the same in both panes (pure text).
        if vk == VK_HOME.0 {
            return Some(0);
        }
        if vk == VK_END.0 {
            return Some(len);
        }
        if vk == VK_LEFT.0 {
            return with_doc(st, |d| prev_word(d, focus));
        }
        if vk == VK_RIGHT.0 {
            return with_doc(st, |d| next_word(d, focus));
        }
    }
    if vk == VK_LEFT.0 {
        return with_doc(st, |d| prev_char(d, focus));
    }
    if vk == VK_RIGHT.0 {
        return with_doc(st, |d| next_char(d, focus));
    }
    match st.kind.get() {
        ContentKind::Text => text_move(hwnd, vk, focus),
        ContentKind::Markdown => md_move(hwnd, vk, focus),
        _ => None,
    }
}

/// Vertical / line-end movement in the mono text pane: Up/Down keep the caret's DISPLAY column
/// (so it tracks visually through tabs and wide chars, like an editor).
unsafe fn text_move(hwnd: HWND, vk: u16, focus: usize) -> Option<usize> {
    let st = &*state(hwnd);
    ensure_text_starts(st);
    let lh = mono_line_h(hwnd);
    let c = content_rect(hwnd);
    let text = st.text.borrow();
    let t = text.as_deref()?;
    let starts = st.line_starts.borrow();
    let li = line_of(&starts, focus);
    if vk == VK_HOME.0 {
        return starts.get(li).copied();
    }
    if vk == VK_END.0 {
        return Some(line_end(t, &starts, li));
    }
    let page = ((c.bottom - c.top) / lh).max(1);
    let step: i64 = match vk {
        v if v == VK_UP.0 => -1,
        v if v == VK_DOWN.0 => 1,
        v if v == VK_PRIOR.0 => -(page as i64),
        v if v == VK_NEXT.0 => page as i64,
        _ => return None,
    };
    let tl = (li as i64 + step).clamp(0, starts.len() as i64 - 1) as usize;
    if tl == li {
        // Already on the first/last line: run to that end rather than doing nothing.
        return Some(if step < 0 {
            starts[li]
        } else {
            line_end(t, &starts, li)
        });
    }
    let hdc = GetDC(Some(hwnd));
    if hdc.is_invalid() {
        return None;
    }
    let f = mono_font(hwnd);
    let old = SelectObject(hdc, f.into());
    let x = highlight::disp_extent(hdc, line_at(t, &starts, li), focus - starts[li]);
    let col = highlight::col_at(hdc, line_at(t, &starts, tl), x);
    SelectObject(hdc, old);
    let _ = DeleteObject(f.into());
    ReleaseDC(Some(hwnd), hdc);
    Some(starts[tl] + col)
}

/// Vertical / line-end movement in the Markdown pane, driven by the painted token rects (there
/// is no line grid — it's a wrapped proportional flow).
unsafe fn md_move(hwnd: HWND, vk: u16, focus: usize) -> Option<usize> {
    let st = &*state(hwnd);
    let h = md_focus_hit(st, focus)?;
    if vk == VK_HOME.0 || vk == VK_END.0 {
        let hits = st.md_hits.borrow();
        let top = h.rect.top;
        let row = || hits.iter().filter(|o| (o.rect.top - top).abs() <= 1);
        return if vk == VK_HOME.0 {
            row().map(|o| o.start).min()
        } else {
            row().map(|o| o.end).max()
        };
    }
    let lh = (h.rect.bottom - h.rect.top).max(1);
    let c = content_rect(hwnd);
    let page = (c.bottom - c.top - lh).max(lh);
    let dy = match vk {
        v if v == VK_UP.0 => -lh,
        v if v == VK_DOWN.0 => lh,
        v if v == VK_PRIOR.0 => -page,
        v if v == VK_NEXT.0 => page,
        _ => return None,
    };
    let x = md_caret_x(hwnd, st, &h, focus);
    let ty = h.rect.top + lh / 2 + dy;
    if ty >= c.top && ty < c.bottom {
        return md_hit(hwnd, x, ty);
    }
    // The target line is off the pane and therefore has no hit rect — scroll it into view (a
    // synchronous repaint records the rects) and hit-test where it landed.
    let over = if ty < c.top {
        ty - c.top
    } else {
        ty - c.bottom + 1
    };
    scroll_by(hwnd, over);
    md_hit(hwnd, x, ty - over)
}

// ===== pure offset math =====

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn prev_char(d: &str, off: usize) -> usize {
    d.get(..off)
        .and_then(|s| s.char_indices().next_back())
        .map(|(i, _)| i)
        .unwrap_or(0)
}

fn next_char(d: &str, off: usize) -> usize {
    d.get(off..)
        .and_then(|s| s.chars().next())
        .map(|c| off + c.len_utf8())
        .unwrap_or(off)
}

/// The char before / at `off` (`None` at the respective end of the document).
fn char_before(d: &str, off: usize) -> Option<char> {
    d.get(..off).and_then(|s| s.chars().next_back())
}
fn char_at(d: &str, off: usize) -> Option<char> {
    d.get(off..).and_then(|s| s.chars().next())
}

/// Ctrl+Left: back over this line's non-word run, then over the word itself. A hop never
/// swallows more than one line break — sitting just after one, it steps back onto the previous
/// line's end and stops there (the mirror of [`next_word`], so the two agree).
fn prev_word(d: &str, off: usize) -> usize {
    let mut i = off;
    if char_before(d, i) == Some('\n') {
        return prev_char(d, i);
    }
    while i > 0 && char_before(d, i).is_some_and(|c| !is_word(c) && c != '\n') {
        let n = prev_char(d, i);
        if n == i {
            break;
        }
        i = n;
    }
    while i > 0 && char_before(d, i).is_some_and(is_word) {
        let n = prev_char(d, i);
        if n == i {
            break;
        }
        i = n;
    }
    i
}

/// Ctrl+Right: forward over the current word, then over the non-word run after it, stopping at
/// the line's end. Sitting ON that line break, the next hop steps over it to the following
/// line's first word — otherwise a line end would be a dead stop you could never hop past.
fn next_word(d: &str, off: usize) -> usize {
    let mut i = off;
    if char_at(d, i) == Some('\n') {
        i = next_char(d, i);
    } else {
        while char_at(d, i).is_some_and(is_word) {
            let n = next_char(d, i);
            if n == i {
                break;
            }
            i = n;
        }
    }
    while char_at(d, i).is_some_and(|c| !is_word(c) && c != '\n') {
        let n = next_char(d, i);
        if n == i {
            break;
        }
        i = n;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::{next_word, prev_word};

    #[test]
    fn word_hops_land_on_boundaries() {
        let d = "let grüße = vec![1, 42];\nnext_line";
        // forward: over the word, then the spaces/punctuation after it
        assert_eq!(next_word(d, 0), d.find("grüße").unwrap()); // "let" + space
        assert_eq!(next_word(d, d.len()), d.len()); // end of doc is a fixed point
                                                    // a newline stops a forward hop (so Ctrl+Shift+Right doesn't skip whole lines)
        let nl = d.find('\n').unwrap();
        assert_eq!(next_word(d, d.find("42").unwrap()), nl);
        // backward: over the leading punctuation/space, then the word
        assert_eq!(
            prev_word(d, d.find("= vec").unwrap()),
            d.find("grüße").unwrap()
        );
        assert_eq!(prev_word(d, 0), 0);
        // mid-word backward lands at the word's start (multi-byte chars included)
        let g = d.find("grüße").unwrap();
        assert_eq!(prev_word(d, g + "grü".len()), g);
    }

    /// A line break must be a stop, never a dead end (forward) or a whole-gap swallow
    /// (backward) — the two directions have to agree or the selection can't be walked back.
    #[test]
    fn word_hops_cross_exactly_one_line_break() {
        let d = "hello world\nfoo bar";
        let nl = d.find('\n').unwrap(); // 11
                                        // forward: ... -> line end -> (next press) the next line's first word. Never stuck.
        assert_eq!(next_word(d, 6), nl); // over "world", stop at the break
        assert_eq!(next_word(d, nl), nl + 1); // ON the break -> start of "foo"
        assert_eq!(next_word(d, nl + 1), d.find("bar").unwrap()); // then normally onward
                                                                  // backward mirrors it: just after the break -> the previous line's end, and no further
        assert_eq!(prev_word(d, nl + 1), nl);
        assert_eq!(prev_word(d, nl), d.find("world").unwrap());
        // blank lines are crossed one at a time, not swallowed whole
        let b = "para one\n\n\npara two";
        let p2 = b.find("para two").unwrap();
        assert_eq!(prev_word(b, p2), p2 - 1);
        assert_eq!(prev_word(b, p2 - 1), p2 - 2);
        // and every hop strictly makes progress (no fixed point except the document ends)
        for off in 0..=d.len() {
            if !d.is_char_boundary(off) {
                continue;
            }
            assert!(
                next_word(d, off) > off || off == d.len(),
                "next_word stuck at {off}"
            );
            assert!(
                prev_word(d, off) < off || off == 0,
                "prev_word stuck at {off}"
            );
        }
    }
}
