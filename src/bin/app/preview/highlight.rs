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

/// Selection wiring for a [`paint_lines`] call that belongs to a bigger document (a Markdown
/// fenced code block): where its text starts in the selection document, and the collector its
/// drawn lines record their hit rects into. The standalone text pane passes `None` — it
/// hit-tests analytically via [`hit_test`] instead.
pub(super) struct LineSel<'a> {
    pub hits: &'a mut Vec<SelHit>,
    pub base: usize,
    pub spec: FontSpec,
}

/// Languages we specially lex. `Plain` = no colouring (falls back to today's uncoloured draw).
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Lang {
    Rust,
    Py,
    Js,
    Json,
    Yaml,
    Toml,
    C,
    Cs,
    Java,
    Go,
    Ruby,
    Php,
    Lua,
    Kotlin,
    Swift,
    Sh,
    Html,
    Css,
    Xml,
    Sql,
    Plain,
}

#[derive(Clone, Copy, PartialEq)]
enum Tag {
    Plain,
    Comment,
    Str,
    Num,
    Keyword,
}

/// The canonical fenced-code info string for a language — used when a copied Markdown code
/// block is written back out as ``` fences, so it still highlights wherever it's pasted.
/// `None` for `Plain` (emit a bare fence). Lossy on purpose: the parse maps many tags onto one
/// `Lang` (`powershell`/`bash`/`zsh` all become `Sh`), and `Lang` is all we keep.
pub(super) fn lang_tag(l: Lang) -> Option<&'static str> {
    Some(match l {
        Lang::Rust => "rust",
        Lang::Py => "python",
        Lang::Js => "js",
        Lang::Json => "json",
        Lang::Yaml => "yaml",
        Lang::Toml => "toml",
        Lang::C => "c",
        Lang::Cs => "csharp",
        Lang::Java => "java",
        Lang::Go => "go",
        Lang::Ruby => "ruby",
        Lang::Php => "php",
        Lang::Lua => "lua",
        Lang::Kotlin => "kotlin",
        Lang::Swift => "swift",
        Lang::Sh => "sh",
        Lang::Html => "html",
        Lang::Css => "css",
        Lang::Xml => "xml",
        Lang::Sql => "sql",
        Lang::Plain => return None,
    })
}

/// Map a file extension (no dot) to a language.
pub(super) fn lang_from_ext(ext: &str) -> Lang {
    match ext.to_ascii_lowercase().as_str() {
        "rs" => Lang::Rust,
        "py" | "pyw" | "pyi" => Lang::Py,
        "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" => Lang::Js,
        "json" | "jsonc" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "c" | "h" | "cpp" | "cxx" | "cc" | "hpp" | "hxx" => Lang::C,
        "cs" => Lang::Cs,
        "java" => Lang::Java,
        "go" => Lang::Go,
        "rb" | "rake" | "gemspec" => Lang::Ruby,
        "php" | "phtml" => Lang::Php,
        "lua" => Lang::Lua,
        "kt" | "kts" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "sh" | "bash" | "zsh" | "ps1" | "bat" | "cmd" => Lang::Sh,
        "html" | "htm" | "xhtml" => Lang::Html,
        "css" | "scss" | "less" => Lang::Css,
        // svg is XML — reachable via the caption's "view source" toggle on a rendered SVG.
        "xml" | "svg" => Lang::Xml,
        "sql" => Lang::Sql,
        // ini/cfg files share TOML's shape (# / ; comments, key=value, quoted strings).
        "ini" | "cfg" | "conf" | "properties" | "editorconfig" | "gitconfig" => Lang::Toml,
        _ => Lang::Plain,
    }
}

/// Map a markdown fenced-code info-string tag (e.g. ```` ```rust ````) to a language. Also accepts
/// a bare extension as the tag.
pub(super) fn lang_from_fence(tag: &str) -> Lang {
    match tag.to_ascii_lowercase().as_str() {
        "rust" | "rs" => Lang::Rust,
        "python" | "py" => Lang::Py,
        "js" | "javascript" | "ts" | "typescript" | "jsx" | "tsx" | "node" => Lang::Js,
        "json" | "jsonc" => Lang::Json,
        "yaml" | "yml" => Lang::Yaml,
        "toml" => Lang::Toml,
        "c" | "cpp" | "c++" | "h" | "hpp" => Lang::C,
        "cs" | "csharp" | "c#" => Lang::Cs,
        "java" => Lang::Java,
        "go" | "golang" => Lang::Go,
        "ruby" | "rb" => Lang::Ruby,
        "php" => Lang::Php,
        "lua" => Lang::Lua,
        "kotlin" | "kt" => Lang::Kotlin,
        "swift" => Lang::Swift,
        "sh" | "bash" | "shell" | "zsh" | "ps1" | "powershell" | "bat" | "console" => Lang::Sh,
        "html" | "htm" => Lang::Html,
        "css" | "scss" | "less" => Lang::Css,
        "xml" | "svg" => Lang::Xml,
        "sql" => Lang::Sql,
        other => lang_from_ext(other),
    }
}

/// Per-language lexer spec.
struct Spec {
    line_comment: &'static [&'static str],
    block: Option<(&'static str, &'static str)>,
    strings: &'static [u8], // quote chars
    keywords: &'static [&'static str],
}

/// The per-language tuple `spec()` builds from: (line comments, block comment, quotes, keywords).
type SpecParts = (
    &'static [&'static str],
    Option<(&'static str, &'static str)>,
    &'static [u8],
    &'static [&'static str],
);

fn spec(lang: Lang) -> Spec {
    let (lc, bl, st, kw): SpecParts = match lang {
        Lang::Rust => (&["//"], Some(("/*", "*/")), b"\"", RUST_KW),
        Lang::Py => (&["#"], None, b"\"'", PY_KW),
        Lang::Js => (&["//"], Some(("/*", "*/")), b"\"'`", JS_KW),
        Lang::Json => (&[], None, b"\"", &["true", "false", "null"]),
        Lang::Yaml => (&["#"], None, b"\"'", YAML_KW),
        Lang::Toml => (&["#"], None, b"\"'", &["true", "false"]),
        Lang::C => (&["//"], Some(("/*", "*/")), b"\"'", C_KW),
        Lang::Cs => (&["//"], Some(("/*", "*/")), b"\"'", CS_KW),
        Lang::Java => (&["//"], Some(("/*", "*/")), b"\"", JAVA_KW),
        Lang::Go => (&["//"], Some(("/*", "*/")), b"\"`", GO_KW),
        Lang::Ruby => (&["#"], None, b"\"'", RUBY_KW),
        Lang::Php => (&["//", "#"], Some(("/*", "*/")), b"\"'", PHP_KW),
        Lang::Lua => (&["--"], None, b"\"'", LUA_KW),
        Lang::Kotlin => (&["//"], Some(("/*", "*/")), b"\"", KOTLIN_KW),
        Lang::Swift => (&["//"], Some(("/*", "*/")), b"\"", SWIFT_KW),
        Lang::Sh => (&["#"], None, b"\"'", SH_KW),
        Lang::Html | Lang::Xml => (&[], Some(("<!--", "-->")), b"\"'", &[]),
        Lang::Css => (&[], Some(("/*", "*/")), b"\"'", &[]),
        Lang::Sql => (&["--"], Some(("/*", "*/")), b"'", SQL_KW),
        Lang::Plain => (&[], None, b"", &[]),
    };
    Spec { line_comment: lc, block: bl, strings: st, keywords: kw }
}

/// UTF-8 byte length of the char starting with lead byte `b`.
fn utf8_len(b: u8) -> usize {
    if b >= 0xF0 {
        4
    } else if b >= 0xE0 {
        3
    } else if b >= 0xC0 {
        2
    } else {
        1
    }
}

fn find_from(hay: &str, from: usize, needle: &str) -> Option<usize> {
    hay.get(from..).and_then(|s| s.find(needle)).map(|p| p + from)
}

/// Tokenize one line into `(tag, slice)` runs. `in_block` carries block-comment state across
/// lines. Non-keyword identifiers + punctuation stay in `Plain` runs (few runs per line).
fn tokenize<'a>(line: &'a str, sp: &Spec, in_block: &mut bool) -> Vec<(Tag, &'a str)> {
    let b = line.as_bytes();
    let n = b.len();
    let mut out: Vec<(Tag, &'a str)> = Vec::new();
    let mut i = 0usize;
    let mut seg = 0usize; // start of the pending Plain segment

    macro_rules! flush {
        ($upto:expr) => {
            if $upto > seg {
                out.push((Tag::Plain, &line[seg..$upto]));
            }
        };
    }

    while i < n {
        // carried-over block comment
        if *in_block {
            if let Some((_, close)) = sp.block {
                if let Some(pos) = find_from(line, i, close) {
                    let end = pos + close.len();
                    out.push((Tag::Comment, &line[i..end]));
                    i = end;
                    seg = i;
                    *in_block = false;
                    continue;
                }
            }
            out.push((Tag::Comment, &line[i..]));
            seg = n;
            break;
        }
        // line comment -> rest of line
        if sp.line_comment.iter().any(|c| line[i..].starts_with(*c)) {
            flush!(i);
            out.push((Tag::Comment, &line[i..]));
            seg = n;
            break;
        }
        // block comment open
        if let Some((open, close)) = sp.block {
            if line[i..].starts_with(open) {
                flush!(i);
                if let Some(pos) = find_from(line, i + open.len(), close) {
                    let end = pos + close.len();
                    out.push((Tag::Comment, &line[i..end]));
                    i = end;
                    seg = i;
                } else {
                    out.push((Tag::Comment, &line[i..]));
                    i = n;
                    seg = n;
                    *in_block = true;
                }
                continue;
            }
        }
        let ch = b[i];
        // string literal
        if sp.strings.contains(&ch) {
            flush!(i);
            let start = i;
            i += 1;
            while i < n {
                if b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == ch {
                    i += 1;
                    break;
                }
                i += 1;
            }
            let end = i.min(n);
            // A string immediately followed by `:` is an object KEY / property — colour it like a
            // keyword (matches QuickLook's blue property names) instead of an orange string value.
            let is_key = line.get(end..).is_some_and(|r| r.trim_start().starts_with(':'));
            out.push((if is_key { Tag::Keyword } else { Tag::Str }, &line[start..end]));
            i = end;
            seg = i;
            continue;
        }
        // number literal
        if ch.is_ascii_digit() {
            flush!(i);
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            out.push((Tag::Num, &line[start..i]));
            seg = i;
            continue;
        }
        // identifier -> keyword lookup (non-keywords stay in the plain segment)
        if ch.is_ascii_alphabetic() || ch == b'_' {
            let start = i;
            while i < n && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                i += 1;
            }
            let word = &line[start..i];
            if sp.keywords.contains(&word) {
                flush!(start);
                out.push((Tag::Keyword, word));
                seg = i;
            }
            continue;
        }
        // plain char (advance by full UTF-8 char so slices never split a codepoint)
        i += if ch < 0x80 { 1 } else { utf8_len(ch) };
    }
    flush!(n);
    out
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
                paint_sel_line(hdc, line, line_start, s, e, code_x, code_right, y, line_h, char_w, sel_bg);
            }
            // One hit per drawn line: this is a mono grid, so hit-testing re-measures inside it
            // for a char-precise offset (`text_x` = code_x, past the line-number gutter).
            if let Some(k) = sink.as_deref_mut() {
                k.hits.push(SelHit {
                    rect: RECT { left: code_x, top: y, right: code_right, bottom: y + line_h },
                    start: k.base + line_start,
                    end: k.base + line_start + line.len(),
                    font: k.spec,
                    text_x: code_x,
                });
            }
            // line number, right-aligned in [x, x+gutter_w-pad]
            SetTextColor(hdc, COLORREF(gutter_fg));
            let mut num: Vec<u16> = line_no.to_string().encode_utf16().collect();
            let mut nr = RECT { left: x, top: y, right: x + gutter_w - gutter_pad, bottom: y + line_h };
            DrawTextW(hdc, &mut num, &mut nr, DT_RIGHT | DT_SINGLELINE | DT_NOPREFIX);
            // code runs, starting past the gutter
            let mut cx = code_x;
            for (tag, s) in runs {
                if s.is_empty() {
                    continue;
                }
                SetTextColor(hdc, COLORREF(colors.of(tag)));
                let w16: Vec<u16> = s.encode_utf16().collect();
                let clip = RECT { left: cx, top: y, right: code_right, bottom: y + line_h };
                let _ = ExtTextOutW(hdc, cx, y, ETO_CLIPPED, Some(&clip as *const RECT), PCWSTR(w16.as_ptr()), w16.len() as u32, None);
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
    let r = RECT { left: x1, top: y, right: x2, bottom: y + line_h };
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
    let line_end = starts.get(li + 1).map(|s| s.saturating_sub(1)).unwrap_or(text.len());
    let line = text.get(line_start..line_end).unwrap_or("");
    let line = line.strip_suffix('\r').unwrap_or(line);
    let off = if x <= code_x { line_start } else { line_start + col_at(hdc, line, x - code_x) };
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
    if w16.len() >= MEASURE_CAP { raw_end.last().copied().unwrap_or(line.len()) } else { line.len() }
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

// ---- keyword tables (intentionally small / incomplete) -----------------------------------

const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern",
    "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move", "mut", "pub",
    "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true", "type", "unsafe",
    "use", "where", "while", "bool", "char", "str", "String", "u8", "u16", "u32", "u64", "u128",
    "usize", "i8", "i16", "i32", "i64", "i128", "isize", "f32", "f64", "Vec", "Option", "Some",
    "None", "Result", "Ok", "Err", "Box", "Rc", "Arc",
];
const PY_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif",
    "else", "except", "False", "finally", "for", "from", "global", "if", "import", "in", "is",
    "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True", "try", "while",
    "with", "yield", "self", "print", "len", "range", "int", "str", "float", "bool", "list", "dict",
    "set", "tuple",
];
const JS_KW: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default",
    "delete", "do", "else", "enum", "export", "extends", "false", "finally", "for", "function", "if",
    "implements", "import", "in", "instanceof", "interface", "let", "new", "null", "of", "return",
    "static", "super", "switch", "this", "throw", "true", "try", "type", "typeof", "undefined",
    "var", "void", "while", "yield", "string", "number", "boolean", "any", "readonly",
];
const YAML_KW: &[&str] = &["true", "false", "null", "yes", "no", "on", "off"];
const C_KW: &[&str] = &[
    "auto", "bool", "break", "case", "char", "class", "const", "continue", "default", "do", "double",
    "else", "enum", "extern", "false", "float", "for", "goto", "if", "inline", "int", "long",
    "namespace", "new", "nullptr", "private", "protected", "public", "register", "return", "short",
    "signed", "sizeof", "static", "struct", "switch", "template", "this", "true", "typedef", "union",
    "unsigned", "using", "virtual", "void", "volatile", "while",
];
const CS_KW: &[&str] = &[
    "abstract", "as", "async", "await", "base", "bool", "break", "byte", "case", "catch", "char",
    "class", "const", "continue", "decimal", "default", "delegate", "do", "double", "else", "enum",
    "event", "false", "finally", "float", "for", "foreach", "get", "if", "in", "int", "interface",
    "internal", "is", "long", "namespace", "new", "null", "object", "out", "override", "params",
    "private", "protected", "public", "readonly", "ref", "return", "sealed", "set", "static",
    "string", "struct", "switch", "this", "throw", "true", "try", "typeof", "using", "var",
    "virtual", "void", "while", "var",
];
const SH_KW: &[&str] = &[
    "if", "then", "else", "elif", "fi", "for", "in", "do", "done", "while", "until", "case", "esac",
    "function", "return", "exit", "echo", "export", "local", "set", "source", "sudo", "cd", "param",
    "function", "foreach", "begin", "process", "end", "true", "false",
];
const SQL_KW: &[&str] = &[
    "SELECT", "select", "FROM", "from", "WHERE", "where", "INSERT", "insert", "UPDATE", "update",
    "DELETE", "delete", "INTO", "into", "VALUES", "values", "SET", "set", "JOIN", "join", "LEFT",
    "left", "RIGHT", "right", "INNER", "inner", "OUTER", "outer", "ON", "on", "AND", "and", "OR",
    "or", "NOT", "not", "NULL", "null", "AS", "as", "ORDER", "order", "BY", "by", "GROUP", "group",
    "HAVING", "having", "LIMIT", "limit", "CREATE", "create", "TABLE", "table", "PRIMARY", "primary",
    "KEY", "key", "DROP", "drop", "ALTER", "alter", "INDEX", "index", "DISTINCT", "distinct",
];

// The seven tables below are distilled from Monaco Editor's Monarch grammars
// (monaco-editor/src/basic-languages/<lang>, MIT) — the `keywords` + `typeKeywords`
// arrays, trimmed to the ~most-seen subset. Same "look colourized, not a grammar"
// stance as the tables above.

const JAVA_KW: &[&str] = &[
    "abstract", "assert", "boolean", "break", "byte", "case", "catch", "char", "class", "const",
    "continue", "default", "do", "double", "else", "enum", "extends", "false", "final", "finally",
    "float", "for", "goto", "if", "implements", "import", "instanceof", "int", "interface", "long",
    "native", "new", "null", "package", "permits", "private", "protected", "public", "record",
    "return", "sealed", "short", "static", "strictfp", "super", "switch", "synchronized", "this",
    "throw", "throws", "transient", "true", "try", "var", "void", "volatile", "while", "yield",
    "String", "Integer", "Boolean", "Object", "List", "Map",
];

const GO_KW: &[&str] = &[
    "any", "append", "bool", "break", "byte", "cap", "case", "chan", "close", "complex64",
    "complex128", "const", "continue", "copy", "default", "defer", "delete", "else", "error",
    "fallthrough", "false", "float32", "float64", "for", "func", "go", "goto", "if", "import",
    "int", "int8", "int16", "int32", "int64", "interface", "iota", "len", "make", "map", "new",
    "nil", "package", "panic", "range", "recover", "return", "rune", "select", "string", "struct",
    "switch", "true", "type", "uint", "uint8", "uint16", "uint32", "uint64", "uintptr", "var",
];

const RUBY_KW: &[&str] = &[
    "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else", "elsif",
    "end", "ensure", "extend", "false", "for", "if", "in", "include", "loop", "module", "next",
    "nil", "not", "or", "private", "protected", "public", "puts", "raise", "redo", "require",
    "require_relative", "rescue", "retry", "return", "self", "super", "then", "true", "undef",
    "unless", "until", "when", "while", "yield", "attr_accessor", "attr_reader", "attr_writer",
    "lambda", "proc", "new",
];

const PHP_KW: &[&str] = &[
    "abstract", "and", "array", "as", "break", "callable", "case", "catch", "class", "clone",
    "const", "continue", "declare", "default", "die", "do", "echo", "else", "elseif", "empty",
    "enum", "exit", "extends", "false", "final", "finally", "fn", "for", "foreach", "function",
    "global", "if",
    "implements", "include", "include_once", "instanceof", "interface", "isset", "list", "match",
    "namespace", "new", "null", "or", "print", "private", "protected", "public", "readonly",
    "require", "require_once", "return", "static", "switch", "throw", "trait", "true", "try",
    "unset", "use", "var", "while", "xor", "yield", "int", "float", "string", "bool", "void",
    "mixed", "never", "object", "self", "parent", "this",
];

const LUA_KW: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in",
    "local", "nil", "not", "or", "repeat", "return", "then", "true", "until", "while", "print",
    "pairs", "ipairs", "type", "tostring", "tonumber", "require", "pcall", "error", "self",
];

const KOTLIN_KW: &[&str] = &[
    "abstract", "actual", "annotation", "as", "break", "by", "catch", "class", "companion",
    "const", "constructor", "continue", "crossinline", "data", "do", "else", "enum", "expect",
    "external", "false", "final", "finally", "for", "fun", "get", "if", "import", "in", "infix",
    "init", "inline", "inner", "interface", "internal", "is", "it", "lateinit", "noinline",
    "null", "object", "open", "operator", "out", "override", "package", "private", "protected",
    "public", "reified", "return", "sealed", "set", "super", "suspend", "tailrec", "this",
    "throw", "true", "try", "typealias", "val", "var", "vararg", "when", "where", "while",
    "Int", "Long", "Float", "Double", "Boolean", "String", "Unit", "Any", "List", "Map",
];

const SWIFT_KW: &[&str] = &[
    "actor", "as", "associatedtype", "async", "await", "break", "case", "catch", "class",
    "continue", "convenience", "default", "defer", "deinit", "didSet", "do", "dynamic", "else",
    "enum", "extension", "fallthrough", "false", "fileprivate", "final", "for", "func", "get",
    "guard", "if", "import", "in", "indirect", "infix", "init", "inout", "internal", "is",
    "lazy", "let", "mutating", "nil", "nonmutating", "open", "operator", "optional", "override",
    "postfix", "precedencegroup", "prefix", "private", "protocol", "public", "repeat", "required",
    "rethrows", "return", "self", "Self", "set", "some", "static", "struct", "subscript", "super",
    "switch", "throw", "throws", "true", "try", "typealias", "unowned", "var", "weak", "where",
    "while", "willSet", "Int", "Double", "Float", "Bool", "String", "Array", "Dictionary",
    "Optional", "Void",
];

#[cfg(test)]
mod tests {
    use super::{col_at, disp_extent, word_at};

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
                -13, 0, 0, 0, 400, 0, 0, 0,
                DEFAULT_CHARSET, OUT_DEFAULT_PRECIS, CLIP_DEFAULT_PRECIS, DEFAULT_QUALITY,
                Default::default(), PCWSTR(face.as_ptr()),
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
}
