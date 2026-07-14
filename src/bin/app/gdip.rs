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
    GdipAddPathArc, GdipClosePathFigure, GdipCreateFromHDC, GdipCreatePath, GdipCreatePen1,
    GdipCreateSolidFill, GdipDeleteBrush, GdipDeleteGraphics, GdipDeletePath, GdipDeletePen,
    GdipDrawEllipseI, GdipDrawLineI, GdipDrawLinesI, GdipDrawPath, GdipDrawRectangleI,
    GdipFillEllipseI, GdipFillPath, GdipFillRectangleI, GdipSetPenEndCap, GdipSetPenLineJoin,
    GdipSetPenStartCap, GdipSetPixelOffsetMode, GdipSetSmoothingMode, GdiplusShutdown,
    GdiplusStartup, GdiplusStartupInput, GdiplusStartupOutput, FillMode, GpBrush, GpGraphics,
    GpPath, GpPen, GpSolidFill, LineCap, LineJoin, PixelOffsetMode, Point, SmoothingMode, Unit,
};

/// Initialise GDI+ for this thread; returns the token to pass to [`shutdown`].
pub(crate) unsafe fn startup() -> usize {
    let mut token: usize = 0;
    let input = GdiplusStartupInput { GdiplusVersion: 1, ..Default::default() };
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
pub(crate) unsafe fn fill_rect(g: *mut GpGraphics, b: *mut GpBrush, x: i32, y: i32, w: i32, h: i32) {
    let _ = GdipFillRectangleI(g, b, x, y, w, h);
}
pub(crate) unsafe fn fill_ellipse(g: *mut GpGraphics, b: *mut GpBrush, x: i32, y: i32, w: i32, h: i32) {
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
pub(crate) unsafe fn fill_round(g: *mut GpGraphics, b: *mut GpBrush, x: i32, y: i32, w: i32, h: i32, r: i32) {
    let p = round_path(x, y, w, h, r);
    let _ = GdipFillPath(g, b, p);
    let _ = GdipDeletePath(p);
}

/// Stroke an anti-aliased rounded rectangle outline.
pub(crate) unsafe fn stroke_round(g: *mut GpGraphics, pen: *mut GpPen, x: i32, y: i32, w: i32, h: i32, r: i32) {
    let p = round_path(x, y, w, h, r);
    let _ = GdipDrawPath(g, pen, p);
    let _ = GdipDeletePath(p);
}
