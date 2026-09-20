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

/// Is this image src ever pilled instead of rendered inline, i.e. does the "load web images"
/// toolbar button need to exist for this document at all? Matches exactly what `Builder::image`
/// treats as remote: `http(s)://` (unlockable via the toggle) plus protocol-relative `//` and
/// embedded `data:` (never unlockable — `Builder::image` pills those unconditionally). One
/// shared predicate for `has_remote_images`, `html_has_remote_img`, and `Builder::image` itself,
/// so a document whose only images are protocol-relative can never silently fail to show the
/// button that explains why they are pills.
pub(super) fn is_gated_image_src(src: &str) -> bool {
    is_remote_src(src) || src.starts_with("//") || src.starts_with("data:")
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
        Event::Start(Tag::Image { dest_url, .. }) => is_gated_image_src(&dest_url),
        Event::Html(s) | Event::InlineHtml(s) => html_has_remote_img(&s),
        _ => false,
    })
}

/// The `src="…` attribute-value openers `html_has_remote_img`'s loose scan looks for: one per
/// quote style, for each scheme `is_gated_image_src` treats as gated. Kept as one list so the
/// two checks cannot drift the way they did before this fix.
const GATED_IMG_SRC_NEEDLES: [&str; 9] = [
    "src=\"http",
    "src='http",
    "src=http",
    "src=\"//",
    "src='//",
    "src=//",
    "src=\"data:",
    "src='data:",
    "src=data:",
];

/// `<img …src="http…">` (or `//…`/`data:…`) in a raw-HTML chunk. Deliberately loose (it does not
/// parse attributes, it just looks for an `src` pointing at a gated scheme in a chunk that
/// contains an `<img`), which is the right trade for a button-visibility check.
fn html_has_remote_img(s: &str) -> bool {
    let low = s.to_ascii_lowercase();
    if !low.contains("<img") {
        return false;
    }
    GATED_IMG_SRC_NEEDLES.iter().any(|n| low.contains(n))
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

/// Refresh the layout's PARSE half (blocks + selection document + offsets) when either the
/// document or the remote-images flag changed. This half is independent of wrap width: a
/// resize can never invalidate it, which is why it's keyed and refreshed separately from
/// [`refresh_height_cache`] below.
fn refresh_parse_cache(layout: &mut MdLayout, md: &str, remote_ok: bool, gen: u64) {
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
}

/// Refresh the layout's measured-HEIGHTS half when the wrap width changed (or the block count
/// did, which follows a fresh parse); unlike the parse above, these die on every width change.
fn refresh_height_cache(layout: &mut MdLayout, full_w: i32) {
    if layout.width_key != Some(full_w) || layout.heights.len() != layout.blocks.len() {
        layout.heights = vec![-1; layout.blocks.len()];
        layout.width_key = Some(full_w);
    }
}

/// The `doc` byte offset of the block `base` starts at, or `None` for a block with no
/// selectable text (a rule/image — [`DocBase::None`]).
fn block_start_offset(base: &DocBase) -> Option<usize> {
    match base {
        DocBase::Runs(offs) => offs.first().copied(),
        DocBase::Code(off) => Some(*off),
        DocBase::Table(rows) => rows
            .iter()
            .flat_map(|row| row.iter())
            .flat_map(|cell| cell.iter())
            .next()
            .copied(),
        DocBase::None => None,
    }
}

/// The document-px y of the top of the block containing selection offset `off`, from the
/// already-measured [`MdLayout`] alone — an EXACT jump target, unlike the fine 24px stepper
/// [`super::selection::ensure_visible`] falls back to for a target still inside the same block
/// (or when no full measurement exists yet). `None` when `heights` isn't fully measured for
/// every block up to and including the one `off` lands in (nothing painted it yet, so there is
/// no y to trust), or when `off` doesn't land inside any block's own text.
pub(super) fn y_for_offset(layout: &MdLayout, off: usize) -> Option<i32> {
    if !layout.ready || layout.heights.len() != layout.bases.len() {
        return None;
    }
    let mut y = 0i32;
    let mut best: Option<i32> = None;
    for (h, base) in layout.heights.iter().zip(layout.bases.iter()) {
        let start = block_start_offset(base);
        if start.is_some_and(|s| s > off) {
            // Offsets only increase, so every later block starts even further past `off` —
            // `best` (if set) is already the containing block; nothing more to check.
            break;
        }
        if *h < 0 {
            // This block starts at or before `off` and was never measured, so the
            // containing block is this one or a later one and its y is unknown.
            return None;
        }
        if start.is_some() {
            best = Some(y);
        }
        // A block with no offset (a rule/image) carries no information either way — just fold
        // its height in and keep scanning; it neither confirms nor rules out `best`.
        y += h;
    }
    best
}

/// Push `block`'s outline entry when it's a heading, called unconditionally (before any
/// off-screen skip below) so the ToC stays complete even when the heading itself is culled.
/// `pre` matches the in-arm pre-margin so click targets align.
fn record_heading_toc(
    block: &Block,
    toc: &mut Vec<TocEntry>,
    pre: i32,
    y: i32,
    top: i32,
    scroll: i32,
) {
    if let Block::Heading(lvl, runs, _) = block {
        toc.push(TocEntry {
            level: *lvl,
            text: runs_text(runs),
            target: (y + pre - top + scroll).max(0),
        });
    }
}

/// A cached text-block height IF it's safe to skip re-measuring `bi`: already measured
/// (`h >= 0`) and fully off-screen at `y`. `None` means render it normally (never measured, or
/// on-screen).
fn cached_offscreen_height(heights: &[i32], bi: usize, y: i32, rc: &RECT) -> Option<i32> {
    let h = heights.get(bi).copied().unwrap_or(-1);
    (h >= 0 && (y + h <= rc.top || y >= rc.bottom)).then_some(h)
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

    // One font cache for this whole paint pass: every block below asks it for the
    // (px, bold, italic) it needs instead of creating its own `Fonts` set, so a document with
    // many headings/paragraphs/list-items/quotes builds each distinct style once per repaint
    // rather than once per block. Freed automatically (`Drop`) when this function returns.
    let mut fonts_cache = FontCache::default();

    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
    let margin = sc(18);
    // Content column = the whole pane minus margins, so a wider window really does
    // re-wrap the text wider (see the note on the metrics above).
    let full_w = (rc.right - rc.left - 2 * margin).max(1);
    let x0 = rc.left + margin;
    let top = rc.top + margin;
    let mut y = top - scroll;
    let mut first = true;

    // Layout cache, keyed in TWO parts because the two halves have different lifetimes — see
    // `refresh_parse_cache`/`refresh_height_cache`. Splitting them is what makes a
    // width-tracking column affordable: dragging the frame (or a 15 ms tick of the ToC slide,
    // which now genuinely changes the wrap width) re-measures only the blocks the culling loop
    // actually reaches, instead of re-running the whole pulldown-cmark walk over a document up
    // to the 5 MB text cap on every frame.
    refresh_parse_cache(layout, md, remote_ok, gen);
    refresh_height_cache(layout, full_w);
    // Cheap handle, not a copy — lets the loop below read the blocks while still writing
    // measured heights back into `layout`.
    let blocks = std::rc::Rc::clone(&layout.blocks);
    let bench_t = std::env::var_os("ST2K_MD_BENCH")
        .is_some()
        .then(std::time::Instant::now);

    for (bi, block) in blocks.iter().enumerate() {
        record_heading_toc(block, toc, if first { 0 } else { sc(8) }, y, top, scroll);
        // Fast-path: a text block we've already measured that's fully off-screen — skip the
        // run_block re-measure entirely and just advance by the cached height.
        let is_text = matches!(
            block,
            Block::Heading(..) | Block::Para(..) | Block::Item(..) | Block::Quote(..)
        );
        if is_text {
            if let Some(h) = cached_offscreen_height(&layout.heights, bi, y, rc) {
                y += h;
                first = false;
                continue;
            }
        }
        y = paint_block(
            hwnd,
            hdc,
            rc,
            block,
            bi,
            y,
            first,
            is_text,
            x0,
            full_w,
            c,
            links,
            imgs,
            doc_dir,
            gen,
            layout,
            sel,
            &mut fonts_cache,
        );
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

/// Paint one block at `y`, cache its measured height when `is_text`, and return the y after it.
#[allow(clippy::too_many_arguments)] // GDI layout pass: target + geometry + out-collectors, no struct gain
unsafe fn paint_block(
    hwnd: HWND,
    hdc: HDC,
    rc: &RECT,
    block: &Block,
    bi: usize,
    mut y: i32,
    first: bool,
    is_text: bool,
    x0: i32,
    full_w: i32,
    c: &MdColors,
    links: &mut Vec<LinkHit>,
    imgs: &mut ImgCache,
    doc_dir: Option<&Path>,
    gen: u64,
    layout: &mut MdLayout,
    sel: &mut MdSel,
    fonts_cache: &mut FontCache,
) -> i32 {
    let sc = |v: i32| crate::win::dpi_scale(hwnd, v);
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
    let mut p = PaintCtx {
        hwnd,
        hdc,
        rc,
        x0,
        full_w,
        c,
        links: &mut *links,
        rsel: &mut rsel,
        fonts_cache,
    };
    let y_block_start = y;
    match block {
        Block::Heading(level, runs, center) => {
            y = paint_heading(&mut p, *level, runs, *center, first, y);
        }
        Block::Para(runs, center) => {
            y = paint_para(&mut p, runs, *center, y);
        }
        Block::Code(text, lang) => {
            let base = match layout.bases.get(bi) {
                Some(DocBase::Code(b)) => *b,
                _ => 0,
            };
            y = paint_code(&mut p, text, *lang, y, base);
        }
        Block::Item(depth, marker, runs, task) => {
            y = paint_item(&mut p, *depth, marker, runs, *task, y);
        }
        Block::Quote(runs) => {
            y = paint_quote(&mut p, runs, y);
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
                fonts_cache,
            );
            y += sc(14);
        }
        Block::Image(ib) => {
            y = draw_image(
                hwnd,
                hdc,
                rc,
                ib,
                x0,
                y,
                full_w,
                c,
                links,
                imgs,
                doc_dir,
                gen,
                fonts_cache,
            );
        }
    }
    // Cache the text block's just-measured height (spacing included) for the skip fast-path.
    if is_text {
        if let Some(slot) = layout.heights.get_mut(bi) {
            *slot = y - y_block_start;
        }
    }
    y
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
mod paintblocks;
use paintblocks::*;

pub(super) use images::decode_bytes_to_dib;
pub(super) use inline::font_for;
use parse::parse_blocks;
pub(super) use parse::Builder;
#[cfg(test)]
use parse::{linkify_into, url_at};

#[cfg(test)]
mod tests;
