//! Minimal GDI+ helpers: anti-aliased outline/fill drawing onto a plain HDC.
//!
//! Raw GDI (`Rectangle`/`Ellipse`/`RoundRect`/`LineTo`/`Polyline`) has **no
//! anti-aliasing** — diagonals, curves and rounded corners come out stair-stepped.
//! These thin wrappers route the same primitives through GDI+ with
//! `SmoothingModeAntiAlias`, so shapes render smooth. Shared by the screenshot
//! annotation tools (freehand/arrows/shapes) and the Settings window chrome (the
//! toggle switches, checkbox glyphs, nav-rail icons and rounded buttons). GDI+ ships
//! in every Windows (`gdiplus.dll`) — no new crate, no size cost. Each entry point
//! (the overlay's message loop, the Settings `WM_CREATE`) calls [`startup`]/[`shutdown`]
//! around its lifetime — GDI+ must be initialised on the thread before any `Gdip*` call.

use windows::Win32::Foundation::COLORREF;
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::Graphics::GdiPlus::{
    FillMode, GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath,
    GdipCreatePen1, GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath,
    GdipDeletePen, GdipDrawEllipseI, GdipDrawLineI, GdipDrawLinesI, GdipDrawPath,
    GdipDrawRectangleI, GdipFillEllipseI, GdipFillPath, GdipFillRectangleI, GdipSetPenEndCap,
    GdipSetPenLineJoin, GdipSetPenStartCap, GdipSetPixelOffsetMode, GdipSetSmoothingMode,
    GdiplusShutdown, GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput, GpBrush,
    GpGraphics, GpPath, GpPen, GpSolidFill, LineCap, LineJoin, PixelOffsetMode, Point,
    SmoothingMode, Unit,
};

/// Initialise GDI+ for this thread; returns the token to pass to [`shutdown`].
pub(crate) unsafe fn startup() -> usize {
    let mut token: usize = 0;
    let input = GdiplusStartupInput {
        GdiplusVersion: 1,
        ..Default::default()
    };
    let mut output = GdiplusStartupOutput::default();
    let _ = GdiplusStartup(&mut token, &input, &mut output);
    token
}

pub(crate) unsafe fn shutdown(token: usize) {
    GdiplusShutdown(token);
}

/// Win32 `COLORREF` (0x00BBGGRR) → opaque GDI+ ARGB (0xAARRGGBB).
fn argb(c: COLORREF) -> u32 {
    let v = c.0;
    let (r, g, b) = (v & 0xFF, (v >> 8) & 0xFF, (v >> 16) & 0xFF);
    0xFF00_0000 | (r << 16) | (g << 8) | b
}

/// Run `f` with an anti-aliased GDI+ graphics over `hdc`. The graphics is deleted on
/// return (which flushes its queued drawing to the DC), so GDI+ output lands in the
/// right z-order relative to any surrounding plain-GDI calls.
pub(crate) unsafe fn with_aa(hdc: HDC, f: impl FnOnce(*mut GpGraphics)) {
    let mut g: *mut GpGraphics = core::ptr::null_mut();
    if GdipCreateFromHDC(hdc, &mut g).0 != 0 || g.is_null() {
        return;
    }
    let _ = GdipSetSmoothingMode(g, SmoothingMode(4)); // SmoothingModeAntiAlias
    let _ = GdipSetPixelOffsetMode(g, PixelOffsetMode(4)); // PixelOffsetModeHalf
    f(g);
    let _ = GdipDeleteGraphics(g);
}

/// A solid pen of `color` and pixel width `w`. Free with [`drop_pen`].
pub(crate) unsafe fn pen(color: COLORREF, w: i32) -> *mut GpPen {
    let mut p: *mut GpPen = core::ptr::null_mut();
    let _ = GdipCreatePen1(argb(color), w.max(1) as f32, Unit(2), &mut p); // UnitPixel
    p
}
/// A solid pen with ROUND end-caps and round joins — for line icons and the checkmark,
/// so stroked strokes end in soft dots and corners don't spike (a Fluent line-icon look).
pub(crate) unsafe fn pen_round(color: COLORREF, w: i32) -> *mut GpPen {
    let p = pen(color, w);
    if !p.is_null() {
        let _ = GdipSetPenStartCap(p, LineCap(2)); // LineCapRound
        let _ = GdipSetPenEndCap(p, LineCap(2));
        let _ = GdipSetPenLineJoin(p, LineJoin(2)); // LineJoinRound
    }
    p
}
pub(crate) unsafe fn drop_pen(p: *mut GpPen) {
    let _ = GdipDeletePen(p);
}

/// A solid fill brush of `color`. Free with [`drop_brush`].
pub(crate) unsafe fn brush(color: COLORREF) -> *mut GpBrush {
    let mut b: *mut GpSolidFill = core::ptr::null_mut();
    let _ = GdipCreateSolidFill(argb(color), &mut b);
    b as *mut GpBrush
}
pub(crate) unsafe fn drop_brush(b: *mut GpBrush) {
    let _ = GdipDeleteBrush(b);
}

// ---- drawing (all take GDI+ integer coordinates; x,y = top-left, w,h = extent) ----

pub(crate) unsafe fn line(g: *mut GpGraphics, p: *mut GpPen, x1: i32, y1: i32, x2: i32, y2: i32) {
    let _ = GdipDrawLineI(g, p, x1, y1, x2, y2);
}
pub(crate) unsafe fn rect(g: *mut GpGraphics, p: *mut GpPen, x: i32, y: i32, w: i32, h: i32) {
    let _ = GdipDrawRectangleI(g, p, x, y, w, h);
}
pub(crate) unsafe fn ellipse(g: *mut GpGraphics, p: *mut GpPen, x: i32, y: i32, w: i32, h: i32) {
    let _ = GdipDrawEllipseI(g, p, x, y, w, h);
}
pub(crate) unsafe fn fill_rect(
    g: *mut GpGraphics,
    b: *mut GpBrush,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    let _ = GdipFillRectangleI(g, b, x, y, w, h);
}
pub(crate) unsafe fn fill_ellipse(
    g: *mut GpGraphics,
    b: *mut GpBrush,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
) {
    let _ = GdipFillEllipseI(g, b, x, y, w, h);
}
/// Connected line segments (a polyline) through `pts`.
pub(crate) unsafe fn polyline(g: *mut GpGraphics, p: *mut GpPen, pts: &[(i32, i32)]) {
    if pts.len() < 2 {
        return;
    }
    let gp: Vec<Point> = pts.iter().map(|&(x, y)| Point { X: x, Y: y }).collect();
    let _ = GdipDrawLinesI(g, p, gp.as_ptr(), gp.len() as i32);
}

/// A rounded-rectangle path (4 corner arcs). Caller deletes via [`GdipDeletePath`].
unsafe fn round_path(x: i32, y: i32, w: i32, h: i32, r: i32) -> *mut GpPath {
    let mut path: *mut GpPath = core::ptr::null_mut();
    let _ = GdipCreatePath(FillMode(1), &mut path); // FillModeWinding
    let (xf, yf, wf, hf) = (x as f32, y as f32, w as f32, h as f32);
    let d = (r * 2).min(w).min(h) as f32; // corner diameter, clamped to the rect
    let _ = GdipAddPathArc(path, xf, yf, d, d, 180.0, 90.0); // top-left
    let _ = GdipAddPathArc(path, xf + wf - d, yf, d, d, 270.0, 90.0); // top-right
    let _ = GdipAddPathArc(path, xf + wf - d, yf + hf - d, d, d, 0.0, 90.0); // bottom-right
    let _ = GdipAddPathArc(path, xf, yf + hf - d, d, d, 90.0, 90.0); // bottom-left
    let _ = GdipClosePathFigure(path);
    path
}

/// Fill an anti-aliased rounded rectangle (corner radius `r`).
pub(crate) unsafe fn fill_round(
    g: *mut GpGraphics,
    b: *mut GpBrush,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
) {
    let p = round_path(x, y, w, h, r);
    let _ = GdipFillPath(g, b, p);
    let _ = GdipDeletePath(p);
}

/// Stroke an anti-aliased rounded rectangle outline.
pub(crate) unsafe fn stroke_round(
    g: *mut GpGraphics,
    pen: *mut GpPen,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    r: i32,
) {
    let p = round_path(x, y, w, h, r);
    let _ = GdipDrawPath(g, pen, p);
    let _ = GdipDeletePath(p);
}

/// The **OCR** mark: a viewfinder of four corner brackets around two text lines, sized off
/// `em` — the device-pixel em of the icon font the surrounding buttons draw with. Shared by
/// the screenshot editor's action bar and the Quick preview's caption toolbar so the same
/// feature carries the same icon in both places.
///
/// Drawn as a vector rather than a Segoe Fluent glyph because the icon font has no
/// unambiguous OCR codepoint, and a wrong one renders as a tofu box. The corners are left
/// OPEN on purpose — a closed rectangle reads as the screenshot editor's Rect tool.
///
/// **It is sized off the FONT, not off the button, and that is the whole point.** Scaling it
/// to the button cell is what made it the odd one out: the preview's caption buttons are
/// 38×36 against the screenshot bar's 28, so the same mark came out at 22×20 device px in a
/// row of 9–14 px font glyphs — measured off a real capture, and exactly the "the icons are
/// different sizes" the toolbar was reported for.
///
/// `cell = 7/4 em` is the screenshot bar's own proportion (a 28 px cell against its 16 px icon
/// font) turned into a rule, so THAT toolbar renders byte-identically and only the preview
/// moves. It lands the mark's height on `0.875 em` — alongside the `0.833 em` the bundled
/// font's glyphs are normalized to (`scripts/build-icon-font.py`, `NORM_TARGET` / 960 upm).
/// Matching the mark's HEIGHT rather than its width is deliberate: it is a wide viewfinder, so
/// equalizing widths shrinks it until the corner brackets and the two text lines inside them
/// collapse into a smudge (tried, measured at 14×12, and plainly worse than the bug).
///
/// Every segment of this mark is axis-aligned, so it is drawn as FILLED RECTANGLES rather than
/// stroked lines. A 1 px anti-aliased stroke spreads its coverage across two pixel rows and
/// never fully lands on either: measured against a real capture, the stroked version reached a
/// peak ink value of 182 with not one fully-covered pixel, while the font glyphs on the same
/// toolbar peak at 232 — so the button read as greyed-out next to its neighbours. Filled rects
/// on integer bounds are exact, and cost nothing in a shape with no diagonals or curves.
pub(crate) unsafe fn ocr_glyph(
    hdc: HDC,
    r: windows::Win32::Foundation::RECT,
    ink: COLORREF,
    em: i32,
) {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    // `em` already carries the DPI (callers pass a dpi-scaled font height), so the mark scales
    // with the toolbar without consulting the DPI a second time.
    let cell = (em * 7 / 4).max(8);
    let s = |v: i32| (v * cell / 28).max(1);
    let (hw, hh, arm, wt) = (s(8), s(7), s(5), s(1));
    let (left, right, top, bottom) = (cx - hw, cx + hw, cy - hh, cy + hh);
    with_aa(hdc, |g| {
        let b = brush(ink);
        // Four corner brackets, each an L of two bars drawn INSIDE the frame's bounds so the
        // mark's real extent is exactly 2*hw by 2*hh whatever the stroke weight rounds to.
        for &(x_out, x_in) in &[(left, left), (right - arm, right - wt)] {
            for &(y_out, y_in) in &[(top, top), (bottom - arm, bottom - wt)] {
                fill_rect(g, b, x_out, y_in, arm, wt); // horizontal arm
                fill_rect(g, b, x_in, y_out, wt, arm); // vertical arm
            }
        }
        // Two text lines inside it, the second short like the end of a paragraph. Their
        // SEPARATION is s(3) while their WEIGHT is s(2): deriving both from s(2) put a 1 px
        // line one pixel above another 1 px line at the size this now draws, and the pair
        // merged into a single smudge that read as a scribble rather than as text.
        let tw = s(2);
        fill_rect(g, b, cx - s(4), cy - s(3) - tw / 2, s(4) * 2, tw);
        fill_rect(g, b, cx - s(4), cy + s(3) - tw / 2, s(4) + s(1), tw);
        drop_brush(b);
    });
}
