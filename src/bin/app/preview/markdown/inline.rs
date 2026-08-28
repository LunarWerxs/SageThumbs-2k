//! Inline run layout: the word-wrapper, the font cache behind it, and the primitive
//! draws every block type shares.
//!
//! This is the hot loop of the whole renderer - it runs per block, per paint - so the
//! fonts are cached per style rather than created per run, and the tokenizer walks the
//! text once.

use super::*;

/// The five font variants a block draws with, created once and freed together.
pub(super) struct Fonts {
    pub(super) reg: HFONT,
    pub(super) bold: HFONT,
    pub(super) ital: HFONT,
    pub(super) bi: HFONT,
    pub(super) mono: HFONT,
    pub(super) px: i32,
    pub(super) base_bold: bool,
    pub(super) base_italic: bool,
}

impl Fonts {
    pub(super) unsafe fn new(hwnd: HWND, px: i32, base_bold: bool, base_italic: bool) -> Fonts {
        Fonts {
            reg: font(hwnd, px, base_bold, base_italic, false),
            bold: font(hwnd, px, true, base_italic, false),
            ital: font(hwnd, px, base_bold, true, false),
            bi: font(hwnd, px, true, true, false),
            mono: font(hwnd, px - 1, false, false, true),
            px,
            base_bold,
            base_italic,
        }
    }
    pub(super) fn pick(&self, r: &Run) -> HFONT {
        if r.code {
            return self.mono;
        }
        let b = self.base_bold || r.bold;
        let i = self.base_italic || r.italic;
        match (b, i) {
            (true, true) => self.bi,
            (true, false) => self.bold,
            (false, true) => self.ital,
            (false, false) => self.reg,
        }
    }
    /// The spec of the font [`Fonts::pick`] would return — recorded per drawn token so
    /// hit-testing can re-create it after these handles are freed. MUST mirror `pick`/`new`.
    pub(super) fn spec(&self, r: &Run) -> FontSpec {
        if r.code {
            return FontSpec {
                px: self.px - 1,
                bold: false,
                italic: false,
                mono: true,
            };
        }
        FontSpec {
            px: self.px,
            bold: self.base_bold || r.bold,
            italic: self.base_italic || r.italic,
            mono: false,
        }
    }
    pub(super) unsafe fn free(self) {
        for f in [self.reg, self.bold, self.ital, self.bi, self.mono] {
            let _ = DeleteObject(f.into());
        }
    }
}

/// Palette + DPI-scaled constants shared by every `run_block` call of one render pass.
pub(super) struct RunCtx {
    pub(super) code_bg: u32,
    pub(super) accent: u32,
    pub(super) base_color: u32,
    pub(super) code_pad: i32,
    pub(super) line_lead: i32,
    pub(super) ul_off: i32,
}

pub(super) fn ctx_for(hwnd: HWND, c: &MdColors, base_color: u32) -> RunCtx {
    RunCtx {
        code_bg: c.code_bg,
        accent: c.accent,
        base_color,
        code_pad: crate::win::dpi_scale(hwnd, 3),
        line_lead: crate::win::dpi_scale(hwnd, 3),
        ul_off: crate::win::dpi_scale(hwnd, 2),
    }
}

/// A measured, placeable token from the flattened run stream. `doc` is the token's slice of the
/// selection document (`None` on dry/unselectable passes).
pub(super) enum Tok {
    Word {
        s: Vec<u16>,
        w: i32,
        pad: i32,
        font: HFONT,
        color: u32,
        code: bool,
        strike: bool,
        link: Option<String>,
        doc: Option<(usize, usize)>,
        spec: FontSpec,
    },
    Space(i32),
    Break,
}

/// Selection wiring for one [`run_block`] call: the active range, the document (to measure a
/// partially-selected word), this block's per-run document offsets, and the hit collector.
pub(super) struct RunSel<'a> {
    pub(super) range: Option<(usize, usize)>,
    pub(super) doc: &'a str,
    pub(super) bases: &'a [usize],
    pub(super) hits: &'a mut Vec<SelHit>,
    pub(super) bg: u32,
}

/// How many UTF-16 units from the front of `w16` fit within `max_w` px — the char-level split
/// point for a token too wide to wrap any other way.
///
/// `at[i]` is the source byte offset of the character unit `i` belongs to, so two units sharing
/// an offset are the halves of one surrogate pair and the boundary walks back off them. Always
/// returns at least one whole character: a single glyph wider than the column still has to be
/// placed somewhere (the cell clip contains it), and returning 0 would spin the caller's loop.
unsafe fn units_fitting(hdc: HDC, w16: &[u16], max_w: i32, at: &[usize]) -> usize {
    // Measure at most this many units per probe. `GetTextExtentExPointW` costs time proportional
    // to the string HANDED to it, not to the answer, and the caller re-probes the whole shrinking
    // remainder each round — so passing a multi-megabyte token straight through makes splitting it
    // quadratic and the window appears to hang. No real column fits anywhere near this many
    // characters, so the cap only ever costs an extra chunk boundary, never correctness.
    const PROBE_CAP: usize = 2048;
    let probe = &w16[..w16.len().min(PROBE_CAP)];
    let mut fit = 0i32;
    let mut sz = SIZE::default();
    let ok = GetTextExtentExPointW(
        hdc,
        PCWSTR(probe.as_ptr()),
        probe.len() as i32,
        max_w,
        Some(&mut fit as *mut i32),
        None,
        &mut sz,
    )
    .as_bool();
    if !ok {
        return probe.len();
    }
    let mut n = (fit as usize).min(probe.len());
    // Back off any boundary that would cut a character apart. Two units sharing a source byte
    // offset are the halves of one surrogate pair; a combining mark or a ZWJ joiner belongs to
    // the character before it, so splitting there detaches an accent or shears an emoji cluster.
    while n > 0 && n < probe.len() && splits_a_cluster(probe, at, n) {
        n -= 1;
    }
    if n == 0 {
        // A single character wider than the whole column still has to go somewhere; the cell
        // clip contains it, and returning 0 would spin the caller's loop forever.
        n = 1;
        while n < probe.len() && at[n] == at[n - 1] {
            n += 1;
        }
    }
    n
}

/// Would breaking `w16` before unit `n` split one user-perceived character?
///
/// `at[i]` is the source byte offset of the character unit `i` belongs to, so equal offsets mean
/// one codepoint's surrogate halves. Combining marks and ZWJ sequences are SEPARATE codepoints
/// with their own offsets, so they need the explicit checks.
fn splits_a_cluster(w16: &[u16], at: &[usize], n: usize) -> bool {
    const ZWJ: u16 = 0x200D;
    if at[n] == at[n - 1] {
        return true; // mid surrogate pair
    }
    if w16[n - 1] == ZWJ {
        return true; // joiner may not end a cluster
    }
    // A lone BMP unit is its own char; a leading surrogate is never a mark, so this is enough.
    char::from_u32(w16[n] as u32).is_some_and(|c| c == '\u{200D}' || is_combining(c))
}

/// Flattens `runs` into measured tokens (words / spaces / hard breaks), each remembering the
/// run bytes it came from so it maps back to the selection document. Splits any word wider
/// than `width` into character chunks (the way CSS `overflow-wrap: anywhere` does), so a
/// single token can never run off the pane edge unbroken. `sel` only needs read access here
/// (per-run document-offset bases); the draw pass reborrows it mutably for hit-testing.
unsafe fn tokenize_runs(
    hdc: HDC,
    runs: &[Run],
    fonts: &Fonts,
    width: i32,
    ctx: &RunCtx,
    sel: Option<&RunSel>,
) -> Vec<Tok> {
    let mut toks: Vec<Tok> = Vec::new();
    for (ri, r) in runs.iter().enumerate() {
        let f = fonts.pick(r);
        let spec = fonts.spec(r);
        let color = if r.link.is_some() {
            ctx.accent
        } else {
            ctx.base_color
        };
        let pad = if r.code { ctx.code_pad } else { 0 };
        SelectObject(hdc, f.into());
        let base = sel.and_then(|s| s.bases.get(ri).copied());
        let mut word: Vec<u16> = Vec::new();
        // Per UTF-16 unit of `word`, the byte offset of the CHARACTER that unit belongs to. Only
        // read when an over-wide token has to be split below, where it both maps each chunk back
        // to its slice of the selection document and keeps a surrogate pair from being cut in half.
        let mut unit_at: Vec<usize> = Vec::new();
        let mut wstart = 0usize; // byte offset in `r.text` where the pending word began
        macro_rules! flush_word {
            ($wend:expr) => {
                if !word.is_empty() {
                    let wend: usize = $wend;
                    let mut sz = SIZE::default();
                    let _ = GetTextExtentPoint32W(hdc, &word, &mut sz);
                    if width > 0 && sz.cx + 2 * pad > width && word.len() > 1 {
                        // A token wider than the ENTIRE line offers the greedy breaker below no
                        // break opportunity, so it used to be placed anyway - running over the
                        // next table column and off the pane edge, which is exactly what a CSV
                        // full of 90-character API keys looked like. Split it between characters
                        // instead, the way CSS `overflow-wrap: anywhere` does.
                        let mut a = 0usize;
                        while a < word.len() {
                            let n = units_fitting(
                                hdc,
                                &word[a..],
                                (width - 2 * pad).max(1),
                                &unit_at[a..],
                            );
                            let b = (a + n).min(word.len());
                            let mut csz = SIZE::default();
                            if !GetTextExtentPoint32W(hdc, &word[a..b], &mut csz).as_bool() {
                                // A failed measure would leave the chunk 0 px wide, which both
                                // stacks the chunks on top of each other and gives selection a
                                // zero-width hit rect. The whole-token width is already known,
                                // so pro-rate it rather than trusting the zero.
                                csz.cx = sz.cx * (b - a) as i32 / word.len().max(1) as i32;
                            }
                            let to = if b < word.len() { unit_at[b] } else { wend };
                            let from = unit_at[a];
                            toks.push(Tok::Word {
                                s: word[a..b].to_vec(),
                                w: csz.cx + 2 * pad,
                                pad,
                                font: f,
                                color,
                                code: r.code,
                                strike: r.strike,
                                link: r.link.clone(),
                                doc: base.map(|bb| (bb + from, bb + to)),
                                spec,
                            });
                            a = b;
                        }
                        word.clear();
                    } else {
                        toks.push(Tok::Word {
                            s: core::mem::take(&mut word),
                            w: sz.cx + 2 * pad,
                            pad,
                            font: f,
                            color,
                            code: r.code,
                            strike: r.strike,
                            link: r.link.clone(),
                            doc: base.map(|b| (b + wstart, b + wend)),
                            spec,
                        });
                    }
                    unit_at.clear();
                }
            };
        }
        let mut chars = r.text.char_indices().peekable();
        while let Some((ci, ch)) = chars.next() {
            match ch {
                '\n' => {
                    flush_word!(ci);
                    toks.push(Tok::Break);
                }
                ' ' | '\t' => {
                    flush_word!(ci);
                    let mut sz = SIZE::default();
                    let sp = [b' ' as u16];
                    let _ = GetTextExtentPoint32W(hdc, &sp, &mut sz);
                    toks.push(Tok::Space(sz.cx));
                }
                _ => {
                    if word.is_empty() {
                        wstart = ci;
                    }
                    let mut b = [0u16; 2];
                    for u in ch.encode_utf16(&mut b) {
                        word.push(*u);
                        unit_at.push(ci);
                    }
                    // Scripts that don't put spaces between words get their break
                    // opportunities here instead. Without this a Chinese/Japanese paragraph is
                    // ONE token, and the greedy line-breaker below places an over-wide token
                    // anyway — so the whole paragraph ran off the pane edge and was clipped.
                    if let Some(&(ni, next)) = chars.peek() {
                        if can_break_between(ch, next) {
                            flush_word!(ni);
                        }
                    }
                }
            }
        }
        flush_word!(r.text.len());
    }
    toks
}

/// Greedy line-break of `toks` into `width`-wide lines, remembering each placed word's
/// line-relative x. Returns `(placements, line width)` per line.
fn break_into_lines(toks: &[Tok], width: i32) -> Vec<(Vec<(i32, usize)>, i32)> {
    let mut lines: Vec<(Vec<(i32, usize)>, i32)> = Vec::new(); // (placements, line width)
    let mut cur: Vec<(i32, usize)> = Vec::new();
    let mut cx = 0;
    let mut pending_space = 0;
    let mut line_start = true;
    for (idx, tok) in toks.iter().enumerate() {
        match tok {
            Tok::Break => {
                lines.push((core::mem::take(&mut cur), cx));
                cx = 0;
                pending_space = 0;
                line_start = true;
            }
            Tok::Space(sw) => {
                if !line_start {
                    pending_space += *sw;
                }
            }
            Tok::Word { w, .. } => {
                if !line_start && cx + pending_space + *w > width {
                    lines.push((core::mem::take(&mut cur), cx));
                    cx = 0;
                    pending_space = 0;
                    line_start = true;
                }
                if !line_start {
                    cx += pending_space;
                }
                pending_space = 0;
                cur.push((cx, idx));
                cx += *w;
                line_start = false;
            }
        }
    }
    if !cur.is_empty() || !line_start {
        lines.push((cur, cx));
    }
    lines
}

/// Draws every laid-out line: selection fill + hit rects first (an opaque fill after the
/// glyphs would erase them), then each word's glyphs, inline-code shading, strikethrough and
/// link underline/hit-rect.
#[allow(clippy::too_many_arguments)] // GDI draw core: hdc + geometry + mode flags, no struct gain
unsafe fn draw_wrapped_lines(
    hdc: HDC,
    toks: &[Tok],
    lines: &[(Vec<(i32, usize)>, i32)],
    x0: i32,
    y: i32,
    width: i32,
    align: u8,
    line_h: i32,
    ctx: &RunCtx,
    links: &mut Vec<LinkHit>,
    mut sel: Option<&mut RunSel>,
) {
    // Copied out so the draw loop can read the selection while `sel` is mutably reborrowed for
    // the per-line fill.
    let (sel_rng, sel_bg) = match sel.as_ref() {
        Some(s) => (s.range, s.bg),
        None => (None, 0),
    };
    for (li, (placed, lw)) in lines.iter().enumerate() {
        let xoff = match align {
            1 => (width - lw).max(0) / 2,
            2 => (width - lw).max(0),
            _ => 0,
        };
        let cy = y + li as i32 * line_h;
        // Selection fill + hit rects BEFORE the glyphs — an opaque fill after would erase them.
        if let Some(s) = sel.as_deref_mut() {
            line_sel(hdc, toks, placed, x0 + xoff, cy, line_h, s);
        }
        for (rx, idx) in placed {
            let Tok::Word {
                s,
                w,
                pad,
                font,
                color,
                code,
                strike,
                link,
                doc,
                ..
            } = &toks[*idx]
            else {
                continue;
            };
            let cx = x0 + xoff + rx;
            SelectObject(hdc, (*font).into());
            SetTextColor(hdc, COLORREF(*color));
            if *code {
                // Shaded panel behind inline code (opaque ExtTextOut). It would paint OVER the
                // selection fill, so when the span is selected the panel IS the highlight.
                let hot = sel_rng
                    .zip(*doc)
                    .is_some_and(|((ss, se), (ds, de))| ss < de && se > ds);
                let r = RECT {
                    left: cx,
                    top: cy,
                    right: cx + *w,
                    bottom: cy + line_h,
                };
                SetBkColor(hdc, COLORREF(if hot { sel_bg } else { ctx.code_bg }));
                SetBkMode(hdc, OPAQUE);
                let _ = ExtTextOutW(
                    hdc,
                    cx + *pad,
                    cy,
                    ETO_OPAQUE,
                    Some(&r as *const RECT),
                    PCWSTR(s.as_ptr()),
                    s.len() as u32,
                    None,
                );
                SetBkMode(hdc, TRANSPARENT);
            } else {
                let _ = ExtTextOutW(
                    hdc,
                    cx,
                    cy,
                    ETO_OPTIONS(0),
                    None,
                    PCWSTR(s.as_ptr()),
                    s.len() as u32,
                    None,
                );
            }
            if *strike {
                hline(hdc, cx + *pad, cx + *w - *pad, cy + line_h / 2, *color);
            }
            if let Some(url) = link {
                hline(
                    hdc,
                    cx + *pad,
                    cx + *w - *pad,
                    cy + line_h - ctx.ul_off,
                    *color,
                );
                links.push(LinkHit {
                    rect: RECT {
                        left: cx,
                        top: cy,
                        right: cx + *w,
                        bottom: cy + line_h,
                    },
                    url: url.clone(),
                });
            }
        }
    }
}

/// Word-wrap + draw a block's inline `runs` starting at `(x0, y)` within `width`.
/// `align`: 0 left, 1 center, 2 right (per-line offset). `dry` measures without drawing
/// (no GDI output, no link/selection collection). Returns `(y_after, widest_line)`.
#[allow(clippy::too_many_arguments)] // GDI layout core: hdc + geometry + mode flags, no struct gain
pub(super) unsafe fn run_block(
    hdc: HDC,
    runs: &[Run],
    fonts: &Fonts,
    x0: i32,
    y: i32,
    width: i32,
    align: u8,
    dry: bool,
    ctx: &RunCtx,
    links: &mut Vec<LinkHit>,
    sel: Option<&mut RunSel>,
) -> (i32, i32) {
    if runs.iter().all(|r| r.text.trim().is_empty()) {
        return (y, 0);
    }
    // Line height from the regular font's metrics + a little leading.
    let old_font = SelectObject(hdc, fonts.reg.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let line_h = tm.tmHeight + tm.tmExternalLeading + ctx.line_lead;

    let toks = tokenize_runs(hdc, runs, fonts, width, ctx, sel.as_deref());
    let lines = break_into_lines(&toks, width);
    if lines.is_empty() {
        SelectObject(hdc, old_font);
        return (y, 0);
    }
    let max_w = lines.iter().map(|(_, w)| *w).max().unwrap_or(0);

    if !dry {
        draw_wrapped_lines(
            hdc, &toks, &lines, x0, y, width, align, line_h, ctx, links, sel,
        );
    }
    SelectObject(hdc, old_font);
    (y + lines.len() as i32 * line_h, max_w)
}

/// Fill the selection background behind one laid-out line's selected words (and the spaces
/// between them), and record every word's hit rect. Runs before the line's glyphs are drawn.
pub(super) unsafe fn line_sel(
    hdc: HDC,
    toks: &[Tok],
    placed: &[(i32, usize)],
    xbase: i32,
    cy: i32,
    line_h: i32,
    sel: &mut RunSel,
) {
    let mut prev: Option<(usize, i32)> = None; // (doc end, right x) of the previous word
    for (rx, idx) in placed {
        let Tok::Word {
            w,
            pad,
            font,
            doc,
            spec,
            code,
            ..
        } = &toks[*idx]
        else {
            continue;
        };
        let Some((ds, de)) = *doc else {
            prev = None;
            continue;
        };
        let cx = xbase + rx;
        sel.hits.push(SelHit {
            rect: RECT {
                left: cx,
                top: cy,
                right: cx + *w,
                bottom: cy + line_h,
            },
            start: ds,
            end: de,
            font: *spec,
            text_x: cx + *pad,
        });
        if let Some((ss, se)) = sel.range {
            // The gap holds this line's inter-word spaces: fill it only when the selection
            // actually spans across it (so a selection ending mid-line doesn't overhang).
            if let Some((pde, prx)) = prev {
                if ss <= pde && se >= ds && prx < cx {
                    fill(hdc, prx, cy, cx, cy + line_h, sel.bg);
                }
            }
            // An inline-code span paints its own opaque panel in the selection colour (see the
            // draw loop) — filling here too would just be overpainted.
            if ss < de && se > ds && !*code {
                let (x1, x2) = if ss <= ds && se >= de {
                    (cx, cx + *w) // fully selected: the whole token box, padding included
                } else {
                    // Partly selected (a selection end lands inside this word): measure it.
                    let t = sel.doc.get(ds..de).unwrap_or("");
                    let a = ss.max(ds) - ds;
                    let b = se.min(de) - ds;
                    SelectObject(hdc, (*font).into());
                    let x = cx + *pad;
                    (
                        x + highlight::disp_extent(hdc, t, a),
                        x + highlight::disp_extent(hdc, t, b),
                    )
                };
                fill(hdc, x1, cy, x2, cy + line_h, sel.bg);
            }
        }
        prev = Some((de, cx + *w));
    }
}

/// Fill a rect with a solid colour.
pub(super) unsafe fn fill(hdc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, color: u32) {
    if x2 <= x1 {
        return;
    }
    let r = RECT {
        left: x1,
        top: y1,
        right: x2,
        bottom: y2,
    };
    let b = CreateSolidBrush(COLORREF(color));
    FillRect(hdc, &r, b);
    let _ = DeleteObject(b.into());
}

/// A 1px horizontal line (strike / underline / grid) in `color`.
pub(super) unsafe fn hline(hdc: HDC, x1: i32, x2: i32, y: i32, color: u32) {
    let pen = CreatePen(PS_SOLID, 1, COLORREF(color));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, x1, y, None);
    let _ = LineTo(hdc, x2, y);
    SelectObject(hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

/// Draw a short single-line string at `(x, y)` (list markers).
pub(super) unsafe fn draw_at(hdc: HDC, text: &str, x: i32, y: i32, font: HFONT, color: u32) {
    let old = SelectObject(hdc, font.into());
    SetTextColor(hdc, COLORREF(color));
    let mut w: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT {
        left: x,
        top: y,
        right: x + 400,
        bottom: y + 100,
    };
    DrawTextW(hdc, &mut w, &mut r, DT_LEFT | DT_TOP | DT_NOPREFIX);
    SelectObject(hdc, old);
}

/// Draw a GitHub-style task-list checkbox at `(x, y)` (its top-left), in place of a list
/// bullet. Unchecked = a rounded outline box; checked = an accent-filled box with a white
/// tick. `(x, y)` is already DPI-scaled; the box sizes itself off the body line.
pub(super) unsafe fn draw_checkbox(hwnd: HWND, hdc: HDC, x: i32, y: i32, done: bool, c: &MdColors) {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let sz = sc(14);
    let (l, t) = (x, y + sc(2)); // nudge down to sit on the 16px text line
    let (r, b) = (l + sz, t + sz);
    let rad = sc(4);
    let pen = CreatePen(
        PS_SOLID,
        sc(1).max(1),
        COLORREF(if done { c.accent } else { c.border }),
    );
    let brush = CreateSolidBrush(COLORREF(if done { c.accent } else { c.bg }));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let ob = SelectObject(hdc, HGDIOBJ(brush.0));
    let _ = RoundRect(hdc, l, t, r, b, rad, rad);
    SelectObject(hdc, op);
    SelectObject(hdc, ob);
    let _ = DeleteObject(HGDIOBJ(pen.0));
    let _ = DeleteObject(HGDIOBJ(brush.0));
    if done {
        // A white tick reads on the accent fill in both light and dark themes.
        let cw = CreatePen(PS_SOLID, sc(2).max(2), COLORREF(0x00FF_FFFF));
        let oc = SelectObject(hdc, HGDIOBJ(cw.0));
        let fx = |f: f32| l + (sz as f32 * f) as i32;
        let fy = |f: f32| t + (sz as f32 * f) as i32;
        let _ = MoveToEx(hdc, fx(0.24), fy(0.52), None);
        let _ = LineTo(hdc, fx(0.42), fy(0.70));
        let _ = LineTo(hdc, fx(0.76), fy(0.30));
        SelectObject(hdc, oc);
        let _ = DeleteObject(HGDIOBJ(cw.0));
    }
}

/// Re-create the font a drawn token was measured with (hit-testing; caller frees it).
pub(crate) unsafe fn font_for(hwnd: HWND, s: FontSpec) -> HFONT {
    font(hwnd, s.px, s.bold, s.italic, s.mono)
}

/// Create a font: `px` @96dpi (DPI-scaled), Segoe UI (or Consolas if `mono`), bold/italic.
pub(super) unsafe fn font(hwnd: HWND, px: i32, bold: bool, italic: bool, mono: bool) -> HFONT {
    let h = crate::win::dpi_scale(hwnd, px);
    let face = crate::win::wide(if mono { "Consolas" } else { "Segoe UI" });
    CreateFontW(
        -h,
        0,
        0,
        0,
        if bold { 700 } else { 400 },
        u32::from(italic),
        0,
        0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        Default::default(),
        PCWSTR(face.as_ptr()),
    )
}
