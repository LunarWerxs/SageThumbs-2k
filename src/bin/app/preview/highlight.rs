//! Lightweight, zero-dependency syntax highlighting for the Quick preview viewer's code display
//! (code files + markdown fenced code blocks). A single-pass per-line lexer per language — line/
//! block comments (with cross-line state), string literals, numbers, and a per-language keyword
//! set — NOT a real parser. Deliberately small (no syntect / onig / regex). Colours come from the
//! theme (`dark.rs`). Keyword tables are intentionally incomplete: enough to "look colourized",
//! not to be a grammar.

use std::borrow::Cow;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, ExtTextOutW, FillRect, GetTextExtentExPointW,
    GetTextExtentPoint32W, GetTextMetricsW, SelectObject, SetTextColor, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, ETO_CLIPPED, HDC, HFONT, TEXTMETRICW,
};

use super::selection::{FontSpec, SelHit};

mod keywords;
mod lex;

pub(in crate::preview) use lex::*;

/// Selection wiring for a [`paint_lines`] call that belongs to a bigger document (a Markdown
/// fenced code block): where its text starts in the selection document, and the collector its
/// drawn lines record their hit rects into. The standalone text pane passes `None` — it
/// hit-tests analytically via [`hit_test`] instead.
pub(super) struct LineSel<'a> {
    pub hits: &'a mut Vec<SelHit>,
    pub base: usize,
    pub spec: FontSpec,
}

/// Theme-resolved code colours (plain uses the caller's `fg`).
struct Colors {
    plain: u32,
    comment: u32,
    string: u32,
    num: u32,
    keyword: u32,
}
impl Colors {
    fn of(&self, t: Tag) -> u32 {
        match t {
            Tag::Plain => self.plain,
            Tag::Comment => self.comment,
            Tag::Str => self.string,
            Tag::Num => self.num,
            Tag::Keyword => self.keyword,
        }
    }
}

/// Draw `text` as syntax-highlighted monospace lines starting at `(x, y0)`, one line per source
/// line, each run clipped to `[x, x+width]` (long code lines clip at the pane edge rather than
/// wrapping — normal for a code view). Lines fully outside `[clip_top, clip_bottom)` are not drawn
/// (scroll culling) but are still lexed so cross-line block-comment state stays correct. `font`
/// must be the mono font. Returns the total content height. `Lang::Plain` draws every run in `fg`.
/// `sel` is a normalized (start < end) RAW byte range into `text`; the covered glyphs get a
/// selection-background fill behind them ([`hit_test`] is the inverse mapping).
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn paint_lines(
    hdc: HDC,
    text: &str,
    lang: Lang,
    x: i32,
    y0: i32,
    width: i32,
    clip_top: i32,
    clip_bottom: i32,
    font: HFONT,
    fg: u32,
    sel: Option<(usize, usize)>,
    mut sink: Option<&mut LineSel>,
) -> i32 {
    let colors = Colors {
        plain: fg,
        comment: crate::dark::CODE_COMMENT().0,
        string: crate::dark::CODE_STRING().0,
        num: crate::dark::CODE_NUMBER().0,
        keyword: crate::dark::CODE_KEYWORD().0,
    };
    let sp = spec(lang);
    let old = SelectObject(hdc, font.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let line_h = tm.tmHeight + tm.tmExternalLeading;

    // Left gutter: right-aligned 1-based line numbers in a muted colour, like QuickLook / an editor.
    // Its width is sized to the digit count of the last line, so the code column shifts right by it.
    let total_lines = text.split('\n').count().max(1);
    let char_w = tm.tmAveCharWidth.max(1);
    let digits = total_lines.to_string().len() as i32;
    let gutter_pad = char_w; // gap between the numbers and the code
    let gutter_w = digits * char_w + gutter_pad * 2;
    let code_x = x + gutter_w;
    let code_right = x + width; // the code column ends at the same right edge as before
    let gutter_fg = crate::dark::HEADER_TEXT().0;
    let sel_bg = crate::dark::SEL_BG().0;

    let mut in_block = false;
    let mut y = y0;
    let mut line_no = 0usize;
    let mut line_start = 0usize; // raw byte offset of this line's first char in `text`
    for raw in text.split('\n') {
        line_no += 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        // `ExtTextOutW` doesn't expand tabs (unlike the plain path's DT_EXPANDTABS), so tab-indented
        // code (Go, Makefiles) would collapse its indentation — expand per line, keeping the raw
        // line addressable (selection offsets live in RAW bytes).
        let disp: Cow<str> = if line.contains('\t') {
            Cow::Owned(line.replace('\t', "    "))
        } else {
            Cow::Borrowed(line)
        };
        let runs = tokenize(&disp, &sp, &mut in_block); // always lex (block-comment state)
        if y + line_h > clip_top && y < clip_bottom {
            // Selection fill FIRST — the runs draw transparent-bk on top of it.
            if let Some((s, e)) = sel {
                paint_sel_line(
                    hdc, line, line_start, s, e, code_x, code_right, y, line_h, char_w, sel_bg,
                );
            }
            // One hit per drawn line: this is a mono grid, so hit-testing re-measures inside it
            // for a char-precise offset (`text_x` = code_x, past the line-number gutter).
            if let Some(k) = sink.as_deref_mut() {
                k.hits.push(SelHit {
                    rect: RECT {
                        left: code_x,
                        top: y,
                        right: code_right,
                        bottom: y + line_h,
                    },
                    start: k.base + line_start,
                    end: k.base + line_start + line.len(),
                    font: k.spec,
                    text_x: code_x,
                });
            }
            // line number, right-aligned in [x, x+gutter_w-pad]
            SetTextColor(hdc, COLORREF(gutter_fg));
            let mut num: Vec<u16> = line_no.to_string().encode_utf16().collect();
            let mut nr = RECT {
                left: x,
                top: y,
                right: x + gutter_w - gutter_pad,
                bottom: y + line_h,
            };
            DrawTextW(
                hdc,
                &mut num,
                &mut nr,
                DT_RIGHT | DT_SINGLELINE | DT_NOPREFIX,
            );
            // code runs, starting past the gutter
            let mut cx = code_x;
            for (tag, s) in runs {
                if s.is_empty() {
                    continue;
                }
                SetTextColor(hdc, COLORREF(colors.of(tag)));
                let w16: Vec<u16> = s.encode_utf16().collect();
                let clip = RECT {
                    left: cx,
                    top: y,
                    right: code_right,
                    bottom: y + line_h,
                };
                let _ = ExtTextOutW(
                    hdc,
                    cx,
                    y,
                    ETO_CLIPPED,
                    Some(&clip as *const RECT),
                    PCWSTR(w16.as_ptr()),
                    w16.len() as u32,
                    None,
                );
                let mut sz = SIZE::default();
                let _ = GetTextExtentPoint32W(hdc, &w16, &mut sz);
                cx += sz.cx;
                if cx > code_right {
                    break; // rest of the line is off the pane
                }
            }
        }
        y += line_h;
        line_start += raw.len() + 1; // + the '\n' this line was split on
    }
    SelectObject(hdc, old);
    y - y0
}

/// Fill the selection background for one line: the intersection of the document byte range
/// `[s, e)` with this line's content, plus a half-character stub when the selection continues
/// through the line break (so selected empty lines / trailing newlines stay visible). X
/// positions are measured on the DISPLAY text (tabs expanded), matching the run painter.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
unsafe fn paint_sel_line(
    hdc: HDC,
    line: &str,        // raw line content (no trailing \r / \n)
    line_start: usize, // offset of the line's first byte in the document
    s: usize,
    e: usize,
    code_x: i32,
    code_right: i32,
    y: i32,
    line_h: i32,
    char_w: i32,
    sel_bg: u32,
) {
    let ls = line_start;
    let le = line_start + line.len();
    if s > le || e <= ls {
        return; // selection doesn't touch this line
    }
    let a = s.max(ls) - ls; // line-local selected byte range
    let b = e.min(le) - ls;
    let through_break = e > le; // continues past this line's end → draw the newline stub
    if a == b && !through_break {
        return;
    }
    let x1 = code_x + disp_extent(hdc, line, a);
    let mut x2 = code_x + disp_extent(hdc, line, b);
    if through_break {
        x2 += (char_w / 2).max(3);
    }
    let (x1, x2) = (x1.min(code_right), x2.min(code_right));
    if x2 <= x1 {
        return; // fully past the pane's right edge
    }
    let r = RECT {
        left: x1,
        top: y,
        right: x2,
        bottom: y + line_h,
    };
    let brush = CreateSolidBrush(COLORREF(sel_bg));
    FillRect(hdc, &r, brush);
    let _ = DeleteObject(brush.into());
}

/// Beyond this many UTF-16 units a line is far past any real pane width — stop measuring.
const MEASURE_CAP: usize = 16_384;

/// Pixel width of the display prefix (tabs expanded) equivalent to the raw line's first
/// `raw_to` bytes, measured with the currently selected font.
pub(super) unsafe fn disp_extent(hdc: HDC, line: &str, raw_to: usize) -> i32 {
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
pub(super) unsafe fn hit_test(
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
pub(super) unsafe fn col_at(hdc: HDC, line: &str, dx: i32) -> usize {
    // Display text (tabs → 4 spaces) + a parallel map: display unit → raw byte END of its char.
    let mut w16: Vec<u16> = Vec::new();
    let mut raw_end: Vec<usize> = Vec::new();
    for (i, c) in line.char_indices() {
        if w16.len() >= MEASURE_CAP {
            break; // way past any real pane width
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
    if w16.is_empty() {
        return 0;
    }
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
        return 0;
    }
    // Walk raw chars via the map: each char covers display span [left, right) — snap to the
    // nearer edge of the char containing dx.
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
pub(super) fn word_at(text: &str, off: usize) -> (usize, usize) {
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

/// Name/shebang fallback used only when [`lang_from_ext`] came back [`Lang::Plain`] (an
/// extensionless or specially-named file); QuickLook has an equivalent detector tier behind its
/// own extension lookup. Tries the exact file name first (case-insensitive; `keywords::NAME_LANG`
/// covers build files with no extension at all, e.g. Makefile and Dockerfile, plus dotfiles whose
/// whole name IS what `Path::extension()` calls "no extension", like `.gitignore`), then a `#!`
/// shebang on `leading_text`'s first line. Every match lands on a `Lang` variant `lex` already
/// lexes; no new variant, no lexer/keyword-table change.
pub(super) fn lang_from_name_or_shebang(name: &str, leading_text: &str) -> Lang {
    let lower = name.to_ascii_lowercase();
    if let Some(entry) = keywords::NAME_LANG.iter().find(|e| e.0 == lower.as_str()) {
        return lang_from_ext(entry.1);
    }
    lang_from_shebang(leading_text)
}

/// Reads a `#!` interpreter line (first line only) and maps it to a `Lang`. Matches by prefix so
/// versioned interpreters (`python3`, `ruby2.7`) still hit; `env`'s own argument is unwrapped
/// first (`#!/usr/bin/env python3` names `env`, not `python3`, as the literal program). Perl has
/// no `Lang` variant to land on, so it (and anything else unrecognised) falls through to `Plain`.
fn lang_from_shebang(leading_text: &str) -> Lang {
    let first = leading_text.lines().next().unwrap_or("");
    let Some(rest) = first.strip_prefix("#!") else {
        return Lang::Plain;
    };
    let interp = rest
        .trim()
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    let mut words = interp.split_whitespace();
    let mut prog = words.next().unwrap_or("");
    if prog == "env" {
        prog = words.next().unwrap_or("");
    }
    let is_shell = prog == "sh"
        || prog.starts_with("bash")
        || prog.starts_with("zsh")
        || prog.starts_with("dash");
    if is_shell {
        Lang::Sh
    } else if prog.starts_with("python") {
        Lang::Py
    } else if prog.starts_with("node") {
        Lang::Js
    } else if prog.starts_with("ruby") {
        Lang::Ruby
    } else if prog.starts_with("php") {
        Lang::Php
    } else {
        Lang::Plain
    }
}

#[cfg(test)]
mod tests {
    use super::{col_at, disp_extent, lang_from_name_or_shebang, lang_from_shebang, word_at, Lang};

    /// Every raw char boundary must round-trip: measure its x with `disp_extent` (the paint
    /// side, `GetTextExtentPoint32W`), feed that x back through `col_at` (the hit-test side,
    /// `GetTextExtentExPointW`) and land on the same boundary — proving the two GDI measures
    /// agree and the tab/surrogate display-unit grouping is right.
    #[test]
    fn col_at_roundtrips_disp_extent() {
        use windows::core::PCWSTR;
        use windows::Win32::Graphics::Gdi::{
            CreateFontW, DeleteObject, GetDC, ReleaseDC, SelectObject, CLIP_DEFAULT_PRECIS,
            DEFAULT_CHARSET, DEFAULT_QUALITY, OUT_DEFAULT_PRECIS,
        };
        unsafe {
            let hdc = GetDC(None);
            assert!(!hdc.is_invalid());
            let face: Vec<u16> = "Consolas\0".encode_utf16().collect();
            let font = CreateFontW(
                -13,
                0,
                0,
                0,
                400,
                0,
                0,
                0,
                DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                DEFAULT_QUALITY,
                Default::default(),
                PCWSTR(face.as_ptr()),
            );
            let old = SelectObject(hdc, font.into());
            let line = "\tlet grüße = vec![1, 42];\t// done 🚀 end";
            assert_eq!(col_at(hdc, line, 0), 0);
            for (i, c) in line.char_indices() {
                let b = i + c.len_utf8();
                let x = disp_extent(hdc, line, b);
                assert_eq!(col_at(hdc, line, x), b, "boundary {b} (after {c:?})");
            }
            SelectObject(hdc, old);
            let _ = DeleteObject(font.into());
            ReleaseDC(None, hdc);
        }
    }

    #[test]
    fn word_at_selects_identifiers_and_singles() {
        let t = "fn räum_1() {\n\tlet x = 42;\n}";
        let f = t.find("räum_1").unwrap();
        assert_eq!(word_at(t, f), (f, f + "räum_1".len())); // start of word
        assert_eq!(word_at(t, f + 3), (f, f + "räum_1".len())); // mid-word (after the 2-byte 'ä')
        let paren = t.find('(').unwrap();
        assert_eq!(word_at(t, paren), (paren, paren + 1)); // punctuation = itself
        let nl = t.find('\n').unwrap();
        assert_eq!(word_at(t, nl), (nl, nl)); // line break = nothing
        assert_eq!(word_at(t, t.len()), (t.len(), t.len())); // end of doc
        let num = t.find("42").unwrap();
        assert_eq!(word_at(t, num + 1), (num, num + 2)); // digits group like a word
    }

    #[test]
    fn name_fallback_maps_known_files_case_insensitively() {
        assert!(matches!(
            lang_from_name_or_shebang("Makefile", ""),
            Lang::Sh
        ));
        assert!(matches!(
            lang_from_name_or_shebang("DOCKERFILE", ""),
            Lang::Sh
        ));
        assert!(matches!(
            lang_from_name_or_shebang("Jenkinsfile", ""),
            Lang::Java
        ));
        assert!(matches!(
            lang_from_name_or_shebang("Vagrantfile", ""),
            Lang::Ruby
        ));
        assert!(matches!(
            lang_from_name_or_shebang(".GitIgnore", ""),
            Lang::Toml
        ));
        assert!(matches!(lang_from_name_or_shebang("go.mod", ""), Lang::Go));
        assert!(matches!(
            lang_from_name_or_shebang("Cargo.lock", ""),
            Lang::Toml
        ));
        // an unlisted name with no shebang falls all the way through to Plain.
        assert!(matches!(
            lang_from_name_or_shebang("readme", "just text"),
            Lang::Plain
        ));
    }

    #[test]
    fn shebang_fallback_maps_known_interpreters() {
        assert!(matches!(
            lang_from_shebang("#!/bin/bash\necho hi"),
            Lang::Sh
        ));
        assert!(matches!(
            lang_from_shebang("#!/usr/bin/env python3\nprint(1)"),
            Lang::Py
        ));
        assert!(matches!(
            lang_from_shebang("#!/usr/bin/env node\nconsole.log(1)"),
            Lang::Js
        ));
        assert!(matches!(
            lang_from_shebang("#!/usr/bin/ruby\nputs 1"),
            Lang::Ruby
        ));
        assert!(matches!(
            lang_from_shebang("#!/usr/bin/php\n<?php"),
            Lang::Php
        ));
        // perl has no Lang variant to land on.
        assert!(matches!(
            lang_from_shebang("#!/usr/bin/perl\nprint 1;"),
            Lang::Plain
        ));
        // the first line isn't a shebang at all (one appears later, which must not count).
        assert!(matches!(
            lang_from_shebang("just some text\n#!/bin/bash"),
            Lang::Plain
        ));
    }
}
