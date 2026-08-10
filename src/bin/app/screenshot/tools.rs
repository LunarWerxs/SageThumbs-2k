//! The annotation tool set: the [`Tool`]/[`Shape`] model and the rendering for each.
//! No window or capture state lives here — `overlay.rs` owns that and calls these to
//! paint. Outline shapes (rect/ellipse/line/arrow/pen/number ring) draw through GDI+
//! ([`gdip`](crate::gdip)) so they're anti-aliased; the region effects (highlight/
//! pixelate/invert) and text stay on plain GDI (they're alpha/pixel/text ops, not
//! outlines). Every draw takes an `(ox, oy)` offset added to all coordinates: it's
//! `(0, 0)` for the live overlay, and `(-sel.left, -sel.top)` when `overlay::compose`
//! bakes the shapes into the cropped output. (We can't use `SetViewportOrgEx` for
//! this — GDI+ ignores the DC's viewport origin, only plain GDI honours it.)

use core::ffi::c_void;

use windows::Win32::Foundation::{COLORREF, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    AlphaBlend, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW, CreatePen,
    CreateSolidBrush, DeleteDC, DeleteObject, FillRect, GetObjectW, GetStockObject, PatBlt,
    Rectangle, SelectObject, SetBkMode, SetStretchBltMode, SetTextColor, StretchBlt, TextOutW,
    AC_SRC_OVER, BLENDFUNCTION, COLORONCOLOR, DSTINVERT, HDC, HGDIOBJ, LOGFONTW, NULL_BRUSH,
    PS_SOLID, SRCCOPY, TRANSPARENT,
};

use crate::dark::rgb;
use crate::win::{gui_font, wide};

use crate::gdip;

/// Which annotation the next drag/click creates.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Tool {
    Rect,
    Ellipse,
    Arrow,
    Line,
    Pen,
    Text,
    Number,
    Highlight,
    Pixelate,
    Invert,
    /// Sample the colour of a pixel in the frozen screenshot (copies its hex to the
    /// clipboard + sets the active colour). Doesn't draw — it's a pick-and-go tool.
    Eyedropper,
    /// Select an existing annotation and drag it (or Delete it). Doesn't draw.
    Move,
}

impl Tool {
    /// Short label for the hint strip.
    pub(super) fn label(self) -> &'static str {
        match self {
            Tool::Rect => "Rect",
            Tool::Ellipse => "Ellipse",
            Tool::Arrow => "Arrow",
            Tool::Line => "Line",
            Tool::Pen => "Pen",
            Tool::Text => "Text",
            Tool::Number => "Number",
            Tool::Highlight => "Highlight",
            Tool::Pixelate => "Pixelate",
            Tool::Invert => "Invert",
            Tool::Eyedropper => "Pick",
            Tool::Move => "Move",
        }
    }

    /// The tools offered as a STARTING tool, in the order the Settings dropdown lists them.
    ///
    /// Deliberately not every variant: `Eyedropper` and `Move` don't draw anything, so opening
    /// the editor already in one of them looks like the editor is broken. This array IS the
    /// stored format — a setting holds an INDEX into it — so only ever APPEND, never reorder
    /// or remove, or existing users silently get a different tool than the one they chose.
    pub(super) const DEFAULTABLE: [Tool; 10] = [
        Tool::Arrow,
        Tool::Rect,
        Tool::Ellipse,
        Tool::Line,
        Tool::Pen,
        Tool::Text,
        Tool::Number,
        Tool::Highlight,
        Tool::Pixelate,
        Tool::Invert,
    ];

    /// The starting tool for a stored setting index, falling back to the first entry
    /// ([`Tool::Arrow`]) for anything out of range — a hand-edited registry value can be
    /// anything at all, and an unusable editor is a worse answer than the default one.
    pub(super) fn from_default_index(index: u32) -> Tool {
        *Tool::DEFAULTABLE
            .get(index as usize)
            .unwrap_or(&Tool::DEFAULTABLE[0])
    }
}

/// The Settings dialog clamps its dropdown with `settings::SHOT_TOOL_COUNT`, because it
/// cannot see [`Tool::DEFAULTABLE`] (that array is `pub(super)` to this module). Adding a
/// tool here without updating that constant would silently drop it off the end of the
/// dropdown, so the two are welded together at COMPILE time rather than by a comment.
const _: () = assert!(
    Tool::DEFAULTABLE.len() == sagethumbs2k_core::settings::SHOT_TOOL_COUNT as usize,
    "Tool::DEFAULTABLE and settings::SHOT_TOOL_COUNT disagree"
);

/// Named for its subject rather than `tests`: this file already has a `mod tests` further
/// down, and two modules of the same name in one file do not compile.
#[cfg(test)]
mod default_tool_tests {
    use super::Tool;

    /// The stored setting is an INDEX into DEFAULTABLE, so index 0 must stay Arrow and the
    /// list must not be reordered under existing users. This test is the tripwire for that.
    #[test]
    fn default_tool_index_zero_is_arrow() {
        assert!(matches!(Tool::from_default_index(0), Tool::Arrow));
        assert!(matches!(Tool::DEFAULTABLE[1], Tool::Rect));
    }

    /// Anything out of range degrades to the default rather than panicking or leaving the
    /// editor in a tool that cannot draw.
    #[test]
    fn out_of_range_falls_back_to_arrow() {
        for i in [Tool::DEFAULTABLE.len() as u32, 99, u32::MAX] {
            assert!(matches!(Tool::from_default_index(i), Tool::Arrow));
        }
    }

    /// The non-drawing tools must never be offered as a starting tool.
    #[test]
    fn non_drawing_tools_are_not_offered() {
        for t in Tool::DEFAULTABLE {
            assert!(!matches!(t, Tool::Eyedropper | Tool::Move));
        }
    }
}

/// A placed annotation. Coordinates are in virtual-screen client space.
pub(super) enum Shape {
    Rect {
        r: RECT,
        color: COLORREF,
        w: i32,
    },
    Ellipse {
        r: RECT,
        color: COLORREF,
        w: i32,
    },
    Arrow {
        a: POINT,
        b: POINT,
        color: COLORREF,
        w: i32,
    },
    Line {
        a: POINT,
        b: POINT,
        color: COLORREF,
        w: i32,
    },
    Pen {
        pts: Vec<POINT>,
        color: COLORREF,
        w: i32,
    },
    Text {
        at: POINT,
        s: String,
        color: COLORREF,
        font: LOGFONTW,
    },
    Number {
        at: POINT,
        n: u32,
        color: COLORREF,
    },
    /// Translucent colour wash over a region (a marker/highlighter).
    Highlight {
        r: RECT,
        color: COLORREF,
    },
    /// Blockify the pixels under a region (hide sensitive content).
    Pixelate {
        r: RECT,
    },
    /// Invert the colours under a region.
    Invert {
        r: RECT,
    },
}

/// Colour palette cycled with `K` (Flameshot-ish defaults; red first).
pub(super) const PALETTE: &[(u8, u8, u8)] = &[
    (230, 40, 40),
    (40, 170, 60),
    (40, 120, 230),
    (240, 190, 30),
    (20, 20, 20),
    (250, 250, 250),
];

/// Normalize a rect so left<=right, top<=bottom (drags go any direction).
pub(super) fn norm(a: POINT, b: POINT) -> RECT {
    RECT {
        left: a.x.min(b.x),
        top: a.y.min(b.y),
        right: a.x.max(b.x),
        bottom: a.y.max(b.y),
    }
}

/// Constrain a line/arrow endpoint to the nearest 45-degree direction from `a`.
///
/// The raw drag distance is preserved, so pressing or releasing Shift changes only
/// the angle rather than making the annotation suddenly much longer or shorter.
/// Screen coordinates already map 1:1 to output pixels; no DPI scaling belongs here.
pub(super) fn snap_endpoint_45(a: POINT, b: POINT) -> POINT {
    let dx = b.x as f64 - a.x as f64;
    let dy = b.y as f64 - a.y as f64;
    let len = dx.hypot(dy);
    if len == 0.0 {
        return a;
    }

    let step = std::f64::consts::FRAC_PI_4;
    let angle = (dy.atan2(dx) / step).round() * step;
    POINT {
        x: (a.x as f64 + len * angle.cos()).round() as i32,
        y: (a.y as f64 + len * angle.sin()).round() as i32,
    }
}

/// The endpoint shown/stored for an active drag. Keeping this decision in one pure
/// helper prevents the live preview and the committed screenshot from disagreeing.
pub(super) fn drag_endpoint(tool: Tool, a: POINT, raw: POINT, shift: bool) -> POINT {
    if shift && matches!(tool, Tool::Line | Tool::Arrow) {
        snap_endpoint_45(a, raw)
    } else {
        raw
    }
}

// ---- anti-aliased outline primitives (GDI+) --------------------------------

/// Stroke a rect outline (`r` in client space, shifted by `ox,oy`).
unsafe fn outline_rect(hdc: HDC, ox: i32, oy: i32, r: RECT, color: COLORREF, w: i32) {
    gdip::with_aa(hdc, |g| {
        let p = gdip::pen(color, w);
        gdip::rect(
            g,
            p,
            r.left + ox,
            r.top + oy,
            r.right - r.left,
            r.bottom - r.top,
        );
        gdip::drop_pen(p);
    });
}

/// Stroke an ellipse outline inscribed in `r`.
unsafe fn outline_ellipse(hdc: HDC, ox: i32, oy: i32, r: RECT, color: COLORREF, w: i32) {
    gdip::with_aa(hdc, |g| {
        let p = gdip::pen(color, w);
        gdip::ellipse(
            g,
            p,
            r.left + ox,
            r.top + oy,
            r.right - r.left,
            r.bottom - r.top,
        );
        gdip::drop_pen(p);
    });
}

/// Stroke a straight line `a`→`b`.
unsafe fn outline_line(hdc: HDC, ox: i32, oy: i32, a: POINT, b: POINT, color: COLORREF, w: i32) {
    gdip::with_aa(hdc, |g| {
        let p = gdip::pen(color, w);
        gdip::line(g, p, a.x + ox, a.y + oy, b.x + ox, b.y + oy);
        gdip::drop_pen(p);
    });
}

/// Draw a committed [`Shape`], all coordinates shifted by `(ox, oy)`.
pub(super) unsafe fn draw_shape(hdc: HDC, ox: i32, oy: i32, sh: &Shape) {
    match sh {
        Shape::Rect { r, color, w } => outline_rect(hdc, ox, oy, *r, *color, *w),
        Shape::Ellipse { r, color, w } => outline_ellipse(hdc, ox, oy, *r, *color, *w),
        Shape::Line { a, b, color, w } => outline_line(hdc, ox, oy, *a, *b, *color, *w),
        Shape::Arrow { a, b, color, w } => draw_arrow(hdc, ox, oy, *a, *b, *color, *w),
        Shape::Pen { pts, color, w } => draw_pen(hdc, ox, oy, pts, *color, *w),
        Shape::Text { at, s, color, font } => draw_text(hdc, ox, oy, *at, s, *color, font, false),
        Shape::Number { at, n, color } => draw_number(hdc, ox, oy, *at, *n, *color),
        Shape::Highlight { r, color } => draw_highlight(hdc, ox, oy, *r, *color),
        Shape::Pixelate { r } => draw_pixelate(hdc, ox, oy, *r),
        Shape::Invert { r } => draw_invert(hdc, ox, oy, *r),
    }
}

/// Translucent colour wash (~43% alpha) over `r` — the highlighter/marker. Fills a
/// scratch DC with the colour and AlphaBlends it onto `hdc`.
unsafe fn draw_highlight(hdc: HDC, ox: i32, oy: i32, r: RECT, color: COLORREF) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w <= 0 || h <= 0 {
        return;
    }
    let tmp = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, w, h);
    let old = SelectObject(tmp, HGDIOBJ(bmp.0));
    let br = CreateSolidBrush(color);
    FillRect(
        tmp,
        &RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        },
        br,
    );
    let _ = DeleteObject(br.into());
    let bf = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 110,
        AlphaFormat: 0,
    };
    let _ = AlphaBlend(hdc, r.left + ox, r.top + oy, w, h, tmp, 0, 0, w, h, bf);
    SelectObject(tmp, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(tmp);
}

/// Blockify `r`: shrink it into a tiny DC and stretch it back (nearest-neighbour),
/// so the region becomes coarse blocks — hides faces/text without storing pixels.
unsafe fn draw_pixelate(hdc: HDC, ox: i32, oy: i32, r: RECT) {
    let (w, h) = (r.right - r.left, r.bottom - r.top);
    if w < 4 || h < 4 {
        return;
    }
    let (x, y) = (r.left + ox, r.top + oy);
    let block = 9;
    let (sw, sh) = ((w / block).max(1), (h / block).max(1));
    let tmp = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, sw, sh);
    let old = SelectObject(tmp, HGDIOBJ(bmp.0));
    SetStretchBltMode(hdc, COLORONCOLOR);
    SetStretchBltMode(tmp, COLORONCOLOR);
    // Down-sample the region, then blow it back up blocky.
    let _ = StretchBlt(tmp, 0, 0, sw, sh, Some(hdc), x, y, w, h, SRCCOPY);
    let _ = StretchBlt(hdc, x, y, w, h, Some(tmp), 0, 0, sw, sh, SRCCOPY);
    SelectObject(tmp, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(tmp);
}

/// Invert the colours under `r` (one `PatBlt` with `DSTINVERT`).
unsafe fn draw_invert(hdc: HDC, ox: i32, oy: i32, r: RECT) {
    let _ = PatBlt(
        hdc,
        r.left + ox,
        r.top + oy,
        r.right - r.left,
        r.bottom - r.top,
        DSTINVERT,
    );
}

/// Draw the in-progress shape for the live drag (the tool defines the kind). Only
/// called on the overlay, so the offset is always `(0, 0)` — kept explicit for
/// symmetry with [`draw_shape`].
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(super) unsafe fn draw_inprogress(
    hdc: HDC,
    ox: i32,
    oy: i32,
    tool: Tool,
    a: POINT,
    b: POINT,
    color: COLORREF,
    w: i32,
    pen_pts: &[POINT],
) {
    match tool {
        Tool::Rect => outline_rect(hdc, ox, oy, norm(a, b), color, w),
        Tool::Ellipse => outline_ellipse(hdc, ox, oy, norm(a, b), color, w),
        Tool::Line => outline_line(hdc, ox, oy, a, b, color, w),
        Tool::Arrow => draw_arrow(hdc, ox, oy, a, b, color, w),
        Tool::Pen => draw_pen(hdc, ox, oy, pen_pts, color, w),
        // Region-effect tools: preview just the area outline; the effect applies on
        // release (a live effect would flicker as the drag reads changing pixels).
        Tool::Highlight | Tool::Pixelate | Tool::Invert => {
            outline_rect(hdc, ox, oy, norm(a, b), color, 1)
        }
        Tool::Text | Tool::Number | Tool::Eyedropper | Tool::Move => {}
    }
}

/// Freehand polyline through the captured points.
unsafe fn draw_pen(hdc: HDC, ox: i32, oy: i32, pts: &[POINT], color: COLORREF, w: i32) {
    if pts.len() < 2 {
        return;
    }
    let pl: Vec<(i32, i32)> = pts.iter().map(|q| (q.x + ox, q.y + oy)).collect();
    gdip::with_aa(hdc, |g| {
        let p = gdip::pen(color, w);
        gdip::polyline(g, p, &pl);
        gdip::drop_pen(p);
    });
}

unsafe fn draw_arrow(hdc: HDC, ox: i32, oy: i32, a: POINT, b: POINT, color: COLORREF, w: i32) {
    let (ax, ay, bx, by) = (a.x + ox, a.y + oy, b.x + ox, b.y + oy);
    gdip::with_aa(hdc, |g| {
        let p = gdip::pen(color, w);
        gdip::line(g, p, ax, ay, bx, by);
        // Arrowhead: two short segments back from the tip.
        let dx = (bx - ax) as f64;
        let dy = (by - ay) as f64;
        let len = (dx * dx + dy * dy).sqrt().max(1.0);
        let (ux, uy) = (dx / len, dy / len);
        let head = (10 + w * 2) as f64;
        let ang = 0.5_f64; // ~28°
        let (ca, sa) = (ang.cos(), ang.sin());
        for sgn in [1.0_f64, -1.0] {
            let rx = -ux * ca + sgn * -uy * sa;
            let ry = -uy * ca - sgn * -ux * sa;
            let hx = bx + (rx * head) as i32;
            let hy = by + (ry * head) as i32;
            gdip::line(g, p, bx, by, hx, hy);
        }
        gdip::drop_pen(p);
    });
}

/// The system UI font as a [`LOGFONTW`] template, scaled to pixel height `size`.
/// Used as the default for the Text tool and re-tuned by the native Font dialog.
pub(super) unsafe fn default_text_font(size: i32) -> LOGFONTW {
    let mut lf = LOGFONTW::default();
    let got = GetObjectW(
        HGDIOBJ(gui_font().0),
        core::mem::size_of::<LOGFONTW>() as i32,
        Some(&mut lf as *mut _ as *mut c_void),
    );
    if got == 0 {
        // Couldn't read the UI font — fall back to a sane face.
        let face = wide("Segoe UI");
        for (i, c) in face.iter().take(lf.lfFaceName.len() - 1).enumerate() {
            lf.lfFaceName[i] = *c;
        }
    }
    lf.lfHeight = -size.max(8);
    lf.lfWidth = 0;
    lf
}

/// The face name held in a [`LOGFONTW`] (up to the NUL).
pub(super) fn face_name(lf: &LOGFONTW) -> String {
    let end = lf
        .lfFaceName
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(lf.lfFaceName.len());
    String::from_utf16_lossy(&lf.lfFaceName[..end])
}

/// Set the face name of a [`LOGFONTW`] (truncated to fit the 32-wchar field).
pub(super) fn set_face(lf: &mut LOGFONTW, name: &str) {
    lf.lfFaceName = [0; 32];
    let w = wide(name);
    for (i, c) in w.iter().take(lf.lfFaceName.len() - 1).enumerate() {
        lf.lfFaceName[i] = *c;
    }
}

/// Draw text at `at` in `font`; `caret` appends a `_` for the live typing preview.
#[allow(clippy::too_many_arguments)] // owner-draw helper: many positional draw params by nature
pub(super) unsafe fn draw_text(
    hdc: HDC,
    ox: i32,
    oy: i32,
    at: POINT,
    s: &str,
    color: COLORREF,
    font: &LOGFONTW,
    caret: bool,
) {
    let hf = CreateFontIndirectW(font);
    let oldf = SelectObject(hdc, HGDIOBJ(hf.0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, color);
    let shown = if caret {
        format!("{s}_")
    } else {
        s.to_string()
    };
    let w = wide(&shown);
    let _ = TextOutW(hdc, at.x + ox, at.y + oy, &w[..w.len().saturating_sub(1)]);
    SelectObject(hdc, oldf);
    let _ = DeleteObject(HGDIOBJ(hf.0));
}

unsafe fn draw_number(hdc: HDC, ox: i32, oy: i32, at: POINT, n: u32, color: COLORREF) {
    let r = 13;
    let (cx, cy) = (at.x + ox, at.y + oy);
    gdip::with_aa(hdc, |g| {
        let b = gdip::brush(color);
        gdip::fill_ellipse(g, b, cx - r, cy - r, 2 * r, 2 * r);
        gdip::drop_brush(b);
        let p = gdip::pen(rgb(255, 255, 255), 1);
        gdip::ellipse(g, p, cx - r, cy - r, 2 * r, 2 * r);
        gdip::drop_pen(p);
    });
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(255, 255, 255));
    let s = n.to_string();
    let label = wide(&s);
    let half = 4 * s.len() as i32;
    let _ = TextOutW(
        hdc,
        cx - half,
        cy - 8,
        &label[..label.len().saturating_sub(1)],
    );
}

/// A rectangle outline of `color`/`w` (the selection / move-grab frame). Stays on
/// plain GDI for a crisp 1px UI line; only ever drawn on the overlay (no offset).
pub(super) unsafe fn frame(hdc: HDC, r: RECT, color: COLORREF, w: i32) {
    let pen = CreatePen(PS_SOLID, w.max(1), color);
    let oldp = SelectObject(hdc, HGDIOBJ(pen.0));
    let oldb = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    let _ = Rectangle(hdc, r.left, r.top, r.right, r.bottom);
    SelectObject(hdc, oldp);
    SelectObject(hdc, oldb);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

// ---- Move/select support (the Move tool) -----------------------------------

/// Bounding box of a shape — used to hit-test a click and to draw the selection
/// frame. (Text is approximate; that's fine for grabbing.)
pub(super) fn shape_bbox(sh: &Shape) -> RECT {
    match sh {
        Shape::Rect { r, .. }
        | Shape::Ellipse { r, .. }
        | Shape::Highlight { r, .. }
        | Shape::Pixelate { r }
        | Shape::Invert { r } => *r,
        Shape::Arrow { a, b, .. } | Shape::Line { a, b, .. } => norm(*a, *b),
        Shape::Pen { pts, .. } => {
            let mut r = RECT {
                left: i32::MAX,
                top: i32::MAX,
                right: i32::MIN,
                bottom: i32::MIN,
            };
            for q in pts {
                r.left = r.left.min(q.x);
                r.top = r.top.min(q.y);
                r.right = r.right.max(q.x);
                r.bottom = r.bottom.max(q.y);
            }
            if pts.is_empty() {
                RECT::default()
            } else {
                r
            }
        }
        Shape::Text { at, s, font, .. } => {
            let size = (-font.lfHeight).max(8);
            RECT {
                left: at.x,
                top: at.y,
                right: at.x + (size * 6 / 10) * s.len().max(1) as i32,
                bottom: at.y + size * 13 / 10,
            }
        }
        Shape::Number { at, .. } => RECT {
            left: at.x - 13,
            top: at.y - 13,
            right: at.x + 13,
            bottom: at.y + 13,
        },
    }
}

/// Shift a shape by `(dx, dy)`.
pub(super) fn translate_shape(sh: &mut Shape, dx: i32, dy: i32) {
    match sh {
        Shape::Rect { r, .. }
        | Shape::Ellipse { r, .. }
        | Shape::Highlight { r, .. }
        | Shape::Pixelate { r }
        | Shape::Invert { r } => {
            r.left += dx;
            r.right += dx;
            r.top += dy;
            r.bottom += dy;
        }
        Shape::Arrow { a, b, .. } | Shape::Line { a, b, .. } => {
            a.x += dx;
            a.y += dy;
            b.x += dx;
            b.y += dy;
        }
        Shape::Pen { pts, .. } => {
            for q in pts {
                q.x += dx;
                q.y += dy;
            }
        }
        Shape::Text { at, .. } | Shape::Number { at, .. } => {
            at.x += dx;
            at.y += dy;
        }
    }
}

/// The topmost shape whose bbox (plus a small grab margin) contains `(x, y)`.
/// Newest-first so the visually-on-top annotation wins.
pub(super) fn hit_shape(shapes: &[Shape], x: i32, y: i32) -> Option<usize> {
    const M: i32 = 6;
    shapes
        .iter()
        .enumerate()
        .rev()
        .find(|(_, sh)| {
            let r = shape_bbox(sh);
            x >= r.left - M && x <= r.right + M && y >= r.top - M && y <= r.bottom + M
        })
        .map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    fn length(a: POINT, b: POINT) -> f64 {
        (b.x as f64 - a.x as f64).hypot(b.y as f64 - a.y as f64)
    }

    #[test]
    fn snap_45_keeps_zero_and_exact_eight_way_directions() {
        let a = point(37, -19);
        assert_eq!(snap_endpoint_45(a, a), a);

        for (dx, dy) in [
            (20, 0),
            (20, 20),
            (0, 20),
            (-20, 20),
            (-20, 0),
            (-20, -20),
            (0, -20),
            (20, -20),
        ] {
            let b = point(a.x + dx, a.y + dy);
            assert_eq!(snap_endpoint_45(a, b), b);
        }
    }

    #[test]
    fn snap_45_chooses_the_nearest_octant_in_every_quadrant() {
        let a = point(100, 100);
        for raw in [
            point(140, 112),
            point(140, 88),
            point(60, 112),
            point(60, 88),
        ] {
            let b = snap_endpoint_45(a, raw);
            assert_eq!(b.y, a.y, "{raw:?} should snap horizontally");
        }
        for raw in [
            point(112, 140),
            point(88, 140),
            point(112, 60),
            point(88, 60),
        ] {
            let b = snap_endpoint_45(a, raw);
            assert_eq!(b.x, a.x, "{raw:?} should snap vertically");
        }
        for raw in [
            point(130, 125),
            point(70, 125),
            point(70, 75),
            point(130, 75),
        ] {
            let b = snap_endpoint_45(a, raw);
            assert_eq!(
                (b.x - a.x).abs(),
                (b.y - a.y).abs(),
                "{raw:?} should snap diagonally"
            );
        }
    }

    #[test]
    fn snap_45_switches_at_the_half_octant_boundaries() {
        let a = point(0, 0);

        // tan(22.5°) is about 0.41421356: one side is horizontal,
        // the other is diagonal.
        let below_22 = snap_endpoint_45(a, point(1000, 414));
        let above_22 = snap_endpoint_45(a, point(1000, 415));
        assert_eq!(below_22.y, 0);
        assert_eq!(above_22.x.abs(), above_22.y.abs());

        // The matching 67.5° boundary separates diagonal from vertical.
        let below_67 = snap_endpoint_45(a, point(415, 1000));
        let above_67 = snap_endpoint_45(a, point(414, 1000));
        assert_eq!(below_67.x.abs(), below_67.y.abs());
        assert_eq!(above_67.x, 0);
    }

    #[test]
    fn snap_45_preserves_drag_length_to_rounding_tolerance() {
        let a = point(-73, 211);
        for raw in [
            point(492, 329),
            point(-188, 804),
            point(-601, -97),
            point(171, -380),
        ] {
            let snapped = snap_endpoint_45(a, raw);
            assert!(
                (length(a, raw) - length(a, snapped)).abs() <= 1.0,
                "{raw:?} -> {snapped:?} changed drag length too much"
            );
        }
    }

    #[test]
    fn drag_endpoint_only_constrains_shifted_lines_and_arrows() {
        let a = point(20, 30);
        let raw = point(120, 58);
        let snapped = snap_endpoint_45(a, raw);

        assert_eq!(drag_endpoint(Tool::Line, a, raw, true), snapped);
        assert_eq!(drag_endpoint(Tool::Arrow, a, raw, true), snapped);
        assert_eq!(drag_endpoint(Tool::Line, a, raw, false), raw);
        assert_eq!(drag_endpoint(Tool::Rect, a, raw, true), raw);
        assert_eq!(drag_endpoint(Tool::Pen, a, raw, true), raw);
    }
}
