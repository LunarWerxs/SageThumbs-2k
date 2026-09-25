//! Inline run layout: the word-wrapper, the font cache behind it, and the primitive
//! draws every block type shares.
//!
//! This is the hot loop of the whole renderer - it runs per block, per paint - so the
//! fonts are cached per style rather than created per run, and the tokenizer walks the
//! text once.

use super::*;
mod fonts;
#[cfg(test)]
use fonts::*;
mod wrap;
pub(crate) use fonts::font_for;
pub(super) use fonts::{font, FontCache, Fonts};
use wrap::*;
pub(super) use wrap::{RunSel, Tok};

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
        code_pad: st2k_appkit::win::dpi_scale(hwnd, 3),
        line_lead: st2k_appkit::win::dpi_scale(hwnd, 3),
        ul_off: st2k_appkit::win::dpi_scale(hwnd, 2),
    }
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
            let cx = x0 + xoff + rx;
            draw_word(
                hdc,
                &toks[*idx],
                cx,
                cy,
                line_h,
                ctx,
                sel_rng,
                sel_bg,
                links,
            );
        }
    }
}

/// Draw one placed word token: its glyphs, inline-code panel, strikethrough and link
/// underline/hit rect (a non-word token is skipped).
#[allow(clippy::too_many_arguments)] // GDI draw core: hdc + geometry + ctx, no struct gain
unsafe fn draw_word(
    hdc: HDC,
    tok: &Tok,
    cx: i32,
    cy: i32,
    line_h: i32,
    ctx: &RunCtx,
    sel_rng: Option<(usize, usize)>,
    sel_bg: u32,
    links: &mut Vec<LinkHit>,
) {
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
    } = tok
    else {
        return;
    };
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
        prev = sel_word(hdc, &toks[*idx], *rx, xbase, cy, line_h, sel, prev);
    }
}

/// Record one placed line token's hit rect (skipping non-word tokens and words with no
/// document span) and fill its selection background, returning the `(doc end, right x)`
/// pair the next word's inter-word gap fill needs.
#[allow(clippy::too_many_arguments)] // hdc + geometry + selection state, no struct gain
unsafe fn sel_word(
    hdc: HDC,
    tok: &Tok,
    rx: i32,
    xbase: i32,
    cy: i32,
    line_h: i32,
    sel: &mut RunSel,
    prev: Option<(usize, i32)>,
) -> Option<(usize, i32)> {
    let Tok::Word {
        w,
        pad,
        font,
        doc,
        spec,
        code,
        ..
    } = tok
    else {
        return prev;
    };
    let (ds, de) = (*doc)?;
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
        fill_word_sel(
            hdc, sel, cx, *w, *pad, *font, ds, de, ss, se, *code, cy, line_h,
        );
    }
    Some((de, cx + *w))
}

/// Fill the selection background behind one word's selected extent: the whole token box
/// when the range covers it, otherwise the measured sub-extent. Inline-code spans are
/// skipped (their own opaque panel is the highlight).
#[allow(clippy::too_many_arguments)] // geometry + span bounds + GDI handle, no struct gain
unsafe fn fill_word_sel(
    hdc: HDC,
    sel: &RunSel,
    cx: i32,
    w: i32,
    pad: i32,
    font: HFONT,
    ds: usize,
    de: usize,
    ss: usize,
    se: usize,
    code: bool,
    cy: i32,
    line_h: i32,
) {
    if ss < de && se > ds && !code {
        let (x1, x2) = if ss <= ds && se >= de {
            (cx, cx + w) // fully selected: the whole token box, padding included
        } else {
            // Partly selected (a selection end lands inside this word): measure it.
            let t = sel.doc.get(ds..de).unwrap_or("");
            let a = ss.max(ds) - ds;
            let b = se.min(de) - ds;
            SelectObject(hdc, font.into());
            let x = cx + pad;
            (
                x + highlight::disp_extent(hdc, t, a),
                x + highlight::disp_extent(hdc, t, b),
            )
        };
        fill(hdc, x1, cy, x2, cy + line_h, sel.bg);
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

/// A rounded box: `brush_color` fill inside a `pen_w`-wide `pen_color` outline, with
/// `radius`-px corners. Creates and tears down its own pen and brush.
pub(super) unsafe fn rounded_box(
    hdc: HDC,
    r: RECT,
    radius: i32,
    pen_w: i32,
    pen_color: u32,
    brush_color: u32,
) {
    let pen = CreatePen(PS_SOLID, pen_w, COLORREF(pen_color));
    let brush = CreateSolidBrush(COLORREF(brush_color));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let ob = SelectObject(hdc, HGDIOBJ(brush.0));
    let _ = RoundRect(hdc, r.left, r.top, r.right, r.bottom, radius, radius);
    SelectObject(hdc, op);
    SelectObject(hdc, ob);
    let _ = DeleteObject(HGDIOBJ(pen.0));
    let _ = DeleteObject(HGDIOBJ(brush.0));
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
    let sc = |v: i32| st2k_appkit::win::dpi_scale(hwnd, v);
    let sz = sc(14);
    let (l, t) = (x, y + sc(2)); // nudge down to sit on the 16px text line
    let (r, b) = (l + sz, t + sz);
    let rad = sc(4);
    rounded_box(
        hdc,
        RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        },
        rad,
        sc(1).max(1),
        if done { c.accent } else { c.border },
        if done { c.accent } else { c.bg },
    );
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

#[cfg(test)]
mod font_cache_tests;
