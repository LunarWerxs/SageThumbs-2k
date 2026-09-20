//! Mouse to text: hit-testing, display-width measurement and word boundaries for selection in the code view.

use super::*;

/// Beyond this many UTF-16 units a line is far past any real pane width — stop measuring.
pub(super) const MEASURE_CAP: usize = 16_384;

/// Pixel width of the display prefix (tabs expanded) equivalent to the raw line's first
/// `raw_to` bytes, measured with the currently selected font.
pub(in super::super) unsafe fn disp_extent(hdc: HDC, line: &str, raw_to: usize) -> i32 {
    if raw_to == 0 {
        return 0;
    }
    let mut w16: Vec<u16> = Vec::with_capacity(raw_to.min(MEASURE_CAP) + 4);
    for (i, c) in line.char_indices() {
        if i >= raw_to || w16.len() >= MEASURE_CAP {
            break;
        }
        if c == '\t' {
            w16.extend_from_slice(&[b' ' as u16; 4]);
        } else {
            let mut buf = [0u16; 2];
            w16.extend_from_slice(c.encode_utf16(&mut buf));
        }
    }
    if w16.is_empty() {
        return 0;
    }
    let mut sz = SIZE::default();
    let _ = GetTextExtentPoint32W(hdc, &w16, &mut sz);
    sz.cx
}

/// Map a client-space point to the raw byte offset in `text` under it — the inverse of
/// [`paint_lines`]' layout (same gutter, tab expansion, and line metrics). `x0`/`y0` are the
/// layout origin passed to `paint_lines` (`y0` already carries the scroll offset). `starts` is
/// the document's line-start byte index (one entry per line, first is 0) so a hit on a big file
/// never rescans the text — this runs on EVERY mouse-move during a selection drag. Out-of-range
/// points clamp to the nearest valid position; the result is always on a char boundary.
#[allow(clippy::too_many_arguments)]
pub(in super::super) unsafe fn hit_test(
    hdc: HDC,
    text: &str,
    starts: &[usize],
    font: HFONT,
    x0: i32,
    y0: i32,
    x: i32,
    y: i32,
) -> usize {
    let old = SelectObject(hdc, font.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let line_h = (tm.tmHeight + tm.tmExternalLeading).max(1);
    let total_lines = starts.len().max(1);
    let char_w = tm.tmAveCharWidth.max(1);
    let digits = total_lines.to_string().len() as i32;
    let gutter_w = digits * char_w + char_w * 2; // mirrors paint_lines' gutter math
    let code_x = x0 + gutter_w;

    let li = ((y - y0).div_euclid(line_h) as i64).clamp(0, total_lines as i64 - 1) as usize;
    let line_start = starts.get(li).copied().unwrap_or(0);
    let line_end = starts
        .get(li + 1)
        .map(|s| s.saturating_sub(1))
        .unwrap_or(text.len());
    let line = text.get(line_start..line_end).unwrap_or("");
    let line = line.strip_suffix('\r').unwrap_or(line);
    let off = if x <= code_x {
        line_start
    } else {
        line_start + col_at(hdc, line, x - code_x)
    };
    SelectObject(hdc, old);
    off
}

/// The raw byte offset within `line` whose display-x is nearest `dx` px past the code column's
/// left edge, snapping to the nearest character boundary like an editor caret. ONE GDI call:
/// `GetTextExtentExPointW` fills every display prefix's cumulative width, then a measure-free
/// walk over the raw chars picks the boundary (per-char re-measuring would be O(n²) per
/// mouse-move — [`disp_extent`] on the paint side is the same single-measure discipline).
pub(in super::super) unsafe fn col_at(hdc: HDC, line: &str, dx: i32) -> usize {
    let (w16, raw_end) = display_units(line);
    if w16.is_empty() {
        return 0;
    }
    let Some(dxs) = measure_display_extents(hdc, &w16) else {
        return 0;
    };
    nearest_char_boundary(line, &w16, &raw_end, &dxs, dx)
}

/// Display text (tabs → 4 spaces) + a parallel map: display unit → raw byte END of its
/// char. Stops at [`MEASURE_CAP`] display units — way past any real pane width.
pub(super) fn display_units(line: &str) -> (Vec<u16>, Vec<usize>) {
    let mut w16: Vec<u16> = Vec::new();
    let mut raw_end: Vec<usize> = Vec::new();
    for (i, c) in line.char_indices() {
        if w16.len() >= MEASURE_CAP {
            break;
        }
        let e = i + c.len_utf8();
        if c == '\t' {
            for _ in 0..4 {
                w16.push(b' ' as u16);
                raw_end.push(e);
            }
        } else {
            let mut buf = [0u16; 2];
            for u in c.encode_utf16(&mut buf) {
                w16.push(*u);
                raw_end.push(e);
            }
        }
    }
    (w16, raw_end)
}

/// ONE GDI call filling every display prefix's cumulative width. None on GDI failure.
pub(super) unsafe fn measure_display_extents(hdc: HDC, w16: &[u16]) -> Option<Vec<i32>> {
    let mut dxs = vec![0i32; w16.len()];
    let mut sz = SIZE::default();
    // lpnFit None → nMaxExtent is ignored and every partial extent is filled.
    if !GetTextExtentExPointW(
        hdc,
        PCWSTR(w16.as_ptr()),
        w16.len() as i32,
        0,
        None,
        Some(dxs.as_mut_ptr()),
        &mut sz,
    )
    .as_bool()
    {
        return None;
    }
    Some(dxs)
}

/// Walk raw chars via the display map: each char covers display span [left, right) —
/// snap to the nearer edge of the char containing `dx`.
pub(super) fn nearest_char_boundary(
    line: &str,
    w16: &[u16],
    raw_end: &[usize],
    dxs: &[i32],
    dx: i32,
) -> usize {
    let mut left = 0i32;
    let mut d0 = 0usize;
    while d0 < w16.len() {
        let e = raw_end[d0];
        let mut d1 = d0;
        while d1 + 1 < w16.len() && raw_end[d1 + 1] == e {
            d1 += 1; // group the units of one raw char (tab's 4 spaces, surrogate pair)
        }
        let right = dxs[d1];
        if right >= dx {
            let start = if d0 == 0 { 0 } else { raw_end[d0 - 1] };
            return if dx - left <= right - dx { start } else { e };
        }
        left = right;
        d0 = d1 + 1;
    }
    if w16.len() >= MEASURE_CAP {
        raw_end.last().copied().unwrap_or(line.len())
    } else {
        line.len()
    }
}

/// The word range around byte offset `off` (double-click selection): a run of alphanumerics/`_`;
/// any other char selects just itself; a line break selects nothing. Raw byte offsets.
pub(in super::super) fn word_at(text: &str, off: usize) -> (usize, usize) {
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    match text.get(off..).and_then(|s| s.chars().next()) {
        Some(c) if is_word(c) => {
            let start = text[..off]
                .char_indices()
                .rev()
                .take_while(|(_, c)| is_word(*c))
                .last()
                .map(|(i, _)| i)
                .unwrap_or(off);
            let end = off
                + text[off..]
                    .char_indices()
                    .take_while(|(_, c)| is_word(*c))
                    .last()
                    .map(|(i, c)| i + c.len_utf8())
                    .unwrap_or(0);
            (start, end)
        }
        Some(c) if c != '\n' && c != '\r' => (off, off + c.len_utf8()),
        _ => (off, off),
    }
}
