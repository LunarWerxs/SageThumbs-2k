//! In-document find (Ctrl+F) for the text and Markdown panes.
//!
//! The whole feature rides machinery that already exists. A "match" is just a byte range in the
//! same *selection document* [`super::selection`] defines, so jumping to one is `st.sel = range`
//! plus the selection module's own scroll-into-view: the highlight, the copy behaviour and the
//! Markdown-versus-text coordinate difference are all inherited rather than re-implemented.
//!
//! Two deliberate limits, both about staying cheap on a pane that can hold a 5 MB document:
//!
//! - **Case folding is ASCII-only.** `to_ascii_lowercase` is the one fold that cannot change a
//!   string's byte length, so offsets into the folded haystack stay valid in the real document.
//!   A Unicode fold would shift them (`İ` folds to two chars) and every match would land in the
//!   wrong place, which is a much worse bug than "Straße does not match STRASSE".
//! - **The folded haystack is cached** and only rebuilt when the document changes, so typing into
//!   the box re-scans but never re-allocates the document.

use std::cell::RefCell;

use windows::Win32::Foundation::{COLORREF, HWND, RECT};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, InvalidateRect, LineTo,
    MoveToEx, SelectObject, SetBkMode, SetTextColor, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE,
    DT_VCENTER, HDC, HGDIOBJ, PS_SOLID, TRANSPARENT,
};

use super::selection;
use super::window::{state, ViewerState};

/// Height of the find bar in 96-dpi design px (DPI-scaled at use).
pub(in crate::preview) const FIND_H: i32 = 30;

/// The find bar's state. Lives for the window's lifetime: the query deliberately survives closing
/// the bar and switching files, so Ctrl+F, Ctrl+F reopens on what you last looked for.
#[derive(Default)]
pub(in crate::preview) struct FindState {
    pub open: bool,
    pub query: String,
    /// Byte offsets of every match in the current document, ascending.
    hits: Vec<usize>,
    /// Index into `hits` of the match currently selected; `None` when there are none.
    cur: Option<usize>,
    /// ASCII-folded copy of the document the `hits` were found in, plus the decode generation it
    /// came from. A mismatched generation means a new file loaded and both are stale.
    hay: String,
    hay_gen: u64,
    /// Whether `hay_gen` has ever been set (generation 0 is a legitimate value).
    primed: bool,
}

impl FindState {
    /// "3/17", or empty when there is nothing to count.
    fn counter(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        if self.hits.is_empty() {
            return crate::win::t("find_no_results").to_string();
        }
        format!(
            "{}/{}",
            self.cur.map(|i| i + 1).unwrap_or(0),
            self.hits.len()
        )
    }
}

/// The find bar's rect: a strip across the top of the content area. Returns an empty rect when the
/// bar is closed, which is also what makes [`content_rect`] able to call this without recursing
/// into a meaningful value.
pub(in crate::preview) unsafe fn bar_rect(hwnd: HWND) -> RECT {
    let st = &*state(hwnd);
    if !st.find.borrow().open {
        return RECT::default();
    }
    let cap = crate::win::dpi_scale(hwnd, super::window::CAPTION_H);
    let h = crate::win::dpi_scale(hwnd, FIND_H);
    let mut r = RECT::default();
    let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut r);
    RECT {
        left: 0,
        top: cap,
        right: r.right,
        bottom: cap + h,
    }
}

/// Whether the current content has something to search.
pub(in crate::preview) unsafe fn available(st: &ViewerState) -> bool {
    selection::selectable(st.kind.get())
}

/// Ctrl+F. Opens the bar on a searchable pane; pressing it again while open steps to the next
/// match, which is what a browser does and what muscle memory expects.
pub(in crate::preview) unsafe fn toggle(hwnd: HWND) {
    let st = &*state(hwnd);
    if !available(st) {
        return;
    }
    let already = st.find.borrow().open;
    if already {
        step(hwnd, true);
        return;
    }
    st.find.borrow_mut().open = true;
    research(hwnd);
    // The pane just got shorter, so the content has to re-lay out, not just repaint the strip.
    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = super::window::clamp_text_scroll(hwnd);
}

/// Close the bar (Esc). The query is kept for the next Ctrl+F.
pub(in crate::preview) unsafe fn close(hwnd: HWND) {
    let st = &*state(hwnd);
    if !st.find.borrow().open {
        return;
    }
    {
        let mut f = st.find.borrow_mut();
        f.open = false;
        f.hits.clear();
        f.cur = None;
    }
    let _ = InvalidateRect(Some(hwnd), None, false);
    let _ = super::window::clamp_text_scroll(hwnd);
}

/// A typed character while the bar has the keyboard (routed from `WM_CHAR`). Returns whether it
/// was consumed.
pub(in crate::preview) unsafe fn on_char(hwnd: HWND, ch: u32) -> bool {
    let st = &*state(hwnd);
    if !st.find.borrow().open {
        return false;
    }
    match ch {
        // Backspace.
        0x08 => {
            let mut f = st.find.borrow_mut();
            f.query.pop();
        }
        // Enter and Esc arrive as WM_KEYDOWN too; let that handler own them.
        0x0D | 0x1B => return false,
        // Anything else printable.
        c if c >= 0x20 => match char::from_u32(c) {
            Some(c) => st.find.borrow_mut().query.push(c),
            None => return false,
        },
        _ => return false,
    }
    research(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
    true
}

/// A key press while the bar is open. Returns whether it was consumed.
pub(in crate::preview) unsafe fn on_key(hwnd: HWND, vk: u16, shift: bool) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_F3, VK_RETURN, VK_SPACE};
    let st = &*state(hwnd);
    // F3 works even with the bar closed, so a search survives Esc and can be resumed.
    if vk == VK_F3.0 {
        if st.find.borrow().query.is_empty() || !available(st) {
            return false;
        }
        if !st.find.borrow().open {
            toggle(hwnd);
            return true;
        }
        step(hwnd, !shift);
        return true;
    }
    if !st.find.borrow().open {
        return false;
    }
    if vk == VK_ESCAPE.0 {
        close(hwnd);
        return true;
    }
    if vk == VK_RETURN.0 {
        step(hwnd, !shift);
        return true;
    }
    // Manual mode closes the viewer on a bare Space (window.rs), which runs AFTER this
    // returns false. A typed space in the query would otherwise close the viewer mid-query
    // before WM_CHAR ever delivers the character. Consume the keydown here (WM_CHAR still
    // fires separately and inserts the space via on_char) so it never reaches that handler.
    if vk == VK_SPACE.0 {
        return true;
    }
    false
}

/// Re-run the search for the current query and jump to the first match at or after where the user
/// is already looking, so typing does not yank a scrolled reader back to the top of the file.
unsafe fn research(hwnd: HWND) {
    let st = &*state(hwnd);
    ensure_hay(hwnd);
    let from = resume_from(st.sel.get());
    {
        let mut f = st.find.borrow_mut();
        f.hits.clear();
        f.cur = None;
        if f.query.is_empty() {
            return;
        }
        let needle = f.query.to_ascii_lowercase();
        // `match_indices` finds OVERLAPPING-free occurrences left to right, which is what a find
        // box shows; the folded haystack is byte-for-byte aligned with the real document (see the
        // module docs), so these offsets index straight into it.
        let hits: Vec<usize> = f.hay.match_indices(&needle).map(|(i, _)| i).collect();
        f.hits = hits;
        f.cur = f
            .hits
            .iter()
            .position(|o| *o >= from)
            .or(if f.hits.is_empty() { None } else { Some(0) });
    }
    goto_current(hwnd);
}

/// The byte offset a fresh search should resume from. `sel` is stored unordered — (anchor,
/// focus) — so a right-to-left drag has anchor > focus; taking the raw first element would
/// then resume from the far (larger) end of the selection instead of past all of it. The
/// normalized end (the max of the pair) is correct regardless of drag direction.
fn resume_from(sel: Option<(usize, usize)>) -> usize {
    sel.map(|(a, b)| a.max(b)).unwrap_or(0)
}

/// Move to the next/previous match, wrapping at both ends.
unsafe fn step(hwnd: HWND, forward: bool) {
    let st = &*state(hwnd);
    {
        let mut f = st.find.borrow_mut();
        let n = f.hits.len();
        if n == 0 {
            return;
        }
        f.cur = Some(match f.cur {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => 0,
        });
    }
    goto_current(hwnd);
    let _ = InvalidateRect(Some(hwnd), None, false);
}

/// Select the current match and scroll it into view. Selecting it is the whole highlight: the
/// panes already paint `st.sel`, and Ctrl+C already copies it.
unsafe fn goto_current(hwnd: HWND) {
    let st = &*state(hwnd);
    let range = {
        let f = st.find.borrow();
        match f.cur.and_then(|i| f.hits.get(i)).copied() {
            Some(o) => Some((o, o + f.query.len())),
            None => None,
        }
    };
    let Some((a, b)) = range else {
        st.sel.set(None);
        return;
    };
    // Never hand the panes a range that could panic a slice. An ASCII-folded search cannot land
    // mid-character, but a document that changed under us could still make this stale.
    let ok = selection::with_doc(st, |d| {
        b <= d.len() && d.is_char_boundary(a) && d.is_char_boundary(b)
    })
    .unwrap_or(false);
    if !ok {
        st.sel.set(None);
        return;
    }
    st.sel.set(Some((a, b)));
    selection::ensure_visible(hwnd, a);
}

/// Rebuild the ASCII-folded haystack if the document changed since it was built.
unsafe fn ensure_hay(hwnd: HWND) {
    let st = &*state(hwnd);
    let gen = st.decode_gen.get();
    {
        let f = st.find.borrow();
        if f.primed && f.hay_gen == gen {
            return;
        }
    }
    let mut folded = selection::with_doc(st, |d| d.to_ascii_lowercase()).unwrap_or_default();
    if folded.is_empty() && st.kind.get() == super::window::ContentKind::Markdown {
        // The Markdown pane's selection document is a by-product of RENDERING, so it does not
        // exist until the pane has painted at least once. Searching before that quietly returns
        // "no results" for a document that plainly contains the word. Force one synchronous paint
        // and read it again; `selection` uses the same trick for its hit rects.
        //
        // The `find` borrow is deliberately not held across this: the paint itself reads the find
        // state to draw the bar.
        let _ = InvalidateRect(Some(hwnd), None, false);
        let _ = windows::Win32::Graphics::Gdi::UpdateWindow(hwnd);
        folded = selection::with_doc(st, |d| d.to_ascii_lowercase()).unwrap_or_default();
    }
    let mut f = st.find.borrow_mut();
    f.hay = folded;
    f.hay_gen = gen;
    f.primed = true;
}

/// A load STARTED: drop the cached haystack and hits.
///
/// Deliberately does NOT re-search. It runs at the very top of `loader::load`, before the new
/// file's kind/text/layout exist, so a search here would read the OUTGOING document (or, for the
/// text pane, nothing at all) and then cache that under the INCOMING document's generation,
/// freezing a wrong result for the rest of that file's life. [`refresh`] is the other half.
pub(in crate::preview) unsafe fn on_document_changed(hwnd: HWND) {
    let mut f = (*state(hwnd)).find.borrow_mut();
    f.primed = false;
    f.hits.clear();
    f.cur = None;
}

/// A load FINISHED: re-run the open search against the content that now actually exists. Called at
/// every exit of `loader::load`, including its early returns.
pub(in crate::preview) unsafe fn refresh(hwnd: HWND) {
    let st = &*state(hwnd);
    if !st.find.borrow().open {
        return;
    }
    // The bar can outlive the kind that justified it (←/→ from a document onto an image). Close it
    // rather than leaving a dead strip permanently eating the content area.
    if !available(st) {
        close(hwnd);
        return;
    }
    research(hwnd);
}

/// Paint the bar: the typed query with a caret on the left, the match counter on the right.
pub(in crate::preview) unsafe fn paint(hwnd: HWND, hdc: HDC, text: u32, subtle: u32) {
    let st = &*state(hwnd);
    if !st.find.borrow().open {
        return;
    }
    let r = bar_rect(hwnd);
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let bg = CreateSolidBrush(COLORREF(crate::dark::DARK_BG().0));
    FillRect(hdc, &r, bg);
    let _ = DeleteObject(bg.into());
    let pen = CreatePen(PS_SOLID, 1, COLORREF(crate::dark::BORDER().0));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, r.left, r.bottom - 1, None);
    let _ = LineTo(hdc, r.right, r.bottom - 1);
    SelectObject(hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    let f = st.find.borrow();
    let font = crate::win::gui_font_for(hwnd);
    let oldf = SelectObject(hdc, font.into());
    SetBkMode(hdc, TRANSPARENT);

    // Query, with a block caret so an empty box still looks like somewhere you can type.
    SetTextColor(hdc, COLORREF(text));
    let shown = format!("Find: {}|", f.query);
    let mut w: Vec<u16> = shown.encode_utf16().collect();
    let mut tr = RECT {
        left: r.left + sc(10),
        top: r.top,
        right: r.right - sc(110),
        bottom: r.bottom,
    };
    DrawTextW(
        hdc,
        &mut w,
        &mut tr,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
    );

    // Counter, right-aligned. `subtle` unless the search found nothing, which is worth noticing.
    let counter = f.counter();
    if !counter.is_empty() {
        SetTextColor(hdc, COLORREF(if f.hits.is_empty() { text } else { subtle }));
        let mut cw: Vec<u16> = counter.encode_utf16().collect();
        let mut cr = RECT {
            left: r.right - sc(106),
            top: r.top,
            right: r.right - sc(10),
            bottom: r.bottom,
        };
        DrawTextW(
            hdc,
            &mut cw,
            &mut cr,
            DT_RIGHT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }
    SelectObject(hdc, oldf);
}

/// The find state a fresh window starts with.
pub(in crate::preview) fn new_state() -> RefCell<FindState> {
    RefCell::new(FindState::default())
}

#[cfg(test)]
mod tests {
    use super::resume_from;

    /// `sel` is stored unordered as (anchor, focus), the anchor being wherever the drag
    /// started. The old code read the raw first tuple element (the anchor) directly: for a
    /// left-to-right drag (anchor is the SMALLER offset) that resumed from the start of the
    /// selection instead of past it. The fix takes the normalized end (the larger offset)
    /// regardless of which direction the drag went, so both directions agree.
    #[test]
    fn resume_from_uses_normalized_end_not_raw_anchor() {
        // Left-to-right drag: anchor(5) < focus(20). Raw-anchor (the old bug) would give 5;
        // the normalized end is 20.
        assert_eq!(resume_from(Some((5, 20))), 20);
        // Right-to-left drag: anchor(20) > focus(5). Normalized end is still the larger
        // offset, 20, independent of which element of the pair is the anchor.
        assert_eq!(resume_from(Some((20, 5))), 20);
        assert_eq!(resume_from(None), 0);
    }

    /// The property the whole design rests on: ASCII folding never moves a byte, so an offset found
    /// in the folded copy indexes the same character in the original. A Unicode fold does not have
    /// this property, which is why it is not used.
    #[test]
    fn ascii_fold_preserves_byte_offsets() {
        for s in [
            "Hello World",
            "grüße GRÜSSE",
            "ΑΒΓ αβγ",
            "日本語 Text",
            "İstanbul",
            "",
        ] {
            assert_eq!(
                s.to_ascii_lowercase().len(),
                s.len(),
                "fold moved bytes: {s}"
            );
        }
        // And a match in the folded copy really does line up in the original.
        let doc = "grüße World, hello WORLD";
        let hay = doc.to_ascii_lowercase();
        let hits: Vec<usize> = hay.match_indices("world").map(|(i, _)| i).collect();
        assert_eq!(hits.len(), 2);
        for h in hits {
            assert!(doc.is_char_boundary(h));
            assert!(doc[h..].to_ascii_lowercase().starts_with("world"));
        }
    }
}
