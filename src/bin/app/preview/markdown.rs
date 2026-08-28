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
    DrawTextW, ExtTextOutW, FillRect, GetTextExtentExPointW, GetTextExtentPoint32W,
    GetTextMetricsW, IntersectClipRect, LineTo, MoveToEx, RestoreDC, RoundRect, SaveDC,
    SelectObject, SetBkColor, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt,
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
    code: bool,           // inline `code` / alt-text pill (mono + shaded background)
    strike: bool,         // ~~strikethrough~~
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

/// The pulldown-cmark [`Options`] shared by every markdown pass over a document: the real
/// render ([`parse::parse_blocks`]) and the pre-decide toolbar-visibility scans below
/// ([`has_headings`], [`has_remote_images`]). One source of truth so a flag added here
/// reaches the toolbar checks for free, instead of the same 3-line block silently drifting
/// out of sync at one of the three call sites.
pub(super) fn md_options() -> Options {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts
}

/// Does the markdown contain any heading (markdown `#`/setext OR a raw-HTML `<h1>`-`<h6>`)?
/// Used ONCE at load time to decide whether the outline sidebar/toolbar-toggle exist at all.
/// Parses with the SAME options as [`render`] so it agrees with what the render will list.
pub(super) fn has_headings(md: &str) -> bool {
    let opts = md_options();
    Parser::new_ext(md, opts).any(|ev| match ev {
        Event::Start(Tag::Heading { .. }) => true,
        Event::Html(s) | Event::InlineHtml(s) => html_has_heading(&s),
        _ => false,
    })
}

/// Cheap scan for `<h1`..`<h6` (case-insensitive) in a raw-HTML fragment.
/// Whether the document references any WEB-HOSTED image, i.e. whether the "load web images"
/// toolbar button has anything to act on. Same streaming parse as [`has_headings`] (no layout, no
/// allocation of the rendered document), run once per load.
///
/// Raw `<img src="http…">` counts too, because README hero blocks are written in HTML and their
/// badges are exactly the case this button exists for.
pub(super) fn has_remote_images(md: &str) -> bool {
    let opts = md_options();
    Parser::new_ext(md, opts).any(|ev| match ev {
        Event::Start(Tag::Image { dest_url, .. }) => is_remote_src(&dest_url),
        Event::Html(s) | Event::InlineHtml(s) => html_has_remote_img(&s),
        _ => false,
    })
}

/// `<img …src="http…">` in a raw-HTML chunk. Deliberately loose (it does not parse attributes,
/// it just looks for an `src` pointing at a web scheme in a chunk that contains an `<img`), which
/// is the right trade for a button-visibility check.
fn html_has_remote_img(s: &str) -> bool {
    let low = s.to_ascii_lowercase();
    if !low.contains("<img") {
        return false;
    }
    low.contains("src=\"http") || low.contains("src='http") || low.contains("src=http")
}

fn html_has_heading(s: &str) -> bool {
    let b = s.as_bytes();
    b.windows(3)
        .any(|w| w[0] == b'<' && (w[1] | 0x20) == b'h' && (b'1'..=b'6').contains(&w[2]))
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
    /// (indent depth, bullet/number marker, runs, task-checkbox state). `task` is
    /// `Some(done)` for a GFM task-list item (`- [ ]`/`- [x]`) — the draw side renders a
    /// checkbox in place of the bullet; `None` for an ordinary list item.
    Item(u8, String, Vec<Run>, Option<bool>),
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
/// file. Two keys, because the halves have different lifetimes: the parse is keyed by (decode gen,
/// remote-images flag) and survives any resize, while the measured heights are keyed by the wrap
/// width alone. Only the text blocks' heights are cached; code/tables/images are cheap-to-measure
/// or async, so they always re-run.
#[derive(Default)]
pub(super) struct MdLayout {
    ready: bool,
    /// What the PARSE below was built from: `(decode generation, remote-images allowed)`.
    parse_key: (u64, bool),
    /// The wrap width `heights` was measured at — `None` forces a re-measure.
    width_key: Option<i32>,
    /// The PARSED document. Cached with everything else — it used to be re-parsed on every
    /// single paint, which meant the whole pulldown-cmark walk ran again for each 15 ms tick of
    /// the ToC slide animation and each wheel notch, on documents up to the 5 MB text cap. `Rc`
    /// so the render loop can hold it while still mutating `heights` on the same struct.
    blocks: std::rc::Rc<Vec<Block>>,
    heights: Vec<i32>, // per block index; -1 = unmeasured
    /// The RENDERED text of the whole document — the coordinate space every selection offset
    /// lives in (see [`super::selection`]). Complete regardless of what's painted/culled, so
    /// Ctrl+A and copy cover the whole file. Depends only on the parse, so a scroll never
    /// invalidates it and offsets stay stable across paints.
    pub(super) doc: String,
    /// Where each block's runs landed in `doc` — parallel to the block list.
    bases: Vec<DocBase>,
}

// GitHub-ish metrics (CSS px @96dpi, DPI-scaled at draw): 16px body, 6x13 table cell padding,
// 4px quote bar. Headings 2em/1.5em/1.25em/1em/0.875em/0.85em.
//
// GitHub's fixed ~880px content column is deliberately NOT copied. This is a window the user
// SIZES: with a hard cap, dragging the frame wider only grew the empty gutters while every
// paragraph stayed wrapped at the same place — which reads as "resizing does nothing" (and the
// default 1000px window was already past the cap, so it did nothing from the very first drag).
// The column now tracks the pane, so widening genuinely un-wraps the text.
const BODY_PX: i32 = 16;
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
    // Content column = the whole pane minus margins, so a wider window really does
    // re-wrap the text wider (see the note on the metrics above).
    let full_w = (rc.right - rc.left - 2 * margin).max(1);
    let x0 = rc.left + margin;
    let top = rc.top + margin;
    let mut y = top - scroll;
    let mut first = true;

    // Layout cache, keyed in TWO parts because the two halves have different lifetimes:
    //   * the PARSE (blocks + selection document + offsets) depends only on the document and the
    //     remote-images flag — a resize can never change it;
    //   * the measured block HEIGHTS depend on the wrap width, so they die on every width change.
    // Splitting them is what makes a width-tracking column affordable: dragging the frame (or a
    // 15 ms tick of the ToC slide, which now genuinely changes the wrap width) re-measures only
    // the blocks the culling loop actually reaches, instead of re-running the whole pulldown-cmark
    // walk over a document up to the 5 MB text cap on every frame.
    let parse_key = (gen, remote_ok);
    if !layout.ready || layout.parse_key != parse_key {
        layout.blocks = std::rc::Rc::new(parse_blocks(md, remote_ok));
        let (doc, bases) = build_doc(&layout.blocks);
        layout.doc = doc;
        layout.bases = bases;
        layout.parse_key = parse_key;
        layout.width_key = None;
        layout.ready = true;
    }
    if layout.width_key != Some(full_w) || layout.heights.len() != layout.blocks.len() {
        layout.heights = vec![-1; layout.blocks.len()];
        layout.width_key = Some(full_w);
    }
    // Cheap handle, not a copy — lets the loop below read the blocks while still writing
    // measured heights back into `layout`.
    let blocks = std::rc::Rc::clone(&layout.blocks);
    let bench_t = std::env::var_os("ST2K_MD_BENCH")
        .is_some()
        .then(std::time::Instant::now);

    for (bi, block) in blocks.iter().enumerate() {
        // Outline entry for every heading, BEFORE any skip, so the ToC stays complete even when the
        // heading is culled off-screen. `+pre` matches the in-arm pre-margin so click targets align.
        if let Block::Heading(lvl, runs, _) = block {
            let pre = if first { 0 } else { sc(8) };
            toc.push(TocEntry {
                level: *lvl,
                text: runs_text(runs),
                target: (y + pre - top + scroll).max(0),
            });
        }
        // Fast-path: a text block we've already measured that's fully off-screen — skip the
        // run_block re-measure entirely and just advance by the cached height.
        let is_text = matches!(
            block,
            Block::Heading(..) | Block::Para(..) | Block::Item(..) | Block::Quote(..)
        );
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
        let mut rsel = RunSel {
            range: sel.range,
            doc: &layout.doc,
            bases: run_bases,
            hits: &mut *sel.hits,
            bg: c.sel,
        };
        let y_block_start = y;
        match block {
            Block::Heading(level, runs, center) => {
                y = paint_heading(
                    hwnd, hdc, rc, *level, runs, *center, first, x0, y, full_w, c, links, &mut rsel,
                );
            }
            Block::Para(runs, center) => {
                y = paint_para(
                    hwnd, hdc, rc, runs, *center, x0, y, full_w, c, links, &mut rsel,
                );
            }
            Block::Code(text, lang) => {
                let base = match layout.bases.get(bi) {
                    Some(DocBase::Code(b)) => *b,
                    _ => 0,
                };
                y = paint_code(
                    hwnd,
                    hdc,
                    rc,
                    text,
                    *lang,
                    x0,
                    y,
                    full_w,
                    c,
                    sel.range,
                    &mut *sel.hits,
                    base,
                );
            }
            Block::Item(depth, marker, runs, task) => {
                y = paint_item(
                    hwnd, hdc, rc, *depth, marker, runs, *task, x0, y, full_w, c, links, &mut rsel,
                );
            }
            Block::Quote(runs) => {
                y = paint_quote(hwnd, hdc, rc, runs, x0, y, full_w, c, links, &mut rsel);
            }
            Block::Rule => {
                // GitHub hr: a short solid bar, not a hairline.
                let bar = RECT {
                    left: x0,
                    top: y + sc(8),
                    right: x0 + full_w,
                    bottom: y + sc(8) + sc(3),
                };
                let hb = CreateSolidBrush(COLORREF(c.border));
                FillRect(hdc, &bar, hb);
                let _ = DeleteObject(hb.into());
                y += sc(26);
            }
            Block::Table {
                header,
                rows,
                aligns,
            } => {
                let tbases: &[Vec<Vec<usize>>] = match layout.bases.get(bi) {
                    Some(DocBase::Table(v)) => v,
                    _ => &[],
                };
                let mut tsel = TblSel {
                    range: sel.range,
                    doc: &layout.doc,
                    bases: tbases,
                    hits: &mut *sel.hits,
                    bg: c.sel,
                };
                y = draw_table(
                    hwnd,
                    hdc,
                    header,
                    rows,
                    aligns,
                    x0,
                    y,
                    full_w,
                    c,
                    links,
                    &mut tsel,
                    (rc.top, rc.bottom),
                );
                y += sc(14);
            }
            Block::Image(ib) => {
                y = draw_image(
                    hwnd, hdc, rc, ib, x0, y, full_w, c, links, imgs, doc_dir, gen,
                );
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
        eprintln!(
            "[md-bench] {} blocks, scroll {}px: {:?}",
            layout.heights.len(),
            scroll,
            t0.elapsed()
        );
    }
    y + scroll - top + margin // total content height
}

/// `Block::Heading` paint arm: heading text + the h1/h2 hairline underline.
#[allow(clippy::too_many_arguments)]
unsafe fn paint_heading(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    level: u8,
    runs: &[Run],
    center: bool,
    first: bool,
    x0: i32,
    mut y: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    rsel: &mut RunSel,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    if !first {
        y += sc(8); // extra top margin before a heading (GitHub 24px total)
    }
    let px = heading_px(level);
    let fonts = Fonts::new(hwnd, px, true, false);
    let ctx = ctx_for(hwnd, c, c.fg);
    let (ny, _) = run_block(
        hdc,
        runs,
        &fonts,
        x0,
        y,
        full_w,
        if center { 1 } else { 0 },
        y >= rc.bottom,
        &ctx,
        links,
        Some(rsel),
    );
    fonts.free();
    y = ny;
    if level <= 2 {
        // GitHub-style hairline under h1/h2.
        hline(hdc, x0, x0 + full_w, y + sc(4), c.border);
        y += sc(8);
    }
    y + sc(10)
}

/// `Block::Para` paint arm.
#[allow(clippy::too_many_arguments)]
unsafe fn paint_para(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    runs: &[Run],
    center: bool,
    x0: i32,
    y: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    rsel: &mut RunSel,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let fonts = Fonts::new(hwnd, BODY_PX, false, false);
    let ctx = ctx_for(hwnd, c, c.fg);
    let (ny, _) = run_block(
        hdc,
        runs,
        &fonts,
        x0,
        y,
        full_w,
        if center { 1 } else { 0 },
        y >= rc.bottom,
        &ctx,
        links,
        Some(rsel),
    );
    fonts.free();
    if ny > y {
        ny + sc(14)
    } else {
        y
    }
}

/// `Block::Code` paint arm: the rounded panel + syntax-highlighted, unwrapped lines.
#[allow(clippy::too_many_arguments)]
unsafe fn paint_code(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    text: &str,
    lang: highlight::Lang,
    x0: i32,
    y: i32,
    full_w: i32,
    c: &MdColors,
    sel_range: Option<(usize, usize)>,
    sel_hits: &mut Vec<SelHit>,
    base: usize,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
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
#[allow(clippy::too_many_arguments)]
unsafe fn paint_item(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    depth: u8,
    marker: &str,
    runs: &[Run],
    task: Option<bool>,
    x0: i32,
    y: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    rsel: &mut RunSel,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let indent = sc(22) * (depth as i32 + 1);
    let mx = x0 + indent - sc(18);
    match task {
        // GFM task item: a GitHub-style checkbox in place of the bullet.
        Some(done) => draw_checkbox(hwnd, hdc, mx, y, done, c),
        // Ordinary bullet / number in the muted colour.
        None => {
            let mf = font(hwnd, BODY_PX, false, false, false);
            draw_at(hdc, marker, mx, y, mf, c.muted);
            let _ = DeleteObject(mf.into());
        }
    }
    let fonts = Fonts::new(hwnd, BODY_PX, false, false);
    let ctx = ctx_for(hwnd, c, c.fg);
    let (ny, _) = run_block(
        hdc,
        runs,
        &fonts,
        x0 + indent,
        y,
        full_w - indent,
        0,
        y >= rc.bottom,
        &ctx,
        links,
        Some(rsel),
    );
    fonts.free();
    ny + sc(4)
}

/// `Block::Quote` paint arm: the runs, then the GitHub-style gray quote bar.
#[allow(clippy::too_many_arguments)]
unsafe fn paint_quote(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    runs: &[Run],
    x0: i32,
    y: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    rsel: &mut RunSel,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let indent = sc(16);
    let y_start = y;
    let fonts = Fonts::new(hwnd, BODY_PX, false, true);
    let ctx = ctx_for(hwnd, c, c.muted);
    let (ny, _) = run_block(
        hdc,
        runs,
        &fonts,
        x0 + indent,
        y,
        full_w - indent,
        0,
        y >= rc.bottom,
        &ctx,
        links,
        Some(rsel),
    );
    fonts.free();
    let y = ny;
    // GitHub-style gray quote bar spanning the quote's height.
    let pen = CreatePen(PS_SOLID, sc(4), COLORREF(c.border));
    let op = SelectObject(hdc, HGDIOBJ(pen.0));
    let _ = MoveToEx(hdc, x0 + sc(2), y_start, None);
    let _ = LineTo(hdc, x0 + sc(2), y);
    SelectObject(hdc, op);
    let _ = DeleteObject(HGDIOBJ(pen.0));
    y + sc(14)
}

mod doc;
mod images;
mod inline;
mod linebreak;
mod parse;
mod tables;

// Parent-hub imports: children glob-imported PRIVATELY so `render` below still sees the
// whole renderer as one namespace, and each child's `use super::*` sees the shared types.
use doc::*;
use images::*;
use inline::*;
use linebreak::*;
use tables::*;

pub(super) use images::decode_bytes_to_dib;
pub(super) use inline::font_for;
use parse::parse_blocks;
pub(super) use parse::Builder;
#[cfg(test)]
use parse::{linkify_into, url_at};

#[cfg(test)]
mod tests {
    use super::*;

    /// The real render, `has_headings`, and `has_remote_images` used to each build their
    /// own copy of the same 3-line `Options` block, so a flag added to one could silently
    /// desync from the other two (the toolbar would disagree with what actually renders).
    /// Locks the shared [`md_options`] to exactly the flags the renderer needs.
    #[test]
    fn md_options_matches_the_flags_the_renderer_needs() {
        let opts = md_options();
        assert!(opts.contains(Options::ENABLE_TABLES));
        assert!(opts.contains(Options::ENABLE_STRIKETHROUGH));
        assert!(opts.contains(Options::ENABLE_TASKLISTS));
        // Nothing else snuck in - a stray extra/missing bit here is exactly the kind of
        // silent drift the shared helper exists to make impossible.
        assert_eq!(
            opts,
            Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS
        );
    }

    /// Collect linkified runs as (text, is_link, dest) triples for assertions.
    fn linkify(s: &str) -> Vec<(String, Option<String>)> {
        let mut runs = Vec::new();
        linkify_into(&mut runs, s, false, false, false);
        runs.into_iter().map(|r| (r.text, r.link)).collect()
    }

    /// What Ctrl+C would put on the clipboard for `md` (pre-CRLF-normalisation).
    fn copied(md: &str) -> String {
        build_doc(&parse_blocks(md, false)).0
    }

    /// Structured Markdown must survive a copy/paste round trip. Every assertion here is a way
    /// the old flattening broke it: nesting depth was dropped, blocks were joined with a single
    /// newline (so paragraphs merged), and headings/quotes/fences lost their markers entirely.
    #[test]
    fn copy_preserves_document_structure() {
        let out = copied(
            "# Title\n\nIntro para.\n\nAnother para.\n\n\
             - top\n  - nested\n    - deeper\n- second\n\n\
             1. one\n2. two\n\n> quoted\n\n```rust\nfn f() {}\n```\n\n---\n",
        );
        // Blocks are separated by a BLANK line, so they don't merge into one paragraph.
        assert!(
            out.contains("# Title\n\nIntro para.\n\nAnother para.\n\n"),
            "got:\n{out}"
        );
        // Nesting survives, with a real Markdown bullet rather than the display glyph.
        assert!(
            out.contains("- top\n  - nested\n    - deeper\n- second"),
            "got:\n{out}"
        );
        assert!(
            !out.contains('\u{2022}'),
            "display bullet leaked into the copy:\n{out}"
        );
        // Consecutive items stay TIGHT (no blank line) or the list re-renders loose.
        assert!(!out.contains("- top\n\n"), "list went loose:\n{out}");
        // Ordered lists keep their numbers; quotes and fences keep their markers.
        assert!(out.contains("1. one\n2. two"), "got:\n{out}");
        // A DIFFERENT list still gets its blank line, or the two run together as one mangled list.
        assert!(
            out.contains("- second\n\n1. one"),
            "lists butted together:\n{out}"
        );
        assert!(out.contains("> quoted"), "got:\n{out}");
        assert!(out.contains("```rust\nfn f() {}\n```"), "got:\n{out}");
        assert!(out.contains("---"), "got:\n{out}");
    }

    /// GFM order is marker THEN checkbox. Emitting the box in place of the bullet produced
    /// "[x] done", which no Markdown renderer treats as a task list.
    #[test]
    fn copy_emits_valid_gfm_task_items() {
        let out = copied("- [x] done\n- [ ] todo\n");
        assert!(out.contains("- [x] done"), "got:\n{out}");
        assert!(out.contains("- [ ] todo"), "got:\n{out}");
    }

    /// A block that appends nothing must not leave its separator behind as a stray blank line,
    /// and the document must never start with one.
    #[test]
    fn copy_has_no_stray_blank_lines() {
        let out = copied("para one\n\npara two\n");
        assert!(!out.starts_with('\n'), "leading blank line:\n{out}");
        assert!(!out.contains("\n\n\n"), "doubled separator:\n{out}");
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn bare_https_becomes_a_link() {
        let r = linkify("see https://example.com/x now");
        assert_eq!(
            r,
            vec![
                ("see ".into(), None),
                (
                    "https://example.com/x".into(),
                    Some("https://example.com/x".into())
                ),
                (" now".into(), None),
            ]
        );
    }

    #[test]
    fn www_gets_https_scheme() {
        let r = linkify("go www.example.com today");
        assert_eq!(
            r[1],
            (
                "www.example.com".into(),
                Some("https://www.example.com".into())
            )
        );
    }

    #[test]
    fn trailing_punctuation_trimmed_but_url_kept() {
        // sentence-ending period is not part of the link
        let r = linkify("visit https://example.com.");
        assert_eq!(
            r[1],
            (
                "https://example.com".into(),
                Some("https://example.com".into())
            )
        );
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
