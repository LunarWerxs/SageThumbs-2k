//! In-process GDI markdown renderer for the Quick preview viewer.
//!
//! `pulldown-cmark` -> a flat list of styled BLOCKS -> GDI draw. Chosen over a WebView2 host to
//! keep the EXE lean (one small pure-Rust dep, no runtime dependency) and the render capturable
//! by `PrintWindow` (so it's `--shot`-verifiable). Renders GitHub-style: headings, paragraphs,
//! fenced/indented code, lists, block quotes, rules, GFM tables (full grid + zebra rows +
//! per-column alignment), inline **bold**/*italic*/`code`/~~strike~~/links, AND:
//! - **raw HTML** (the README "hero" pattern: `<div align="center">`, `<h1>`, `<p>`, `<img>`,
//!   `<a>`, `<b>/<i>`, `<br>`, `<table>`, lists, `<details>`) via the zero-dep tag feeder in
//!   [`super::mdhtml`] driving the same [`Builder`];
//! - **images**: local files decode through our own pipeline into cached DIBs and draw inline
//!   (aspect-scaled, `width`/`%` attrs honored, clickable when link-wrapped); remote (http/data)
//!   sources are NEVER fetched — they render as alt-text pills (privacy: a previewed README
//!   must not phone home).
//!
//! The content column is capped at a GitHub-like max width and centered in the pane.

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use std::path::{Path, PathBuf};
use windows::core::PCWSTR;

use super::content::RenderData;
use super::highlight;
use super::selection::{FontSpec, SelHit};
use windows::Win32::Foundation::{COLORREF, HWND, RECT, SIZE};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateFontW, CreatePen, CreateSolidBrush, DeleteDC, DeleteObject,
    DrawTextW, ExtTextOutW, FillRect, GetTextExtentPoint32W, GetTextMetricsW, LineTo, MoveToEx,
    RoundRect, SelectObject, SetBkColor, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_QUALITY, DT_LEFT, DT_NOPREFIX, DT_TOP,
    ETO_OPAQUE, ETO_OPTIONS, HALFTONE, HDC, HFONT, HGDIOBJ, OPAQUE, OUT_DEFAULT_PRECIS, PS_SOLID,
    SRCCOPY, TEXTMETRICW, TRANSPARENT,
};

/// Theme-resolved palette handed in by the viewer.
pub(super) struct MdColors {
    pub bg: u32,
    pub fg: u32,
    pub muted: u32,
    pub accent: u32,
    pub code_bg: u32,
    pub border: u32,
    /// Selection highlight fill.
    pub sel: u32,
}

/// Selection wiring for one [`render`] pass: the active range (rendered-document byte offsets)
/// and the hit collector, both rebuilt every paint.
pub(super) struct MdSel<'a> {
    pub range: Option<(usize, usize)>,
    pub hits: &'a mut Vec<SelHit>,
}

/// One inline styled run (a stretch of text sharing a style within a block).
#[derive(Clone)]
pub(super) struct Run {
    text: String,
    bold: bool,
    italic: bool,
    code: bool,   // inline `code` / alt-text pill (mono + shaded background)
    strike: bool, // ~~strikethrough~~
    link: Option<String>, // Some(dest URL) => accent colour + underline + clickable
}

/// A clickable on-screen link rectangle (client coords, already scroll-adjusted for the paint
/// that produced it) plus its destination URL. Collected fresh every markdown render so the
/// viewer can hit-test clicks; one wrapped link yields several rects (one per line segment).
pub(super) struct LinkHit {
    pub rect: RECT,
    pub url: String,
}

/// One entry in the heading outline (table of contents): the heading level (1-6), its plain text,
/// and the scroll offset (document px from the top) that brings it to the top of the pane. Collected
/// fresh every markdown render (positions depend on the pane width).
pub(super) struct TocEntry {
    pub level: u8,
    pub text: String,
    pub target: i32,
}

/// Requested display width of an image block (`width="820"` / `width="31%"` / none).
#[derive(Clone, Copy)]
pub(super) enum ImgW {
    Natural,
    Px(i32),
    Pct(u32),
}

/// A block-level image: local src resolved + decoded at draw time (cached), remote never fetched.
pub(super) struct ImgBlock {
    pub src: String,
    pub alt: String,
    pub width: ImgW,
    pub center: bool,
    pub link: Option<String>,
}

/// One cached inline-image state. Remote fetches resolve asynchronously: the paint that first
/// sees the src inserts `Pending` + spawns the worker, and the posted result flips it to
/// `Ready`/`Failed` (then invalidates). `RenderData`'s `Drop` frees the DIB.
pub(super) enum ImgSlot {
    /// Remote fetch in flight — draw the alt-text pill meanwhile.
    Pending,
    /// Decode/fetch failed (or blocked: over caps, UNC, non-HTTPS) — alt-text pill.
    Failed,
    Ready(RenderData),
}

/// The per-document image cache living in `ViewerState` (cleared on every load).
pub(super) type ImgCache = std::collections::HashMap<String, ImgSlot>;

/// Is this src a web resource (fetched only via the opt-in remote-images toggle)?
pub(super) fn is_remote_src(src: &str) -> bool {
    let l = src.trim_start().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://")
}

/// Does the markdown contain any heading (markdown `#`/setext OR a raw-HTML `<h1>`-`<h6>`)?
/// Used ONCE at load time to decide whether the outline sidebar/toolbar-toggle exist at all.
/// Parses with the SAME options as [`render`] so it agrees with what the render will list.
pub(super) fn has_headings(md: &str) -> bool {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    Parser::new_ext(md, opts).any(|ev| match ev {
        Event::Start(Tag::Heading { .. }) => true,
        Event::Html(s) | Event::InlineHtml(s) => html_has_heading(&s),
        _ => false,
    })
}

/// Cheap scan for `<h1`..`<h6` (case-insensitive) in a raw-HTML fragment.
fn html_has_heading(s: &str) -> bool {
    let b = s.as_bytes();
    b.windows(3).any(|w| {
        w[0] == b'<' && (w[1] | 0x20) == b'h' && (b'1'..=b'6').contains(&w[2])
    })
}

/// Flatten a run list to its plain text (for the outline label).
fn runs_text(runs: &[Run]) -> String {
    let mut s = String::new();
    for r in runs {
        s.push_str(&r.text);
    }
    s.trim().to_string()
}

/// One laid-out block. Inline runs carry the styling; code blocks stay plain monospace text.
/// The `bool` on Heading/Para is "center this block" (from an enclosing `align="center"`).
pub(super) enum Block {
    Heading(u8, Vec<Run>, bool),
    Para(Vec<Run>, bool),
    Code(String, highlight::Lang),
    /// (indent depth, bullet/number marker, runs)
    Item(u8, String, Vec<Run>),
    Quote(Vec<Run>),
    Rule,
    /// GFM or raw-HTML table: header cells + body rows + per-column alignment (0 left,
    /// 1 center, 2 right).
    Table {
        header: Vec<Vec<Run>>,
        rows: Vec<Vec<Vec<Run>>>,
        aligns: Vec<u8>,
    },
    Image(ImgBlock),
}

/// Per-paint layout cache for the Markdown pane: the measured heights (device px, trailing spacing
/// included) of the expensive text blocks (headings/paragraphs/list-items/quotes), plus the
/// document's rendered text and where each run landed in it. Lets a repeat paint while scrolling
/// SKIP re-measuring the off-screen paragraphs and rebuilding the text instead of re-laying-out the
/// whole document every frame — the difference between smooth and stuttering on a big Markdown
/// file. Keyed by (decode gen, pane width, remote-images flag); rebuilt when any changes. Only the
/// text blocks' heights are cached; code/tables/images are cheap-to-measure or async, so they
/// always re-run.
#[derive(Default)]
pub(super) struct MdLayout {
    ready: bool,
    key: (u64, i32, bool),
    heights: Vec<i32>, // per block index; -1 = unmeasured
    /// The RENDERED text of the whole document — the coordinate space every selection offset
    /// lives in (see [`super::selection`]). Complete regardless of what's painted/culled, so
    /// Ctrl+A and copy cover the whole file. Depends only on the parse, so a scroll never
    /// invalidates it and offsets stay stable across paints.
    pub(super) doc: String,
    /// Where each block's runs landed in `doc` — parallel to the block list.
    bases: Vec<DocBase>,
}

/// The `doc` byte offsets of one block's selectable pieces (shape follows the block's).
enum DocBase {
    /// Heading / paragraph / list item / quote: one offset per run.
    Runs(Vec<usize>),
    /// Code block: the offset of its text.
    Code(usize),
    /// Table: `[row][cell][run]`, header row first when there is one (matches the draw order).
    Table(Vec<Vec<Vec<usize>>>),
    /// Nothing selectable (rules, images).
    None,
}

/// Append `block`'s text to the selection document (in reading order) and report where its runs
/// landed. Runs are the only hit-testable pieces: a list marker is copied (it's part of the line)
/// but never individually selectable — browsers don't highlight bullets either.
fn doc_append(doc: &mut String, block: &Block) -> DocBase {
    fn runs(doc: &mut String, runs: &[Run]) -> Vec<usize> {
        let mut v = Vec::with_capacity(runs.len());
        for r in runs {
            v.push(doc.len());
            doc.push_str(&r.text);
        }
        doc.push('\n');
        v
    }
    match block {
        Block::Heading(_, rs, _) | Block::Para(rs, _) | Block::Quote(rs) => DocBase::Runs(runs(doc, rs)),
        Block::Item(_, marker, rs) => {
            doc.push_str(marker);
            doc.push(' ');
            DocBase::Runs(runs(doc, rs))
        }
        Block::Code(text, _) => {
            let b = doc.len();
            doc.push_str(text);
            doc.push('\n');
            DocBase::Code(b)
        }
        Block::Table { header, rows, .. } => {
            let mut all: Vec<&[Vec<Run>]> = Vec::with_capacity(rows.len() + 1);
            if !header.is_empty() {
                all.push(header.as_slice());
            }
            all.extend(rows.iter().map(|r| r.as_slice()));
            let mut out = Vec::with_capacity(all.len());
            for row in all {
                let mut rb = Vec::with_capacity(row.len());
                for (ci, cell) in row.iter().enumerate() {
                    if ci > 0 {
                        doc.push('\t'); // tab-separated: pastes into a spreadsheet as columns
                    }
                    let mut cb = Vec::with_capacity(cell.len());
                    for r in cell {
                        cb.push(doc.len());
                        doc.push_str(&r.text);
                    }
                    rb.push(cb);
                }
                doc.push('\n');
                out.push(rb);
            }
            DocBase::Table(out)
        }
        Block::Rule | Block::Image(_) => DocBase::None,
    }
}

// GitHub-ish metrics (CSS px @96dpi, DPI-scaled at draw): 16px body, 980px-45px*2 ≈ 880 content
// column, 6x13 table cell padding, 4px quote bar. Headings 2em/1.5em/1.25em/1em/0.875em/0.85em.
const BODY_PX: i32 = 16;
const MAX_COL_W: i32 = 880;
fn heading_px(level: u8) -> i32 {
    match level {
        1 => 32,
        2 => 24,
        3 => 20,
        4 => 16,
        5 => 14,
        _ => 13,
    }
}

/// Render `md` into `rc`, scrolled by `scroll` device px. Returns the total content height
/// (device px) so the caller can clamp scrolling. Fills `rc` with the bg first. `doc_dir` is
/// the markdown file's folder (local image srcs resolve against it); `imgs` is the per-document
/// decoded-image cache (owned by the viewer state, cleared on load).
#[allow(clippy::too_many_arguments)] // GDI layout pass: hdc + geometry + out-collectors, no struct gain
pub(super) unsafe fn render(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    md: &str,
    scroll: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    toc: &mut Vec<TocEntry>,
    imgs: &mut ImgCache,
    doc_dir: Option<&Path>,
    gen: u64,
    remote_ok: bool,
    layout: &mut MdLayout,
    sel: &mut MdSel,
) -> i32 {
    links.clear();
    toc.clear();
    sel.hits.clear();
    let brush = CreateSolidBrush(COLORREF(c.bg));
    FillRect(hdc, rc, brush);
    let _ = DeleteObject(brush.into());
    SetBkMode(hdc, TRANSPARENT);

    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let margin = sc(18);
    // GitHub-style content column: capped width, centered in the pane.
    let avail = (rc.right - rc.left - 2 * margin).max(1);
    let full_w = avail.min(sc(MAX_COL_W));
    let x0 = rc.left + margin + (avail - full_w) / 2;
    let top = rc.top + margin;
    let mut y = top - scroll;
    let mut first = true;

    // Layout cache: reuse the measured text-block heights unless the document, the pane width, or
    // the remote-images flag changed. On a repeat paint (scrolling) this lets us skip re-measuring
    // every off-screen paragraph — the expensive part — instead of re-laying-out the whole file.
    // Key on the CLAMPED wrap width (full_w), not the raw pane width: the ToC-sidebar slide
    // animation shrinks the pane a few px per 15ms tick, but full_w stays capped at MAX_COL_W, so
    // the cache survives the slide instead of fully rebuilding every animation frame.
    let key = (gen, full_w, remote_ok);
    let blocks = parse_blocks(md, remote_ok);
    if !layout.ready || layout.key != key || layout.heights.len() != blocks.len() {
        layout.heights = vec![-1; blocks.len()];
        let mut doc = String::new();
        let bases = blocks.iter().map(|b| doc_append(&mut doc, b)).collect();
        layout.doc = doc;
        layout.bases = bases;
        layout.key = key;
        layout.ready = true;
    }
    let bench_t = std::env::var_os("ST2K_MD_BENCH").is_some().then(std::time::Instant::now);

    for (bi, block) in blocks.iter().enumerate() {
        // Outline entry for every heading, BEFORE any skip, so the ToC stays complete even when the
        // heading is culled off-screen. `+pre` matches the in-arm pre-margin so click targets align.
        if let Block::Heading(lvl, runs, _) = block {
            let pre = if first { 0 } else { sc(8) };
            toc.push(TocEntry { level: *lvl, text: runs_text(runs), target: (y + pre - top + scroll).max(0) });
        }
        // Fast-path: a text block we've already measured that's fully off-screen — skip the
        // run_block re-measure entirely and just advance by the cached height.
        let is_text = matches!(block, Block::Heading(..) | Block::Para(..) | Block::Item(..) | Block::Quote(..));
        if is_text {
            let h = layout.heights.get(bi).copied().unwrap_or(-1);
            if h >= 0 && (y + h <= rc.top || y >= rc.bottom) {
                y += h;
                first = false;
                continue;
            }
        }
        // The block's run offsets in the selection document (empty for the dry/unselectable ones).
        let run_bases: &[usize] = match layout.bases.get(bi) {
            Some(DocBase::Runs(v)) => v,
            _ => &[],
        };
        let mut rsel = RunSel { range: sel.range, doc: &layout.doc, bases: run_bases, hits: &mut *sel.hits, bg: c.sel };
        let y_block_start = y;
        match block {
            Block::Heading(level, runs, center) => {
                if !first {
                    y += sc(8); // extra top margin before a heading (GitHub 24px total)
                }
                let px = heading_px(*level);
                let fonts = Fonts::new(hwnd, px, true, false);
                let ctx = ctx_for(hwnd, c, c.fg);
                let (ny, _) = run_block(hdc, runs, &fonts, x0, y, full_w, if *center { 1 } else { 0 }, y >= rc.bottom, &ctx, links, Some(&mut rsel));
                fonts.free();
                y = ny;
                if *level <= 2 {
                    // GitHub-style hairline under h1/h2.
                    hline(hdc, x0, x0 + full_w, y + sc(4), c.border);
                    y += sc(8);
                }
                y += sc(10);
            }
            Block::Para(runs, center) => {
                let fonts = Fonts::new(hwnd, BODY_PX, false, false);
                let ctx = ctx_for(hwnd, c, c.fg);
                let (ny, _) = run_block(hdc, runs, &fonts, x0, y, full_w, if *center { 1 } else { 0 }, y >= rc.bottom, &ctx, links, Some(&mut rsel));
                fonts.free();
                if ny > y {
                    y = ny + sc(14);
                }
            }
            Block::Code(text, lang) => {
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
                    let cb = CreateSolidBrush(COLORREF(c.code_bg));
                    let cp = CreatePen(PS_SOLID, 1, COLORREF(c.code_bg));
                    let ob = SelectObject(hdc, cb.into());
                    let op = SelectObject(hdc, HGDIOBJ(cp.0));
                    let r6 = sc(6);
                    let _ = RoundRect(hdc, x0, y, x0 + full_w, y + h, r6, r6);
                    SelectObject(hdc, ob);
                    SelectObject(hdc, op);
                    let _ = DeleteObject(cb.into());
                    let _ = DeleteObject(HGDIOBJ(cp.0));
                    // The code text is its own slice of the selection document: translate the
                    // range into it (a selection reaching past either end just clamps, which is
                    // exactly the "selection continues outside this block" case).
                    let base = match layout.bases.get(bi) {
                        Some(DocBase::Code(b)) => *b,
                        _ => 0,
                    };
                    let local = sel.range.map(|(s, e)| (s.saturating_sub(base), e.saturating_sub(base)));
                    let mut ls = highlight::LineSel {
                        hits: &mut *sel.hits,
                        base,
                        spec: FontSpec { px: 13, bold: false, italic: false, mono: true },
                    };
                    highlight::paint_lines(hdc, text, *lang, x0 + pad, y + pad, full_w - 2 * pad, rc.top, rc.bottom, f, c.fg, local, Some(&mut ls));
                }
                let _ = DeleteObject(f.into());
                y += h + sc(14);
            }
            Block::Item(depth, marker, runs) => {
                let indent = sc(22) * (*depth as i32 + 1);
                // marker (bullet / number) in the muted colour
                let mf = font(hwnd, BODY_PX, false, false, false);
                draw_at(hdc, marker, x0 + indent - sc(18), y, mf, c.muted);
                let _ = DeleteObject(mf.into());
                let fonts = Fonts::new(hwnd, BODY_PX, false, false);
                let ctx = ctx_for(hwnd, c, c.fg);
                let (ny, _) = run_block(hdc, runs, &fonts, x0 + indent, y, full_w - indent, 0, y >= rc.bottom, &ctx, links, Some(&mut rsel));
                fonts.free();
                y = ny + sc(4);
            }
            Block::Quote(runs) => {
                let indent = sc(16);
                let y_start = y;
                let fonts = Fonts::new(hwnd, BODY_PX, false, true);
                let ctx = ctx_for(hwnd, c, c.muted);
                let (ny, _) = run_block(hdc, runs, &fonts, x0 + indent, y, full_w - indent, 0, y >= rc.bottom, &ctx, links, Some(&mut rsel));
                fonts.free();
                y = ny;
                // GitHub-style gray quote bar spanning the quote's height.
                let pen = CreatePen(PS_SOLID, sc(4), COLORREF(c.border));
                let op = SelectObject(hdc, HGDIOBJ(pen.0));
                let _ = MoveToEx(hdc, x0 + sc(2), y_start, None);
                let _ = LineTo(hdc, x0 + sc(2), y);
                SelectObject(hdc, op);
                let _ = DeleteObject(HGDIOBJ(pen.0));
                y += sc(14);
            }
            Block::Rule => {
                // GitHub hr: a short solid bar, not a hairline.
                let bar = RECT { left: x0, top: y + sc(8), right: x0 + full_w, bottom: y + sc(8) + sc(3) };
                let hb = CreateSolidBrush(COLORREF(c.border));
                FillRect(hdc, &bar, hb);
                let _ = DeleteObject(hb.into());
                y += sc(26);
            }
            Block::Table { header, rows, aligns } => {
                let tbases: &[Vec<Vec<usize>>] = match layout.bases.get(bi) {
                    Some(DocBase::Table(v)) => v,
                    _ => &[],
                };
                let mut tsel = TblSel { range: sel.range, doc: &layout.doc, bases: tbases, hits: &mut *sel.hits, bg: c.sel };
                y = draw_table(hwnd, hdc, header, rows, aligns, x0, y, full_w, c, links, &mut tsel);
                y += sc(14);
            }
            Block::Image(ib) => {
                y = draw_image(hwnd, hdc, rc, ib, x0, y, full_w, c, links, imgs, doc_dir, gen);
            }
        }
        // Cache the text block's just-measured height (spacing included) for the skip fast-path.
        if is_text {
            if let Some(slot) = layout.heights.get_mut(bi) {
                *slot = y - y_block_start;
            }
        }
        first = false;
    }
    if let Some(t0) = bench_t {
        eprintln!("[md-bench] {} blocks, scroll {}px: {:?}", layout.heights.len(), scroll, t0.elapsed());
    }
    y + scroll - top + margin // total content height
}

/// Draw one GFM/HTML table GitHub-style: full 1px grid, bold header, zebra body rows,
/// per-column alignment, auto column widths (natural, proportionally shrunk to fit).
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
unsafe fn draw_table(
    hwnd: HWND,
    hdc: HDC,
    header: &[Vec<Run>],
    rows: &[Vec<Vec<Run>>],
    aligns: &[u8],
    x0: i32,
    y0: i32,
    avail: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    sel: &mut TblSel,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let ncols = header
        .len()
        .max(rows.iter().map(|r| r.len()).max().unwrap_or(0))
        .max(1);
    let hpad = sc(13);
    let vpad = sc(6);
    let fonts = Fonts::new(hwnd, BODY_PX, false, false);
    let hfonts = Fonts::new(hwnd, BODY_PX, true, false);
    let ctx = ctx_for(hwnd, c, c.fg);
    let col_align = |ci: usize| aligns.get(ci).copied().unwrap_or(0);

    // Every row to draw, in order, with its "is the header row" flag (HTML tables may have none).
    let all: Vec<(&[Vec<Run>], bool)> = {
        let mut v: Vec<(&[Vec<Run>], bool)> = Vec::with_capacity(rows.len() + 1);
        if !header.is_empty() {
            v.push((header, true));
        }
        v.extend(rows.iter().map(|r| (r.as_slice(), false)));
        v
    };

    // Pass 1: per-column natural (unwrapped) and minimum (widest single word) widths — the
    // width=1 dry pass forces a wrap at every word, so its widest line IS the widest word.
    let mut nat = vec![sc(24); ncols];
    let mut minw = vec![sc(24); ncols];
    let mut scratch: Vec<LinkHit> = Vec::new();
    for (row, is_hdr) in &all {
        let f = if *is_hdr { &hfonts } else { &fonts };
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            let (_, w) = run_block(hdc, cell, f, 0, 0, i32::MAX / 4, 0, true, &ctx, &mut scratch, None);
            nat[ci] = nat[ci].max(w + 2 * hpad);
            let (_, mw) = run_block(hdc, cell, f, 0, 0, 1, 0, true, &ctx, &mut scratch, None);
            minw[ci] = minw[ci].max(mw + 2 * hpad);
        }
    }
    // Browser-style auto layout: everything at natural width if it fits; otherwise every column
    // keeps at least its min-content width and the slack is distributed in proportion to how
    // much each column WANTS to grow (nat - min). Only when even the minimums overflow do we
    // shrink below min (proportionally, clipped at the pane edge).
    let sum_nat: i64 = nat.iter().map(|w| *w as i64).sum();
    let sum_min: i64 = minw.iter().map(|w| *w as i64).sum();
    let colw: Vec<i32> = if sum_nat <= avail as i64 {
        nat
    } else if sum_min >= avail as i64 {
        minw.iter()
            .map(|w| ((*w as i64 * avail as i64 / sum_min.max(1)) as i32).max(sc(40)))
            .collect()
    } else {
        let slack = avail as i64 - sum_min;
        let want: i64 = sum_nat - sum_min;
        nat.iter()
            .zip(&minw)
            .map(|(n, m)| (*m as i64 + (*n - *m) as i64 * slack / want.max(1)) as i32)
            .collect()
    };
    let table_w: i32 = colw.iter().sum::<i32>().min(avail);
    let cell_x = |ci: usize| x0 + colw[..ci].iter().sum::<i32>();

    // Pass 2: row heights (wrap each cell at its column width).
    let line_h_probe = {
        let old = SelectObject(hdc, fonts.reg.into());
        let mut tm = TEXTMETRICW::default();
        let _ = GetTextMetricsW(hdc, &mut tm);
        SelectObject(hdc, old);
        (tm.tmHeight + tm.tmExternalLeading + sc(3)).max(1)
    };
    let mut row_h: Vec<i32> = Vec::new();
    for (row, is_hdr) in &all {
        let f = if *is_hdr { &hfonts } else { &fonts };
        let mut h = line_h_probe;
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            let w = (colw[ci] - 2 * hpad).max(sc(24));
            let (ny, _) = run_block(hdc, cell, f, 0, 0, w, 0, true, &ctx, &mut scratch, None);
            h = h.max(ny);
        }
        row_h.push(h + 2 * vpad);
    }

    // Pass 3: draw. Zebra fill first, then text, then the grid on top.
    let mut y = y0;
    let mut body_i = 0usize;
    for (ri, (row, is_hdr)) in all.iter().enumerate() {
        let f = if *is_hdr { &hfonts } else { &fonts };
        let h = row_h[ri];
        if !is_hdr {
            // GitHub zebra: every 2nd body row gets the subtle fill.
            if body_i % 2 == 1 {
                let zr = RECT { left: x0, top: y, right: x0 + table_w, bottom: y + h };
                let zb = CreateSolidBrush(COLORREF(c.code_bg));
                FillRect(hdc, &zr, zb);
                let _ = DeleteObject(zb.into());
            }
            body_i += 1;
        }
        for (ci, cell) in row.iter().enumerate().take(ncols) {
            let w = (colw[ci] - 2 * hpad).max(sc(24));
            let mut rsel = RunSel {
                range: sel.range,
                doc: sel.doc,
                bases: sel.bases.get(ri).and_then(|r| r.get(ci)).map(|v| v.as_slice()).unwrap_or(&[]),
                hits: &mut *sel.hits,
                bg: sel.bg,
            };
            let _ = run_block(hdc, cell, f, cell_x(ci) + hpad, y + vpad, w, col_align(ci), false, &ctx, links, Some(&mut rsel));
        }
        y += h;
        hline(hdc, x0, x0 + table_w, y, c.border); // row separator
    }
    // top edge + verticals
    hline(hdc, x0, x0 + table_w, y0, c.border);
    for ci in 0..=ncols {
        let x = if ci == ncols { x0 + table_w } else { cell_x(ci) };
        let pen = CreatePen(PS_SOLID, 1, COLORREF(c.border));
        let op = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = MoveToEx(hdc, x, y0, None);
        let _ = LineTo(hdc, x, y);
        SelectObject(hdc, op);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
    fonts.free();
    hfonts.free();
    y
}

/// Draw one image block: local src -> decoded DIB (cached per document, synchronous — the
/// extension gate keeps it on the fast pure-Rust tiers); remote src (opt-in toggle) -> async
/// fetch worker, pill until the posted result lands; failed/blocked -> alt-text pill. Returns
/// the y after the block.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
unsafe fn draw_image(
    hwnd: HWND,
    hdc: HDC,
    clip: &RECT,
    ib: &ImgBlock,
    x0: i32,
    y: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    imgs: &mut ImgCache,
    doc_dir: Option<&Path>,
    gen: u64,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    const MAX_IMAGES: usize = 24; // bound decode/fetch work per document
    if !imgs.contains_key(&ib.src) {
        if imgs.len() >= MAX_IMAGES {
            return pill_fallback(hwnd, hdc, ib, x0, y, full_w, c, links);
        }
        if is_remote_src(&ib.src) {
            // Only reachable when the remote-images toggle is ON (Builder pills them
            // otherwise). Fetch + decode OFF the paint thread; repaint installs the result.
            imgs.insert(ib.src.clone(), ImgSlot::Pending);
            super::content::spawn_md_img(hwnd, ib.src.clone(), gen);
        } else {
            let slot = match load_img(&ib.src, doc_dir, c.bg) {
                Some(rd) => ImgSlot::Ready(rd),
                None => ImgSlot::Failed,
            };
            imgs.insert(ib.src.clone(), slot);
        }
    }
    let Some(ImgSlot::Ready(rd)) = imgs.get(&ib.src) else {
        return pill_fallback(hwnd, hdc, ib, x0, y, full_w, c, links);
    };
    let mut dw = match ib.width {
        ImgW::Natural => sc(rd.iw),
        ImgW::Px(p) => sc(p),
        ImgW::Pct(p) => (full_w as i64 * (p.min(100)) as i64 / 100) as i32,
    };
    dw = dw.clamp(1, full_w);
    let dh = ((dw as i64 * rd.ih as i64) / rd.iw.max(1) as i64).max(1) as i32;
    let x = if ib.center { x0 + (full_w - dw) / 2 } else { x0 };
    // Blit only when the destination intersects the pane (layout still advances offscreen).
    if y + dh >= clip.top && y <= clip.bottom {
        let memdc = CreateCompatibleDC(Some(hdc));
        let old = SelectObject(memdc, rd.hbmp.into());
        SetStretchBltMode(hdc, HALFTONE);
        let _ = StretchBlt(hdc, x, y, dw, dh, Some(memdc), 0, 0, rd.iw, rd.ih, SRCCOPY);
        SelectObject(memdc, old);
        let _ = DeleteDC(memdc);
    }
    if let Some(url) = &ib.link {
        links.push(LinkHit {
            rect: RECT { left: x, top: y, right: x + dw, bottom: y + dh },
            url: url.clone(),
        });
    }
    y + dh + sc(12)
}

/// Alt-text pill for an image we won't/can't decode (remote, failed, over caps).
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
unsafe fn pill_fallback(
    hwnd: HWND,
    hdc: HDC,
    ib: &ImgBlock,
    x0: i32,
    y: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let label = if ib.alt.trim().is_empty() { "image" } else { ib.alt.trim() };
    let label = label.replace(' ', "\u{00A0}"); // one unbroken pill token
    let runs = [Run {
        text: format!("\u{00A0}{label}\u{00A0}"),
        bold: false,
        italic: false,
        code: true,
        strike: false,
        link: ib.link.clone(),
    }];
    let fonts = Fonts::new(hwnd, 13, false, false);
    let ctx = ctx_for(hwnd, c, c.muted);
    // Synthesized label, not part of the document — no selection wiring.
    let (ny, _) = run_block(hdc, &runs, &fonts, x0, y, full_w, if ib.center { 1 } else { 0 }, false, &ctx, links, None);
    fonts.free();
    ny + sc(8)
}

/// Resolve a (non-remote) image src against the document folder, percent-decoding and dropping
/// any `?query`/`#fragment` suffix.
fn resolve_src(src: &str, dir: Option<&Path>) -> Option<PathBuf> {
    let s = src.split(['?', '#']).next().unwrap_or("");
    if s.is_empty() {
        return None;
    }
    let s = percent_decode(s);
    let s = s.strip_prefix("./").unwrap_or(&s);
    let p = Path::new(s);
    if p.is_absolute() {
        Some(p.to_path_buf())
    } else {
        dir.map(|d| d.join(p))
    }
}

/// Minimal %XX decoder (image paths with spaces). Byte-wise throughout — a `&str` slice here
/// would panic (=abort) on a multibyte char straddling the %XX window (e.g. `"%é"`).
fn percent_decode(s: &str) -> String {
    if !s.contains('%') {
        return s.to_string();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Decode a LOCAL image for inline display: known-fast formats only (the pure-Rust/resvg tiers —
/// never WIC/magick, this runs on the paint path), bounded size, downscaled to a display cap,
/// composited over the pane bg into a DIB. Returns `None` (-> pill) on any miss.
unsafe fn load_img(src: &str, dir: Option<&Path>, bg: u32) -> Option<RenderData> {
    // Remote is NEVER fetched (privacy). This includes UNC paths (`\\server\…` / `//server/…`):
    // fs::read on one opens an SMB connection to an attacker-named host — an outbound network
    // hit (and NTLM handshake) triggered by merely previewing a hostile README.
    if src.starts_with("\\\\") || src.starts_with("//") || src.contains("://") || src.starts_with("data:") {
        return None;
    }
    // Notebook `attachment:` refs are served from the pre-seeded cache, never the filesystem —
    // reject the scheme so a decode-miss can't try `<dir>/…attachment:name` (an NTFS alternate
    // data stream on Windows).
    if src.contains("attachment:") {
        return None;
    }
    let path = resolve_src(src, dir)?;
    if path.as_os_str().to_string_lossy().starts_with("\\\\") {
        return None; // a relative src must not join into a UNC target either
    }
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(
        ext.as_str(),
        "png" | "jpg" | "jpeg" | "jfif" | "gif" | "webp" | "bmp" | "svg" | "svgz" | "ico" | "apng"
    ) {
        return None;
    }
    let meta = std::fs::metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > 32 * 1024 * 1024 {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    decode_bytes_to_dib(&bytes, bg)
}

/// Decode already-in-memory image bytes (a notebook attachment / a fetched remote image) to a
/// display-capped DIB composited over `bg`. Shared by the local-file, remote-fetch, and
/// notebook-attachment paths. `None` on any decode/alloc failure.
pub(super) unsafe fn decode_bytes_to_dib(bytes: &[u8], bg: u32) -> Option<RenderData> {
    let img = sagethumbs2k_core::decode::decode_preview(bytes).ok()?;
    // Bound the cached DIB (README art displays ≤ content width; 2048 keeps HiDPI crisp).
    let img = if img.width() > 2048 || img.height() > 4096 {
        img.thumbnail(2048, 4096)
    } else {
        img
    };
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width() as i32, rgba.height() as i32);
    let hbmp = super::content::make_dib(w, h, rgba.as_raw(), bg)?;
    Some(RenderData { hbmp, iw: w, ih: h })
}

// ---- inline run layout -------------------------------------------------------------------

/// The five font variants a block draws with, created once and freed together.
struct Fonts {
    reg: HFONT,
    bold: HFONT,
    ital: HFONT,
    bi: HFONT,
    mono: HFONT,
    px: i32,
    base_bold: bool,
    base_italic: bool,
}

impl Fonts {
    unsafe fn new(hwnd: HWND, px: i32, base_bold: bool, base_italic: bool) -> Fonts {
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
    fn pick(&self, r: &Run) -> HFONT {
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
    fn spec(&self, r: &Run) -> FontSpec {
        if r.code {
            return FontSpec { px: self.px - 1, bold: false, italic: false, mono: true };
        }
        FontSpec {
            px: self.px,
            bold: self.base_bold || r.bold,
            italic: self.base_italic || r.italic,
            mono: false,
        }
    }
    unsafe fn free(self) {
        for f in [self.reg, self.bold, self.ital, self.bi, self.mono] {
            let _ = DeleteObject(f.into());
        }
    }
}

/// Palette + DPI-scaled constants shared by every `run_block` call of one render pass.
struct RunCtx {
    code_bg: u32,
    accent: u32,
    base_color: u32,
    code_pad: i32,
    line_lead: i32,
    ul_off: i32,
}

fn ctx_for(hwnd: HWND, c: &MdColors, base_color: u32) -> RunCtx {
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
enum Tok {
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
struct RunSel<'a> {
    range: Option<(usize, usize)>,
    doc: &'a str,
    bases: &'a [usize],
    hits: &'a mut Vec<SelHit>,
    bg: u32,
}

/// Selection wiring for one [`draw_table`] call (per-cell [`RunSel`]s are built from it).
struct TblSel<'a> {
    range: Option<(usize, usize)>,
    doc: &'a str,
    bases: &'a [Vec<Vec<usize>>],
    hits: &'a mut Vec<SelHit>,
    bg: u32,
}

/// Word-wrap + draw a block's inline `runs` starting at `(x0, y)` within `width`.
/// `align`: 0 left, 1 center, 2 right (per-line offset). `dry` measures without drawing
/// (no GDI output, no link/selection collection). Returns `(y_after, widest_line)`.
#[allow(clippy::too_many_arguments)] // GDI layout core: hdc + geometry + mode flags, no struct gain
unsafe fn run_block(
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
    mut sel: Option<&mut RunSel>,
) -> (i32, i32) {
    if runs.iter().all(|r| r.text.trim().is_empty()) {
        return (y, 0);
    }
    // Line height from the regular font's metrics + a little leading.
    let old_font = SelectObject(hdc, fonts.reg.into());
    let mut tm = TEXTMETRICW::default();
    let _ = GetTextMetricsW(hdc, &mut tm);
    let line_h = tm.tmHeight + tm.tmExternalLeading + ctx.line_lead;

    // Flatten runs -> measured tokens (words / spaces / hard breaks), each remembering the run
    // bytes it came from so it maps back to the selection document.
    let mut toks: Vec<Tok> = Vec::new();
    for (ri, r) in runs.iter().enumerate() {
        let f = fonts.pick(r);
        let spec = fonts.spec(r);
        let color = if r.link.is_some() { ctx.accent } else { ctx.base_color };
        let pad = if r.code { ctx.code_pad } else { 0 };
        SelectObject(hdc, f.into());
        let base = sel.as_ref().and_then(|s| s.bases.get(ri).copied());
        let mut word: Vec<u16> = Vec::new();
        let mut wstart = 0usize; // byte offset in `r.text` where the pending word began
        macro_rules! flush_word {
            ($wend:expr) => {
                if !word.is_empty() {
                    let mut sz = SIZE::default();
                    let _ = GetTextExtentPoint32W(hdc, &word, &mut sz);
                    toks.push(Tok::Word {
                        s: core::mem::take(&mut word),
                        w: sz.cx + 2 * pad,
                        pad,
                        font: f,
                        color,
                        code: r.code,
                        strike: r.strike,
                        link: r.link.clone(),
                        doc: base.map(|b| (b + wstart, b + $wend)),
                        spec,
                    });
                }
            };
        }
        for (ci, ch) in r.text.char_indices() {
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
                    }
                }
            }
        }
        flush_word!(r.text.len());
    }

    // Break into lines (greedy), remembering each placed word's line-relative x.
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
    if lines.is_empty() {
        SelectObject(hdc, old_font);
        return (y, 0);
    }
    let max_w = lines.iter().map(|(_, w)| *w).max().unwrap_or(0);

    // Copied out so the draw loop can read the selection while `sel` is mutably reborrowed for
    // the per-line fill.
    let (sel_rng, sel_bg) = match sel.as_ref() {
        Some(s) => (s.range, s.bg),
        None => (None, 0),
    };
    if !dry {
        for (li, (placed, lw)) in lines.iter().enumerate() {
            let xoff = match align {
                1 => (width - lw).max(0) / 2,
                2 => (width - lw).max(0),
                _ => 0,
            };
            let cy = y + li as i32 * line_h;
            // Selection fill + hit rects BEFORE the glyphs — an opaque fill after would erase them.
            if let Some(s) = sel.as_deref_mut() {
                line_sel(hdc, &toks, placed, x0 + xoff, cy, line_h, s);
            }
            for (rx, idx) in placed {
                let Tok::Word { s, w, pad, font, color, code, strike, link, doc, .. } = &toks[*idx] else {
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
                    let r = RECT { left: cx, top: cy, right: cx + *w, bottom: cy + line_h };
                    SetBkColor(hdc, COLORREF(if hot { sel_bg } else { ctx.code_bg }));
                    SetBkMode(hdc, OPAQUE);
                    let _ = ExtTextOutW(hdc, cx + *pad, cy, ETO_OPAQUE, Some(&r as *const RECT), PCWSTR(s.as_ptr()), s.len() as u32, None);
                    SetBkMode(hdc, TRANSPARENT);
                } else {
                    let _ = ExtTextOutW(hdc, cx, cy, ETO_OPTIONS(0), None, PCWSTR(s.as_ptr()), s.len() as u32, None);
                }
                if *strike {
                    hline(hdc, cx + *pad, cx + *w - *pad, cy + line_h / 2, *color);
                }
                if let Some(url) = link {
                    hline(hdc, cx + *pad, cx + *w - *pad, cy + line_h - ctx.ul_off, *color);
                    links.push(LinkHit {
                        rect: RECT { left: cx, top: cy, right: cx + *w, bottom: cy + line_h },
                        url: url.clone(),
                    });
                }
            }
        }
    }
    SelectObject(hdc, old_font);
    (y + lines.len() as i32 * line_h, max_w)
}

/// Fill the selection background behind one laid-out line's selected words (and the spaces
/// between them), and record every word's hit rect. Runs before the line's glyphs are drawn.
unsafe fn line_sel(
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
        let Tok::Word { w, pad, font, doc, spec, code, .. } = &toks[*idx] else { continue };
        let Some((ds, de)) = *doc else {
            prev = None;
            continue;
        };
        let cx = xbase + rx;
        sel.hits.push(SelHit {
            rect: RECT { left: cx, top: cy, right: cx + *w, bottom: cy + line_h },
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
                    (x + highlight::disp_extent(hdc, t, a), x + highlight::disp_extent(hdc, t, b))
                };
                fill(hdc, x1, cy, x2, cy + line_h, sel.bg);
            }
        }
        prev = Some((de, cx + *w));
    }
}

/// Fill a rect with a solid colour.
unsafe fn fill(hdc: HDC, x1: i32, y1: i32, x2: i32, y2: i32, color: u32) {
    if x2 <= x1 {
        return;
    }
    let r = RECT { left: x1, top: y1, right: x2, bottom: y2 };
    let b = CreateSolidBrush(COLORREF(color));
    FillRect(hdc, &r, b);
    let _ = DeleteObject(b.into());
}

/// A 1px horizontal line (strike / underline / grid) in `color`.
unsafe fn hline(hdc: HDC, x1: i32, x2: i32, y: i32, color: u32) {
    let pen = CreatePen(PS_SOLID, 1, COLORREF(color));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, x1, y, None);
    let _ = LineTo(hdc, x2, y);
    SelectObject(hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

/// Draw a short single-line string at `(x, y)` (list markers).
unsafe fn draw_at(hdc: HDC, text: &str, x: i32, y: i32, font: HFONT, color: u32) {
    let old = SelectObject(hdc, font.into());
    SetTextColor(hdc, COLORREF(color));
    let mut w: Vec<u16> = text.encode_utf16().collect();
    let mut r = RECT { left: x, top: y, right: x + 400, bottom: y + 100 };
    DrawTextW(hdc, &mut w, &mut r, DT_LEFT | DT_TOP | DT_NOPREFIX);
    SelectObject(hdc, old);
}

/// Re-create the font a drawn token was measured with (hit-testing; caller frees it).
pub(super) unsafe fn font_for(hwnd: HWND, s: FontSpec) -> HFONT {
    font(hwnd, s.px, s.bold, s.italic, s.mono)
}

/// Create a font: `px` @96dpi (DPI-scaled), Segoe UI (or Consolas if `mono`), bold/italic.
unsafe fn font(hwnd: HWND, px: i32, bold: bool, italic: bool, mono: bool) -> HFONT {
    let h = crate::win::dpi_scale(hwnd, px);
    let face = crate::win::wide(if mono { "Consolas" } else { "Segoe UI" });
    CreateFontW(
        -h, 0, 0, 0,
        if bold { 700 } else { 400 },
        u32::from(italic),
        0, 0,
        DEFAULT_CHARSET,
        OUT_DEFAULT_PRECIS,
        CLIP_DEFAULT_PRECIS,
        DEFAULT_QUALITY,
        Default::default(),
        PCWSTR(face.as_ptr()),
    )
}

// ---- markdown -> blocks ------------------------------------------------------------------

/// Shared block-builder state driven by BOTH the pulldown-cmark event loop and the raw-HTML
/// feeder in [`super::mdhtml`]. Raw HTML toggles the same inline-style counters and emits the
/// same [`Block`]s, so `<b>`/`<h1>`/`<img>`/`<table>` render identically to their markdown twins.
pub(super) struct Builder {
    pub(super) out: Vec<Block>,
    runs: Vec<Run>,
    heading: Option<u8>,
    in_para: bool,
    in_quote: u32,
    in_item: bool,
    lists: Vec<(bool, u64)>,
    strong: u32,
    emph: u32,
    strike: u32,
    code_html: u32, // raw-HTML <code>/<kbd> nesting
    link: Option<String>,
    // markdown table state
    in_cell: bool,
    cur_cell: Vec<Run>,
    cur_row: Vec<Vec<Run>>,
    tbl_header: Vec<Vec<Run>>,
    tbl_rows: Vec<Vec<Vec<Run>>>,
    tbl_aligns: Vec<u8>,
    // markdown image capture (alt text arrives as Text events between Start/End)
    img: Option<(String, String)>, // (dest url, alt buffer)
    // raw-HTML state (owned here so it persists across separate HtmlBlock events — a
    // `<div align="center">` opener and its `</div>` arrive in DIFFERENT blocks)
    center: u32,
    html_stack: Vec<(String, bool)>, // (open container tag, contributed-center)
    html_buf: String,
    pub(super) skip_tag: Option<&'static str>, // inside <style>/<script>: skip until close
    pub(super) in_comment: bool,               // inside <!-- ... -->
    h_tbl: Option<HtmlTbl>,
    /// The remote-images toggle: when true, http(s) image srcs become [`Block::Image`]s (the
    /// draw side fetches them asynchronously); when false they stay alt-text pills.
    remote_ok: bool,
}

/// Raw-HTML table under construction.
struct HtmlTbl {
    header: Vec<Vec<Run>>,
    rows: Vec<Vec<Vec<Run>>>,
    cur_row: Vec<Vec<Run>>,
    cur_cell: Option<Vec<Run>>,
    row_all_th: bool,
}

impl Builder {
    fn new(remote_ok: bool) -> Builder {
        Builder {
            remote_ok,
            out: Vec::new(),
            runs: Vec::new(),
            heading: None,
            in_para: false,
            in_quote: 0,
            in_item: false,
            lists: Vec::new(),
            strong: 0,
            emph: 0,
            strike: 0,
            code_html: 0,
            link: None,
            in_cell: false,
            cur_cell: Vec::new(),
            cur_row: Vec::new(),
            tbl_header: Vec::new(),
            tbl_rows: Vec::new(),
            tbl_aligns: Vec::new(),
            img: None,
            center: 0,
            html_stack: Vec::new(),
            html_buf: String::new(),
            skip_tag: None,
            in_comment: false,
            h_tbl: None,
        }
    }

    /// Append styled text to whatever is currently collecting (image alt / HTML table cell /
    /// markdown table cell / the current block's runs).
    pub(super) fn text(&mut self, s: &str) {
        if let Some((_, alt)) = &mut self.img {
            alt.push_str(s);
            return;
        }
        let (bold, italic, code, strike, link) =
            (self.strong > 0, self.emph > 0, self.code_html > 0, self.strike > 0, self.link.clone());
        // Pick the destination run buffer (HTML table cell / GFM table cell / current block).
        let target: &mut Vec<Run> = if let Some(t) = &mut self.h_tbl {
            match &mut t.cur_cell {
                Some(cell) => cell,
                None => return, // whitespace between HTML table cells — drop
            }
        } else if self.in_cell {
            &mut self.cur_cell
        } else {
            &mut self.runs
        };
        // Autolink bare URLs in plain (non-code, not-already-linked) text — GFM extended
        // autolinking, which pulldown-cmark 0.12 does NOT do on its own.
        if !code && link.is_none() {
            linkify_into(target, s, bold, italic, strike);
        } else {
            push_run(target, s, code, bold, italic, strike, link);
        }
    }

    /// Explicit-code text (markdown `` ` `` spans) — same routing, forced code style.
    fn code_text(&mut self, s: &str) {
        self.code_html += 1;
        self.text(s);
        self.code_html -= 1;
    }

    /// A hard line break within the current block.
    pub(super) fn newline(&mut self) {
        self.text("\n");
    }

    /// Close out the currently-accumulated runs as a block (heading > item > quote > para).
    pub(super) fn flush(&mut self) {
        let blank = self.runs.iter().all(|r| r.text.trim().is_empty());
        let taken = core::mem::take(&mut self.runs);
        if blank && self.heading.is_none() {
            return;
        }
        let center = self.center > 0;
        if let Some(lvl) = self.heading.take() {
            self.out.push(Block::Heading(lvl, taken, center));
        } else if self.in_item {
            let depth = (self.lists.len().saturating_sub(1)) as u8;
            let marker = match self.lists.last() {
                Some((true, n)) => format!("{n}."),
                _ => "•".to_string(),
            };
            self.out.push(Block::Item(depth, marker, taken));
        } else if self.in_quote > 0 {
            self.out.push(Block::Quote(taken));
        } else {
            self.out.push(Block::Para(taken, center));
        }
    }

    // ---- semantic ops shared with the HTML feeder ----------------------------------------

    pub(super) fn start_heading(&mut self, level: u8) {
        self.flush();
        self.heading = Some(level);
    }
    pub(super) fn end_heading(&mut self) {
        self.flush();
    }
    pub(super) fn open_para(&mut self) {
        self.flush();
        self.in_para = true;
    }
    pub(super) fn close_para(&mut self) {
        self.flush();
        self.in_para = false;
    }
    pub(super) fn rule(&mut self) {
        self.flush();
        self.out.push(Block::Rule);
    }
    pub(super) fn bold(&mut self, on: bool) {
        adj(&mut self.strong, on);
    }
    pub(super) fn italic(&mut self, on: bool) {
        adj(&mut self.emph, on);
    }
    pub(super) fn strikethrough(&mut self, on: bool) {
        adj(&mut self.strike, on);
    }
    pub(super) fn code(&mut self, on: bool) {
        adj(&mut self.code_html, on);
    }
    pub(super) fn set_link(&mut self, url: Option<String>) {
        self.link = url;
    }
    pub(super) fn open_container(&mut self, tag: &str, centers: bool) {
        self.flush();
        if centers {
            self.center += 1;
        }
        self.html_stack.push((tag.to_string(), centers));
    }
    pub(super) fn close_container(&mut self, tag: &str) {
        self.flush();
        // pop the nearest matching open tag (HTML in READMEs is flat; be forgiving)
        if let Some(pos) = self.html_stack.iter().rposition(|(t, _)| t == tag) {
            let (_, centered) = self.html_stack.remove(pos);
            if centered {
                self.center = self.center.saturating_sub(1);
            }
        }
    }
    pub(super) fn open_quote(&mut self) {
        self.flush();
        self.in_quote += 1;
    }
    pub(super) fn close_quote(&mut self) {
        self.flush();
        self.in_quote = self.in_quote.saturating_sub(1);
    }
    pub(super) fn open_list(&mut self, ordered: bool, start: u64) {
        self.flush();
        self.lists.push((ordered, start));
    }
    pub(super) fn close_list(&mut self) {
        self.flush();
        self.lists.pop();
    }
    pub(super) fn open_item(&mut self) {
        self.flush();
        self.in_item = true;
    }
    pub(super) fn close_item(&mut self) {
        self.flush();
        self.in_item = false;
        if let Some((true, n)) = self.lists.last_mut() {
            *n += 1;
        }
    }

    /// An image: local (or remote with the opt-in toggle) src -> its own [`Block::Image`];
    /// otherwise -> alt-text pill run.
    pub(super) fn image(&mut self, src: &str, alt: &str, width: ImgW) {
        let link = self.link.clone();
        // `//`/`data:` never render. Of the web schemes, only httpS can ever succeed (the fetch
        // layer is HTTPS-only), so plain `http://` pills up front instead of spawning a worker
        // that is guaranteed to fail (review finding, 2026-07-13).
        let fetchable = src.trim_start().to_ascii_lowercase().starts_with("https://");
        let remote = (is_remote_src(src) && !(self.remote_ok && fetchable))
            || src.starts_with("//")
            || src.starts_with("data:");
        let in_cell = self.in_cell || self.h_tbl.as_ref().is_some_and(|t| t.cur_cell.is_some());
        // Inside a list item or blockquote a block-level image would SPLIT the block (flush mid-
        // item duplicates the marker; a quote's bar breaks in two) and escape its indent — degrade
        // to the inline pill there, same as cells/headings (review finding, 2026-07-13).
        if remote || in_cell || self.heading.is_some() || self.in_item || self.in_quote > 0 {
            let label = if alt.trim().is_empty() { "image" } else { alt.trim() };
            // NBSP-join so the pill lays out as ONE unbroken token (its shaded panel stays whole).
            let label = label.replace(' ', "\u{00A0}");
            let text = format!("\u{00A0}{label}\u{00A0}");
            let (bold, italic) = (self.strong > 0, self.emph > 0);
            let tgt = if let Some(t) = &mut self.h_tbl {
                match &mut t.cur_cell {
                    Some(cell) => cell,
                    None => return,
                }
            } else if self.in_cell {
                &mut self.cur_cell
            } else {
                &mut self.runs
            };
            tgt.push(Run { text, bold, italic, code: true, strike: false, link });
        } else {
            self.flush();
            self.out.push(Block::Image(ImgBlock {
                src: src.to_string(),
                alt: alt.to_string(),
                width,
                center: self.center > 0,
                link,
            }));
        }
    }

    // ---- raw-HTML table ops ---------------------------------------------------------------

    pub(super) fn html_table_open(&mut self) {
        self.flush();
        self.h_tbl = Some(HtmlTbl {
            header: Vec::new(),
            rows: Vec::new(),
            cur_row: Vec::new(),
            cur_cell: None,
            row_all_th: true,
        });
    }
    pub(super) fn html_tr_open(&mut self) {
        if let Some(t) = &mut self.h_tbl {
            t.cur_row.clear();
            t.cur_cell = None;
            t.row_all_th = true;
        }
    }
    pub(super) fn html_cell_open(&mut self, th: bool) {
        if let Some(t) = &mut self.h_tbl {
            if let Some(c) = t.cur_cell.take() {
                t.cur_row.push(c); // unclosed previous cell
            }
            t.cur_cell = Some(Vec::new());
            t.row_all_th &= th;
        }
    }
    pub(super) fn html_cell_close(&mut self) {
        if let Some(t) = &mut self.h_tbl {
            if let Some(c) = t.cur_cell.take() {
                t.cur_row.push(c);
            }
        }
    }
    pub(super) fn html_tr_close(&mut self) {
        if let Some(t) = &mut self.h_tbl {
            if let Some(c) = t.cur_cell.take() {
                t.cur_row.push(c);
            }
            let row = core::mem::take(&mut t.cur_row);
            if row.is_empty() {
                return;
            }
            if t.row_all_th && t.header.is_empty() && t.rows.is_empty() {
                t.header = row;
            } else {
                t.rows.push(row);
            }
        }
    }
    pub(super) fn html_table_close(&mut self) {
        self.html_tr_close(); // forgive an unclosed final row
        if let Some(t) = self.h_tbl.take() {
            if !t.header.is_empty() || !t.rows.is_empty() {
                self.out.push(Block::Table { header: t.header, rows: t.rows, aligns: Vec::new() });
            }
        }
    }
}

fn adj(v: &mut u32, on: bool) {
    if on {
        *v += 1;
    } else {
        *v = v.saturating_sub(1);
    }
}

/// Append `text` as a run with the given inline style, merging into the previous run when the
/// style matches (keeps the token stream tight).
fn push_run(runs: &mut Vec<Run>, text: &str, code: bool, bold: bool, italic: bool, strike: bool, link: Option<String>) {
    if text.is_empty() {
        return;
    }
    if !code {
        if let Some(last) = runs.last_mut() {
            if !last.code && last.bold == bold && last.italic == italic && last.strike == strike && last.link == link {
                last.text.push_str(text);
                return;
            }
        }
    }
    runs.push(Run { text: text.to_string(), bold, italic, code, strike, link });
}

/// Split `s` into plain-text runs and clickable link runs for any bare URLs it contains — the
/// GFM "extended autolink" behaviour (`https://…`, `http://…`, `www.…` in running prose become
/// links) that pulldown-cmark 0.12 does not do itself. Only called for plain text (never inside
/// code or an existing `[text](url)` link).
fn linkify_into(runs: &mut Vec<Run>, s: &str, bold: bool, italic: bool, strike: bool) {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut plain_start = 0;
    while i < bytes.len() {
        // Cheap gate: extended autolinks only ever begin with `h` (http) or `w` (www).
        if matches!(bytes[i] | 0x20, b'h' | b'w') {
            if let Some((len, url)) = url_at(s, i) {
                if plain_start < i {
                    push_run(runs, &s[plain_start..i], false, bold, italic, strike, None);
                }
                push_run(runs, &s[i..i + len], false, bold, italic, strike, Some(url));
                i += len;
                plain_start = i;
                continue;
            }
        }
        i += 1;
    }
    if plain_start < s.len() {
        push_run(runs, &s[plain_start..], false, bold, italic, strike, None);
    }
}

/// If a bare URL starts at byte `i` in `s`, return its `(byte length, resolved destination)`.
/// Follows the GFM extended-autolink rules closely enough for prose: valid left boundary, a
/// `http(s)://` or `www.` prefix, a host containing a dot, and trailing-punctuation trimming
/// (with balanced-paren handling so `…/Foo_(bar)` keeps its `)`).
fn url_at(s: &str, i: usize) -> Option<(usize, String)> {
    let b = s.as_bytes();
    // Left boundary: start of run, whitespace, or a common opener — never mid-word (so
    // `foohttp://x` doesn't match).
    if i > 0 && !matches!(b[i - 1], b' ' | b'\t' | b'\n' | b'\r' | b'(' | b'[' | b'{' | b'<' | b'*' | b'_' | b'~' | b'"' | b'\'') {
        return None;
    }
    let rest = &s[i..];
    let lower = rest.as_bytes().iter().take(8).map(|c| c.to_ascii_lowercase()).collect::<Vec<u8>>();
    let (scheme_len, www) = if lower.starts_with(b"https://") {
        (8, false)
    } else if lower.starts_with(b"http://") {
        (7, false)
    } else if lower.starts_with(b"www.") {
        (4, true)
    } else {
        return None;
    };
    // Consume ASCII URL bytes (RFC-3986 unreserved + sub-delims + `:/?#[]@%`). Stopping at the
    // first non-URL byte ends the link at whitespace, quotes, `<`, backtick, AND any multibyte
    // (non-ASCII) char — the latter also guarantees every cut lands on a char boundary.
    let is_url_byte = |c: u8| {
        c.is_ascii_alphanumeric()
            || matches!(
                c,
                b'-' | b'.' | b'_' | b'~' | b':' | b'/' | b'?' | b'#' | b'[' | b']' | b'@'
                    | b'!' | b'$' | b'&' | b'\'' | b'(' | b')' | b'*' | b'+' | b',' | b';'
                    | b'=' | b'%'
            )
    };
    let mut end = 0;
    for (k, &c) in rest.as_bytes().iter().enumerate() {
        if !is_url_byte(c) {
            break;
        }
        end = k + 1;
    }
    if end <= scheme_len {
        return None; // nothing after the scheme
    }
    // Trim trailing punctuation; keep a `)` only if the URL has more `(` than `)`.
    let raw = &rest.as_bytes()[..end];
    let mut e = end;
    while e > scheme_len {
        let c = raw[e - 1];
        if matches!(c, b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"' | b'*' | b'_' | b'~') {
            e -= 1;
        } else if c == b')' {
            let opens = raw[..e].iter().filter(|&&x| x == b'(').count();
            let closes = raw[..e].iter().filter(|&&x| x == b')').count();
            if closes > opens {
                e -= 1;
            } else {
                break;
            }
        } else {
            break;
        }
    }
    if e <= scheme_len {
        return None;
    }
    let url = &s[i..i + e];
    // Require a dot in the host portion (rejects `https://localhost`-only noise and bare schemes).
    if !url[scheme_len..].contains('.') {
        return None;
    }
    let dest = if www { format!("https://{url}") } else { url.to_string() };
    Some((e, dest))
}

/// Walk the markdown events into a flat block list with inline styled runs. Raw HTML (block
/// AND inline) is routed through [`super::mdhtml::feed`] into the same builder.
fn parse_blocks(md: &str, remote_ok: bool) -> Vec<Block> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);

    let mut b = Builder::new(remote_ok);
    let mut in_code = false;
    let mut code_buf = String::new();
    let mut code_lang = highlight::Lang::Plain;

    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(Tag::Heading { level, .. }) => b.start_heading(heading_num(level)),
            Event::End(TagEnd::Heading(_)) => b.end_heading(),
            Event::Start(Tag::Paragraph) => b.open_para(),
            Event::End(TagEnd::Paragraph) => b.close_para(),
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        highlight::lang_from_fence(info.split_whitespace().next().unwrap_or(""))
                    }
                    CodeBlockKind::Indented => highlight::Lang::Plain,
                };
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                let text = code_buf.trim_end_matches('\n').to_string();
                code_buf.clear();
                if !text.is_empty() {
                    b.flush();
                    b.out.push(Block::Code(text, code_lang));
                }
            }
            Event::Start(Tag::List(start)) => b.open_list(start.is_some(), start.unwrap_or(1)),
            Event::End(TagEnd::List(_)) => b.close_list(),
            Event::Start(Tag::Item) => b.open_item(),
            Event::End(TagEnd::Item) => b.close_item(),
            Event::Start(Tag::BlockQuote(_)) => b.open_quote(),
            Event::End(TagEnd::BlockQuote(_)) => b.close_quote(),
            Event::Start(Tag::Table(aligns)) => {
                b.flush();
                b.tbl_header.clear();
                b.tbl_rows.clear();
                b.tbl_aligns = aligns
                    .iter()
                    .map(|a| match a {
                        Alignment::Center => 1,
                        Alignment::Right => 2,
                        _ => 0,
                    })
                    .collect();
            }
            Event::End(TagEnd::Table) => {
                let header = core::mem::take(&mut b.tbl_header);
                let rows = core::mem::take(&mut b.tbl_rows);
                let aligns = core::mem::take(&mut b.tbl_aligns);
                b.out.push(Block::Table { header, rows, aligns });
            }
            Event::Start(Tag::TableHead) => b.cur_row.clear(),
            Event::End(TagEnd::TableHead) => b.tbl_header = core::mem::take(&mut b.cur_row),
            Event::Start(Tag::TableRow) => b.cur_row.clear(),
            Event::End(TagEnd::TableRow) => {
                let row = core::mem::take(&mut b.cur_row);
                b.tbl_rows.push(row);
            }
            Event::Start(Tag::TableCell) => {
                b.in_cell = true;
                b.cur_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                b.in_cell = false;
                let cell = core::mem::take(&mut b.cur_cell);
                b.cur_row.push(cell);
            }
            Event::Start(Tag::Strong) => b.bold(true),
            Event::End(TagEnd::Strong) => b.bold(false),
            Event::Start(Tag::Emphasis) => b.italic(true),
            Event::End(TagEnd::Emphasis) => b.italic(false),
            Event::Start(Tag::Strikethrough) => b.strikethrough(true),
            Event::End(TagEnd::Strikethrough) => b.strikethrough(false),
            Event::Start(Tag::Link { dest_url, .. }) => b.set_link(Some(dest_url.to_string())),
            Event::End(TagEnd::Link) => b.set_link(None),
            Event::Start(Tag::Image { dest_url, .. }) => {
                b.img = Some((dest_url.to_string(), String::new()));
            }
            Event::End(TagEnd::Image) => {
                if let Some((src, alt)) = b.img.take() {
                    b.image(&src, &alt, ImgW::Natural);
                }
            }
            Event::Start(Tag::HtmlBlock) => b.html_buf.clear(),
            Event::Html(s) => b.html_buf.push_str(&s),
            Event::End(TagEnd::HtmlBlock) => {
                let buf = core::mem::take(&mut b.html_buf);
                super::mdhtml::feed(&mut b, &buf);
            }
            Event::InlineHtml(s) => super::mdhtml::feed(&mut b, &s),
            Event::Rule => b.rule(),
            Event::Text(t) => {
                if in_code {
                    code_buf.push_str(&t);
                } else {
                    b.text(&t);
                }
            }
            Event::Code(t) => {
                if b.img.is_some() {
                    b.text(&t); // alt-text fragment
                } else {
                    b.code_text(&t);
                }
            }
            Event::SoftBreak => b.text(" "),
            Event::HardBreak => b.newline(),
            Event::TaskListMarker(done) => b.text(if done { "[x] " } else { "[ ] " }),
            _ => {}
        }
    }
    // trailing text + any half-open raw-HTML structures
    b.html_table_close();
    b.flush();
    b.out
}

fn heading_num(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Collect linkified runs as (text, is_link, dest) triples for assertions.
    fn linkify(s: &str) -> Vec<(String, Option<String>)> {
        let mut runs = Vec::new();
        linkify_into(&mut runs, s, false, false, false);
        runs.into_iter().map(|r| (r.text, r.link)).collect()
    }

    #[test]
    fn bare_https_becomes_a_link() {
        let r = linkify("see https://example.com/x now");
        assert_eq!(r, vec![
            ("see ".into(), None),
            ("https://example.com/x".into(), Some("https://example.com/x".into())),
            (" now".into(), None),
        ]);
    }

    #[test]
    fn www_gets_https_scheme() {
        let r = linkify("go www.example.com today");
        assert_eq!(r[1], ("www.example.com".into(), Some("https://www.example.com".into())));
    }

    #[test]
    fn trailing_punctuation_trimmed_but_url_kept() {
        // sentence-ending period is not part of the link
        let r = linkify("visit https://example.com.");
        assert_eq!(r[1], ("https://example.com".into(), Some("https://example.com".into())));
        assert_eq!(r[2].0, ".");
    }

    #[test]
    fn balanced_paren_kept_unbalanced_trimmed() {
        let kept = url_at("https://en.wikipedia.org/wiki/Foo_(bar)", 0).unwrap();
        assert_eq!(kept.1, "https://en.wikipedia.org/wiki/Foo_(bar)");
        // a wrapping paren is NOT swallowed: "(https://x.com)" trims the trailing ')'
        let wrapped = url_at("(https://x.com)", 1).unwrap();
        assert_eq!(wrapped.1, "https://x.com");
    }

    #[test]
    fn no_match_mid_word_or_without_dot() {
        assert!(url_at("foohttps://x.com", 3).is_none()); // 'o' precedes → not a boundary
        assert!(url_at("https://localhost", 0).is_none()); // no dot in host
        assert!(url_at("https://", 0).is_none()); // bare scheme
    }

    #[test]
    fn multibyte_after_url_is_safe() {
        // a CJK period right after the URL must not panic on a non-char-boundary slice
        let r = linkify("https://example.com。あと");
        assert_eq!(r[0].1, Some("https://example.com".into()));
        assert!(r[1].0.starts_with('。'));
    }
}
