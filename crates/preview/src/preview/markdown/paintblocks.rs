//! Painting one block: heading, paragraph, code, list item, quote.

use super::*;

/// What every block painter needs besides its own block: the target, the column, the palette and
/// the accumulators.
pub(super) struct PaintCtx<'a> {
    pub(super) hwnd: HWND,
    pub(super) hdc: HDC,
    pub(super) rc: &'a RECT,
    pub(super) x0: i32,
    pub(super) full_w: i32,
    pub(super) c: &'a MdColors,
    pub(super) links: &'a mut Vec<LinkHit>,
    pub(super) rsel: &'a mut RunSel<'a>,
    pub(super) fonts_cache: &'a mut FontCache,
}

impl PaintCtx<'_> {
    /// `v` design pixels at the target window's DPI.
    pub(super) fn sc(&self, v: i32) -> i32 {
        st2k_appkit::win::dpi_scale(self.hwnd, v)
    }

    /// One block painter's `run_block` step: the `spec = (px, bold, italic)` font entry, the
    /// colour `fg` for the shared run context, `indent` px of left inset, and whether the block
    /// centres its lines. Culls against the clip rect and collects the block's link hits into
    /// the accumulators. Returns the y after the block.
    pub(super) unsafe fn run_block_in(
        &mut self,
        runs: &[Run],
        spec: (i32, bool, bool),
        y: i32,
        indent: i32,
        center: bool,
        fg: u32,
    ) -> i32 {
        let fonts = self.fonts_cache.get(self.hwnd, spec.0, spec.1, spec.2);
        let ctx = ctx_for(self.hwnd, self.c, fg);
        let (ny, _) = run_block(
            self.hdc,
            runs,
            fonts,
            self.x0 + indent,
            y,
            self.full_w - indent,
            if center { 1 } else { 0 },
            y >= self.rc.bottom,
            &ctx,
            &mut *self.links,
            Some(&mut *self.rsel),
        );
        ny
    }
}

/// `Block::Heading` paint arm: heading text + the h1/h2 hairline underline.
pub(super) unsafe fn paint_heading(
    p: &mut PaintCtx<'_>,
    level: u8,
    runs: &[Run],
    center: bool,
    first: bool,
    mut y: i32,
) -> i32 {
    if !first {
        y += p.sc(8); // extra top margin before a heading (GitHub 24px total)
    }
    y = p.run_block_in(runs, (heading_px(level), true, false), y, 0, center, p.c.fg);
    if level <= 2 {
        // GitHub-style hairline under h1/h2.
        hline(p.hdc, p.x0, p.x0 + p.full_w, y + p.sc(4), p.c.border);
        y += p.sc(8);
    }
    y + p.sc(10)
}

/// `Block::Para` paint arm.
pub(super) unsafe fn paint_para(p: &mut PaintCtx<'_>, runs: &[Run], center: bool, y: i32) -> i32 {
    let ny = p.run_block_in(runs, (BODY_PX, false, false), y, 0, center, p.c.fg);
    if ny > y {
        ny + p.sc(14)
    } else {
        y
    }
}

/// `Block::Code` paint arm: the rounded panel + syntax-highlighted, unwrapped lines.
pub(super) unsafe fn paint_code(
    p: &mut PaintCtx<'_>,
    text: &str,
    lang: highlight::Lang,
    y: i32,
    base: usize,
) -> i32 {
    let (hwnd, hdc, rc, x0, full_w, c) = (p.hwnd, p.hdc, p.rc, p.x0, p.full_w, p.c);
    let sel_range = p.rsel.range;
    let sel_hits = &mut *p.rsel.hits;
    let sc = |v: i32| st2k_appkit::win::dpi_scale(hwnd, v);
    let f = font(hwnd, 13, false, false, true);
    let pad = sc(12);
    // Code isn't wrapped (line-per-line), so the panel height is line_count * line_h.
    let old = SelectObject(hdc, f.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let line_h = tm.tmHeight + tm.tmExternalLeading;
    SelectObject(hdc, old);
    let nlines = text.split('\n').count().max(1) as i32;
    let h = nlines * line_h + 2 * pad;
    // Cull: only paint the panel + code when the block overlaps the viewport.
    // `paint_lines` itself clips to [rc.top, rc.bottom], so a code block taller than the
    // pane draws only its visible lines. `h` is cheap line-count math, so `y` advances
    // either way and the scroll height stays correct.
    if y < rc.bottom && y + h > rc.top {
        // GitHub 6px-radius code panel.
        rounded_box(
            hdc,
            RECT {
                left: x0,
                top: y,
                right: x0 + full_w,
                bottom: y + h,
            },
            sc(6),
            1,
            c.code_bg,
            c.code_bg,
        );
        // The code text is its own slice of the selection document: translate the
        // range into it (a selection reaching past either end just clamps, which is
        // exactly the "selection continues outside this block" case).
        let local = sel_range.map(|(s, e)| (s.saturating_sub(base), e.saturating_sub(base)));
        let mut ls = highlight::LineSel {
            hits: sel_hits,
            base,
            spec: FontSpec {
                px: 13,
                bold: false,
                italic: false,
                mono: true,
            },
        };
        highlight::paint_lines(
            hdc,
            text,
            lang,
            x0 + pad,
            y + pad,
            full_w - 2 * pad,
            rc.top,
            rc.bottom,
            f,
            c.fg,
            local,
            Some(&mut ls),
        );
    }
    let _ = DeleteObject(f.into());
    y + h + sc(14)
}

/// `Block::Item` paint arm: bullet/number or task checkbox, then the item's runs.
pub(super) unsafe fn paint_item(
    p: &mut PaintCtx<'_>,
    depth: u8,
    marker: &str,
    runs: &[Run],
    task: Option<bool>,
    y: i32,
) -> i32 {
    let indent = p.sc(22) * (depth as i32 + 1);
    let mx = p.x0 + indent - p.sc(18);
    // Both the marker and the item's runs draw in the plain body style, so one cache lookup
    // covers both (the marker just borrows `.reg` instead of running the wrapper's own layout).
    let fonts = p.fonts_cache.get(p.hwnd, BODY_PX, false, false);
    match task {
        // GFM task item: a GitHub-style checkbox in place of the bullet.
        Some(done) => draw_checkbox(p.hwnd, p.hdc, mx, y, done, p.c),
        // Ordinary bullet / number in the muted colour.
        None => draw_at(p.hdc, marker, mx, y, fonts.reg, p.c.muted),
    }
    p.run_block_in(runs, (BODY_PX, false, false), y, indent, false, p.c.fg) + p.sc(4)
}

/// `Block::Quote` paint arm: the runs, then the GitHub-style gray quote bar.
pub(super) unsafe fn paint_quote(p: &mut PaintCtx<'_>, runs: &[Run], y: i32) -> i32 {
    let indent = p.sc(16);
    let y_start = y;
    let y = p.run_block_in(runs, (BODY_PX, false, true), y, indent, false, p.c.muted);
    // GitHub-style gray quote bar spanning the quote's height.
    let pen = CreatePen(PS_SOLID, p.sc(4), COLORREF(p.c.border));
    let op = SelectObject(p.hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(p.hdc, p.x0 + p.sc(2), y_start, None);
    let _ = LineTo(p.hdc, p.x0 + p.sc(2), y);
    SelectObject(p.hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));
    y + p.sc(14)
}
