//! Lightweight, zero-dependency syntax highlighting for the Quick preview viewer's code display
//! (code files + markdown fenced code blocks). A single-pass per-line lexer per language — line/
//! block comments (with cross-line state), string literals, numbers, and a per-language keyword
//! set — NOT a real parser. Deliberately small (no syntect / onig / regex). Colours come from the
//! theme (`dark.rs`). Keyword tables are intentionally incomplete: enough to "look colourized",
//! not to be a grammar.

use std::borrow::Cow;
use std::cell::RefCell;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, DrawTextW, ExtTextOutW, FillRect, GetTextExtentExPointW,
    GetTextExtentPoint32W, GetTextMetricsW, SelectObject, SetTextColor, DT_NOPREFIX, DT_RIGHT,
    DT_SINGLELINE, ETO_CLIPPED, HDC, HFONT, TEXTMETRICW,
};

use super::selection::{FontSpec, SelHit};

mod hit;
mod keywords;
mod lex;
pub(super) use hit::{col_at, disp_extent, hit_test, word_at};

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

/// Per-line block-comment (`in_block`) state cached across repeated [`paint_lines`] calls for
/// the SAME text buffer, so a scroll/resize/blink repaint — `paint_lines` runs on every
/// `WM_PAINT`, not only when the document actually changes — doesn't have to re-lex the WHOLE
/// file (up to 5 MB, entirely off-screen) just to recover this one running bool before it can
/// draw the handful of lines actually visible.
struct BlockCache {
    /// Cheap "is this still the same document" fingerprint: the text buffer's address, length,
    /// and language. The viewer keeps the loaded document in one stable buffer across repaints
    /// and only replaces it (a fresh allocation → a new pointer) on an actual reload, so this is
    /// good enough without hashing megabytes of text on every paint. A false MISS just costs one
    /// extra full-file lex — today's behaviour, never wrong. A coincidental false HIT (freed
    /// memory reused at the exact same address+length+language for a genuinely different buffer)
    /// could only ever mis-seed `in_block` for one paint of a syntax-highlighted VIEW — a
    /// cosmetic miscolour, not a correctness or safety issue; this window renders, it never lets
    /// the user edit, so there is no "flush stale colours before a write" concern either.
    /// Buffer address, length, language, and the load generation the buffer belongs to: a
    /// later file of the same length can land at the same address once the old `String`
    /// is freed, and without the generation a hit would paint the previous file's tokens.
    key: (usize, usize, u8, u64),
    /// `in_block` at the START of each line (index = 0-based line number), one entry per line.
    before: Vec<bool>,
    /// Tokenized runs for each line (same indexing as `before`), captured on the same MISS pass
    /// that builds it. A later repaint of a line already covered by this table — a scroll, a
    /// resize, or a cursor-blink repaint of the same viewport — draws straight from here instead
    /// of re-running the tokenizer (and the tab-expansion allocation ahead of it) over content
    /// that has not changed.
    runs: Vec<Vec<(Tag, String)>>,
}

thread_local! {
    // Painting only ever happens on the viewer window's own UI thread (WM_PAINT is delivered
    // there), so a thread-local needs no locking — unlike a process-wide cache, it also can't
    // leak state between multiple viewer windows on different threads.
    static BLOCK_CACHE: RefCell<Option<BlockCache>> = const { RefCell::new(None) };
}

/// Immutable per-call state threaded through the per-line helpers below — geometry, theme
/// colours, the lexer spec, and the cache-lookup identity that every line consults but none of
/// them mutate. Split out of [`paint_lines`]'s locals purely so those helpers don't each need a
/// dozen positional parameters.
struct PaintCtx<'a> {
    hdc: HDC,
    colors: &'a Colors,
    sp: &'a Spec,
    x: i32,
    code_x: i32,
    code_right: i32,
    gutter_w: i32,
    gutter_pad: i32,
    gutter_fg: u32,
    line_h: i32,
    char_w: i32,
    sel_bg: u32,
    sel: Option<(usize, usize)>,
    key: (usize, usize, u8, u64),
}

/// Which line the current loop iteration in [`paint_lines`] is on, and where it draws.
struct LineCtx {
    /// 0-based line index — indexes `BlockCache::before`/`runs`.
    line_no0: usize,
    /// 1-based line number — what the gutter shows.
    line_no: usize,
    /// Raw byte offset of this line's first char in the source text.
    line_start: usize,
    y: i32,
    visible: bool,
}

/// The table a cache MISS builds as it lexes every line, banked into [`BLOCK_CACHE`] after the
/// loop; `in_block` is also the running block-comment state carried line-to-line during lexing.
struct LexState {
    in_block: bool,
    fresh_before: Vec<bool>,
    fresh_runs: Vec<Vec<(Tag, String)>>,
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
    let char_w = tm.tmAveCharWidth.max(1);
    let gutter_fg = crate::dark::HEADER_TEXT().0;
    let sel_bg = crate::dark::SEL_BG().0;

    // Cache lookup: same buffer (address+length), same language → reuse the per-line `in_block`
    // table AND the already-tokenized runs instead of re-lexing lines this call has already
    // drawn before (a scroll, a resize, or a cursor-blink repaint of the same viewport). Cloning
    // the cached `Vec<bool>` (one bool per line) is cheap even for a huge file; `runs` is only
    // ever cloned per line, on demand, for the handful of lines actually drawn below — never the
    // whole document.
    let key = (
        text.as_ptr() as usize,
        text.len(),
        lang as u8,
        super::content::live_generation(),
    );
    let cached_before: Option<Vec<bool>> = BLOCK_CACHE.with(|c| {
        c.borrow()
            .as_ref()
            .and_then(|bc| (bc.key == key).then(|| bc.before.clone()))
    });

    // Left gutter: right-aligned 1-based line numbers in a muted colour, like QuickLook / an
    // editor. Its width is sized to the digit count of the last line, so the code column shifts
    // right by it. The line count comes straight from the cache on a HIT (`before` has one entry
    // per line) instead of re-scanning the whole document for a newline count on every repaint.
    let total_lines = cached_before
        .as_ref()
        .map(|b| b.len())
        .unwrap_or_else(|| text.split('\n').count());
    let (gutter_w, gutter_pad, code_x, code_right) = gutter_metrics(total_lines, x, width, char_w);

    let ctx = PaintCtx {
        hdc,
        colors: &colors,
        sp: &sp,
        x,
        code_x,
        code_right,
        gutter_w,
        gutter_pad,
        gutter_fg,
        line_h,
        char_w,
        sel_bg,
        sel,
        key,
    };
    let has_cache = cached_before.is_some();
    let cached_before_slice = cached_before.as_deref();

    // A MISS lexes every line unconditionally (identical to the old always-lex behaviour) and
    // records the `in_block` state entering each one, plus its tokenized runs, so the NEXT call
    // for this same buffer (the common case: a scroll or resize repaint of the document already
    // on screen) can hit the cache instead.
    let mut state = LexState {
        in_block: false,
        fresh_before: Vec::new(),
        fresh_runs: Vec::new(),
    };

    let mut y = y0;
    let mut line_start = 0usize; // raw byte offset of this line's first char in `text`
    for (line_no0, raw) in text.split('\n').enumerate() {
        let line = LineCtx {
            line_no0,
            line_no: line_no0 + 1,
            line_start,
            y,
            visible: y + line_h > clip_top && y < clip_bottom,
        };

        // A line the MISS pass already tokenized draws straight from that cache — no re-lex, no
        // tab-expansion allocation, nothing. A line the cache doesn't (yet) cover — which cannot
        // happen once a MISS has run for this buffer, but costs nothing to guard — falls through
        // to the real lexer instead.
        let drawn_from_cache =
            try_draw_cached_line(&ctx, sink.as_deref_mut(), raw, &line, has_cache);
        if !drawn_from_cache {
            lex_and_draw_line(
                &ctx,
                sink.as_deref_mut(),
                raw,
                &line,
                cached_before_slice,
                &mut state,
            );
        }
        y += line_h;
        line_start += raw.len() + 1; // + the '\n' this line was split on
    }
    SelectObject(hdc, old);
    if cached_before.is_none() {
        // Built a complete table this call (a MISS lexes every line) — cache it for the next
        // repaint of this same buffer.
        BLOCK_CACHE.with(|c| {
            *c.borrow_mut() = Some(BlockCache {
                key,
                before: state.fresh_before,
                runs: state.fresh_runs,
            });
        });
    }
    y - y0
}

/// Draw one line's already-tokenized `runs`, taking the per-line draw geometry from `ctx` and
/// `line` — the cached-run fast path and the freshly-lexed path in [`paint_lines`] differ only in
/// where `runs` came from.
unsafe fn draw_line_runs(
    ctx: &PaintCtx,
    sink: Option<&mut LineSel>,
    text: &str,
    line: &LineCtx,
    runs: &[(Tag, &str)],
) {
    draw_visible_line(
        ctx.hdc,
        sink,
        text,
        line.line_start,
        ctx.sel,
        ctx.code_x,
        ctx.code_right,
        line.y,
        ctx.line_h,
        ctx.char_w,
        ctx.sel_bg,
        line.line_no,
        ctx.x,
        ctx.gutter_w,
        ctx.gutter_pad,
        ctx.gutter_fg,
        runs,
        ctx.colors,
    );
}

/// Draw line `line.line_no0` purely from the tokenized-run cache, when this call is a cache HIT
/// and the line is visible. Returns whether it drew — `false` means the caller must fall through
/// to the real lexer (see [`lex_and_draw_line`]).
unsafe fn try_draw_cached_line(
    ctx: &PaintCtx,
    sink: Option<&mut LineSel>,
    raw: &str,
    line: &LineCtx,
    has_cache: bool,
) -> bool {
    if !(line.visible && has_cache) {
        return false;
    }
    let cached_run: Option<Vec<(Tag, String)>> = BLOCK_CACHE.with(|c| {
        c.borrow().as_ref().and_then(|bc| {
            if bc.key == ctx.key {
                bc.runs.get(line.line_no0).cloned()
            } else {
                None
            }
        })
    });
    let Some(owned) = cached_run else {
        return false;
    };
    let text = raw.strip_suffix('\r').unwrap_or(raw);
    let runs: Vec<(Tag, &str)> = owned.iter().map(|(t, s)| (*t, s.as_str())).collect();
    draw_line_runs(ctx, sink, text, line, &runs);
    true
}

/// Seed `state.in_block` for line `line_no0` before lexing it: on a cache HIT, read the running
/// state straight from the per-line table (seeding across lines this call never visited); on a
/// MISS, record the state BEFORE this line into `state.fresh_before` so the complete table can be
/// banked after the loop.
fn seed_in_block(cached_before: Option<&[bool]>, line_no0: usize, state: &mut LexState) {
    match cached_before {
        // Reaching the real lexer on a HIT only happens for a line the cache didn't cover (the
        // visibility guard in the caller) — seed straight from the per-line table rather than
        // tracking a running state across lines this call never visited.
        Some(before) => state.in_block = before.get(line_no0).copied().unwrap_or(false),
        None => state.fresh_before.push(state.in_block), // record state BEFORE this line
    }
}

/// Real-lex line `line.line_no0` with the tokenizer when needed — always on a cache MISS (to draw
/// correctly AND build the cache for next time), or when this line is visible on a HIT (only
/// visible lines need re-lexing there; every other line's `in_block` contribution already sits in
/// `cached_before`) — and draw it when visible. Matches the function's old always-lex-every-line
/// behaviour exactly.
unsafe fn lex_and_draw_line(
    ctx: &PaintCtx,
    sink: Option<&mut LineSel>,
    raw: &str,
    line: &LineCtx,
    cached_before: Option<&[bool]>,
    state: &mut LexState,
) {
    if cached_before.is_some() && !line.visible {
        return;
    }
    let text = raw.strip_suffix('\r').unwrap_or(raw);
    // `ExtTextOutW` doesn't expand tabs (unlike the plain path's DT_EXPANDTABS), so tab-indented
    // code (Go, Makefiles) would collapse its indentation — expand per line, keeping the raw line
    // addressable (selection offsets live in RAW bytes).
    let disp: Cow<str> = if text.contains('\t') {
        Cow::Owned(text.replace('\t', "    "))
    } else {
        Cow::Borrowed(text)
    };
    seed_in_block(cached_before, line.line_no0, state);
    let runs = tokenize(&disp, ctx.sp, &mut state.in_block);
    if cached_before.is_none() {
        // Building the complete table this call — bank the owned runs alongside it.
        state
            .fresh_runs
            .push(runs.iter().map(|&(t, s)| (t, s.to_owned())).collect());
    }
    if line.visible {
        draw_line_runs(ctx, sink, text, line, &runs);
    }
}

/// Selection fill, hit-test record, gutter number and token runs for one drawn line — shared by
/// the cached-run fast path and the freshly-lexed path in [`paint_lines`], which differ only in
/// where `runs` came from.
#[allow(clippy::too_many_arguments)]
unsafe fn draw_visible_line(
    hdc: HDC,
    sink: Option<&mut LineSel>,
    line: &str,
    line_start: usize,
    sel: Option<(usize, usize)>,
    code_x: i32,
    code_right: i32,
    y: i32,
    line_h: i32,
    char_w: i32,
    sel_bg: u32,
    line_no: usize,
    x: i32,
    gutter_w: i32,
    gutter_pad: i32,
    gutter_fg: u32,
    runs: &[(Tag, &str)],
    colors: &Colors,
) {
    // Selection fill FIRST — the runs draw transparent-bk on top of it.
    if let Some((s, e)) = sel {
        paint_sel_line(
            hdc, line, line_start, s, e, code_x, code_right, y, line_h, char_w, sel_bg,
        );
    }
    // One hit per drawn line: this is a mono grid, so hit-testing re-measures inside it for a
    // char-precise offset (`text_x` = code_x, past the line-number gutter).
    record_line_hit(sink, line_start, line.len(), code_x, code_right, y, line_h);
    draw_gutter_line_number(hdc, line_no, x, gutter_w, gutter_pad, y, line_h, gutter_fg);
    draw_code_runs(hdc, runs, code_x, code_right, y, line_h, colors);
}

/// Left-gutter geometry: right-aligned 1-based line numbers, sized to the digit count of the
/// LAST line so the code column shifts right by exactly its width. Returns
/// `(gutter_w, gutter_pad, code_x, code_right)`.
fn gutter_metrics(total_lines: usize, x: i32, width: i32, char_w: i32) -> (i32, i32, i32, i32) {
    let digits = total_lines.max(1).to_string().len() as i32;
    let gutter_pad = char_w; // gap between the numbers and the code
    let gutter_w = digits * char_w + gutter_pad * 2;
    let code_x = x + gutter_w;
    let code_right = x + width; // the code column ends at the same right edge as before
    (gutter_w, gutter_pad, code_x, code_right)
}

/// Record one drawn line's hit-test rect into the caller's [`LineSel`] collector, if any (the
/// standalone text pane passes `None` and hit-tests analytically instead).
fn record_line_hit(
    sink: Option<&mut LineSel>,
    line_start: usize,
    line_len: usize,
    code_x: i32,
    code_right: i32,
    y: i32,
    line_h: i32,
) {
    if let Some(k) = sink {
        k.hits.push(SelHit {
            rect: RECT {
                left: code_x,
                top: y,
                right: code_right,
                bottom: y + line_h,
            },
            start: k.base + line_start,
            end: k.base + line_start + line_len,
            font: k.spec,
            text_x: code_x,
        });
    }
}

/// Draw one line's right-aligned 1-based number in the gutter, `[x, x+gutter_w-pad]`.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
unsafe fn draw_gutter_line_number(
    hdc: HDC,
    line_no: usize,
    x: i32,
    gutter_w: i32,
    gutter_pad: i32,
    y: i32,
    line_h: i32,
    gutter_fg: u32,
) {
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
}

/// Draw one line's tagged runs, starting past the gutter, clipped to `code_right`.
unsafe fn draw_code_runs(
    hdc: HDC,
    runs: &[(Tag, &str)],
    code_x: i32,
    code_right: i32,
    y: i32,
    line_h: i32,
    colors: &Colors,
) {
    let mut cx = code_x;
    for (tag, s) in runs {
        if s.is_empty() {
            continue;
        }
        SetTextColor(hdc, COLORREF(colors.of(*tag)));
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
/// versioned interpreters (`python3`, `ruby2.7`, `perl5`) still hit; `env`'s own argument is
/// unwrapped first (`#!/usr/bin/env python3` names `env`, not `python3`, as the literal program).
/// Anything unrecognised falls through to `Plain`.
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
    match_interpreter_lang(prog)
}

/// Classifies an interpreter binary name into a syntax-highlighting language.
fn match_interpreter_lang(prog: &str) -> Lang {
    match prog {
        "sh" => Lang::Sh,
        p if is_shell_interpreter(p) => Lang::Sh,
        p if p.starts_with("python") => Lang::Py,
        p if p.starts_with("node") => Lang::Js,
        p if p.starts_with("ruby") => Lang::Ruby,
        p if p.starts_with("php") => Lang::Php,
        p if p.starts_with("perl") => Lang::Perl,
        _ => Lang::Plain,
    }
}

/// True for the POSIX-shell interpreter family (`bash`/`zsh`/`dash`) that maps to [`Lang::Sh`].
fn is_shell_interpreter(prog: &str) -> bool {
    prog.starts_with("bash") || prog.starts_with("zsh") || prog.starts_with("dash")
}

#[cfg(test)]
mod tests;
