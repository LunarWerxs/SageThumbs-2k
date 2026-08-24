//! The floating action bar shown under the selection once a region is chosen.
//!
//! Owner-drawn and hit-tested by `overlay.rs` (no child windows — it's painted
//! straight onto the fullscreen overlay), so it stays part of the same GDI surface.
//! Buttons either pick a [`Tool`] or run an action; the overlay maps the clicked
//! [`Button`] to the right effect.

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, FrameRect,
    SelectObject, SetBkMode, SetTextColor, DT_CALCRECT, DT_CENTER, DT_LEFT, DT_SINGLELINE,
    DT_VCENTER, HDC, HFONT, HGDIOBJ, LOGFONTW, TRANSPARENT,
};

use crate::dark::rgb;
use crate::win::{dpi_scale_dpi, gui_font, wide};

use super::tools::{face_name, Tool};
use crate::gdip;

mod colors;
mod textflyout;

// Parent-hub import model: the flyouts are separate painted surfaces with their own
// layout + hit models, so they live in their own files; re-export the names the
// overlay reaches through `toolbar::` by name.
pub(super) use colors::{color_flyout_layout, draw_color_flyout, Swatch};
pub(super) use textflyout::{draw_text_flyout, text_flyout_layout, TextItem, PRESET_FONTS};

/// A toolbar item. `Sep` is a non-clickable divider between groups.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Button {
    Tool(Tool),
    Color,
    Undo,
    Redo,
    Copy,
    /// Read the text out of the region (OCR) instead of copying the pixels.
    Ocr,
    Save,
    Upload,
    Close,
    Sep,
}

const CELL: i32 = 28; // square icon-button size
const H: i32 = CELL; // button height = width (square cells)
const GAP: i32 = 3;
const PAD: i32 = 5; // bar padding around the buttons
const MARGIN: i32 = 8; // gap between selection and bar
const SEPW: i32 = 11; // separator gap width

/// The ordered toolbar items, grouped by `Sep` dividers: draw tools · text/number ·
/// region effects · colour · move + actions. Every button is a square icon.
fn items() -> [(Button, i32); 24] {
    [
        (Button::Tool(Tool::Rect), CELL),
        (Button::Tool(Tool::Ellipse), CELL),
        (Button::Tool(Tool::Arrow), CELL),
        (Button::Tool(Tool::Line), CELL),
        (Button::Tool(Tool::Pen), CELL),
        (Button::Sep, SEPW),
        (Button::Tool(Tool::Text), CELL),
        (Button::Tool(Tool::Number), CELL),
        (Button::Sep, SEPW),
        (Button::Tool(Tool::Highlight), CELL),
        (Button::Tool(Tool::Pixelate), CELL),
        (Button::Tool(Tool::Invert), CELL),
        (Button::Sep, SEPW),
        (Button::Color, CELL),
        (Button::Tool(Tool::Eyedropper), CELL),
        (Button::Sep, SEPW),
        (Button::Tool(Tool::Move), CELL),
        (Button::Undo, CELL),
        (Button::Redo, CELL),
        (Button::Copy, CELL),
        (Button::Ocr, CELL),
        (Button::Save, CELL),
        (Button::Upload, CELL),
        (Button::Close, CELL),
    ]
}

/// Total inner width of all buttons + gaps, scaled to `dpi`.
fn inner_width(dpi: i32) -> i32 {
    let it = items();
    it.iter().map(|(_, w)| dpi_scale_dpi(*w, dpi)).sum::<i32>()
        + dpi_scale_dpi(GAP, dpi) * (it.len() as i32 - 1)
}

/// Lay the bar out under `sel` (or above it if there's no room below), clamped to
/// the virtual screen. `dpi` scales the design pixels (identity at 96). Returns each
/// button with its absolute rect.
pub(super) fn layout(sel: RECT, vw: i32, vh: i32, dpi: i32) -> Vec<(Button, RECT)> {
    let pad = dpi_scale_dpi(PAD, dpi);
    let gap = dpi_scale_dpi(GAP, dpi);
    let margin = dpi_scale_dpi(MARGIN, dpi);
    let h = dpi_scale_dpi(H, dpi);
    let bar_w = inner_width(dpi) + pad * 2;
    let bar_h = h + pad * 2;
    let mut x = sel.left;
    if x + bar_w > vw {
        x = vw - bar_w;
    }
    x = x.max(0);
    let mut y = sel.bottom + margin;
    if y + bar_h > vh {
        y = (sel.top - margin - bar_h).max(0); // not enough room below → above
    }

    let mut out = Vec::with_capacity(24);
    let mut bx = x + pad;
    let by = y + pad;
    for (btn, w) in items() {
        let w = dpi_scale_dpi(w, dpi);
        out.push((
            btn,
            RECT {
                left: bx,
                top: by,
                right: bx + w,
                bottom: by + h,
            },
        ));
        bx += w + gap;
    }
    out
}

/// The bar's background rect (so the overlay can paint a backdrop behind buttons).
/// `dpi` scales the padding around the buttons (identity at 96).
fn bar_rect(buttons: &[(Button, RECT)], dpi: i32) -> RECT {
    let pad = dpi_scale_dpi(PAD, dpi);
    let first = buttons.first().map(|(_, r)| *r).unwrap_or_default();
    let last = buttons.last().map(|(_, r)| *r).unwrap_or_default();
    RECT {
        left: first.left - pad,
        top: first.top - pad,
        right: last.right + pad,
        bottom: last.bottom + pad,
    }
}

/// Which button (if any) is under `(x, y)`. Separators are not clickable.
pub(super) fn hit(buttons: &[(Button, RECT)], x: i32, y: i32) -> Option<Button> {
    buttons
        .iter()
        .find(|(b, r)| {
            !matches!(b, Button::Sep) && x >= r.left && x < r.right && y >= r.top && y < r.bottom
        })
        .map(|(b, _)| *b)
}

/// One-line description of a button, shown as a hover tooltip.
pub(super) fn button_tip(btn: Button) -> &'static str {
    match btn {
        Button::Tool(Tool::Rect) => "Rectangle (R) — drag to draw",
        Button::Tool(Tool::Ellipse) => "Ellipse (O) — drag to draw",
        Button::Tool(Tool::Arrow) => "Arrow (A) — drag tail to head · Shift snaps 45°",
        Button::Tool(Tool::Line) => "Line (L) — drag to draw · Shift snaps 45°",
        Button::Tool(Tool::Pen) => "Pen (P) — freehand draw",
        Button::Tool(Tool::Text) => "Text (T) — click then type · [ ] resize",
        Button::Tool(Tool::Number) => "Number (N) — click to drop 1, 2, 3…",
        Button::Tool(Tool::Highlight) => "Highlight (H) — translucent marker",
        Button::Tool(Tool::Pixelate) => "Pixelate (B) — blur/blockify a region",
        Button::Tool(Tool::Invert) => "Invert (I) — invert a region's colours",
        Button::Tool(Tool::Eyedropper) => "Pick colour (E) — click a pixel to copy its hex",
        Button::Tool(Tool::Move) => "Move (M) — drag a shape · Del removes · or Ctrl-drag",
        Button::Color => "Colour (K) — cycle the palette",
        Button::Undo => "Undo (Ctrl+Z)",
        Button::Redo => "Redo (Ctrl+Y / Ctrl+Shift+Z)",
        Button::Copy => "Copy to the clipboard (Ctrl+C / Enter)",
        Button::Ocr => "Copy text (OCR) (Ctrl+T) — read the words in the region",
        Button::Save => "Save a PNG (Ctrl+S)",
        Button::Upload => "Upload & copy the link",
        Button::Close => "Close (Esc)",
        Button::Sep => "",
    }
}

/// Draw a light tooltip bubble for `text`, anchored below the button rect `anchor`
/// (flipped above / clamped so it stays on the virtual screen `vw`×`vh`). `dpi`
/// scales the design pixels (identity at 96).
pub(super) unsafe fn draw_tooltip(hdc: HDC, anchor: RECT, text: &str, vw: i32, vh: i32, dpi: i32) {
    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    let mut w = wide(text);
    let n = w.len().saturating_sub(1);
    // Measure the text.
    let mut calc = RECT::default();
    DrawTextW(
        hdc,
        &mut w[..n],
        &mut calc,
        DT_CALCRECT | DT_SINGLELINE | DT_LEFT,
    );
    let pad = dpi_scale_dpi(6, dpi);
    let off = dpi_scale_dpi(6, dpi);
    let bw = (calc.right - calc.left) + pad * 2;
    let bh = (calc.bottom - calc.top) + pad * 2;
    // Position below the button, clamped; flip above if there's no room below.
    let mut x = anchor.left;
    if x + bw > vw {
        x = vw - bw;
    }
    x = x.max(0);
    let mut y = anchor.bottom + off;
    if y + bh > vh {
        y = (anchor.top - bh - off).max(0);
    }
    let r = RECT {
        left: x,
        top: y,
        right: x + bw,
        bottom: y + bh,
    };
    let bg = CreateSolidBrush(rgb(248, 248, 240));
    FillRect(hdc, &r, bg);
    let _ = DeleteObject(bg.into());
    let border = CreateSolidBrush(rgb(120, 120, 120));
    FrameRect(hdc, &r, border);
    let _ = DeleteObject(border.into());
    SetTextColor(hdc, rgb(20, 20, 20));
    let mut tr = RECT {
        left: x + pad,
        top: y + pad,
        right: x + bw,
        bottom: y + bh,
    };
    DrawTextW(hdc, &mut w[..n], &mut tr, DT_SINGLELINE | DT_LEFT);
}

/// Paint the bar: a rounded backdrop, rounded per-group icon cells, the group
/// dividers, then each button's icon (a Segoe Fluent glyph, an AA vector glyph, or
/// the colour swatch).
pub(super) unsafe fn draw(
    hdc: HDC,
    buttons: &[(Button, RECT)],
    active: Tool,
    color: COLORREF,
    dpi: i32,
) {
    let bar = bar_rect(buttons, dpi);

    // Rounded backdrop + every cell background, in one anti-aliased GDI+ pass.
    let r_bar = dpi_scale_dpi(9, dpi); // bar corner radius
    let r_cell = dpi_scale_dpi(6, dpi); // per-cell corner radius
    gdip::with_aa(hdc, |g| {
        let bg = gdip::brush(rgb(28, 28, 28));
        gdip::fill_round(
            g,
            bg,
            bar.left,
            bar.top,
            bar.right - bar.left,
            bar.bottom - bar.top,
            r_bar,
        );
        gdip::drop_brush(bg);
        let pen = gdip::pen(rgb(72, 72, 72), 1);
        gdip::stroke_round(
            g,
            pen,
            bar.left,
            bar.top,
            bar.right - bar.left,
            bar.bottom - bar.top,
            r_bar,
        );
        gdip::drop_pen(pen);
        for (btn, r) in buttons {
            if matches!(btn, Button::Sep) {
                continue;
            }
            let on = matches!(btn, Button::Tool(t) if *t == active);
            let cb = gdip::brush(if on {
                rgb(0, 120, 210)
            } else {
                rgb(54, 54, 54)
            });
            gdip::fill_round(
                g,
                cb,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                r_cell,
            );
            gdip::drop_brush(cb);
        }
    });

    // Group divider lines (between the rounded cells).
    let div_inset = dpi_scale_dpi(5, dpi);
    for (btn, r) in buttons {
        if let Button::Sep = btn {
            let cx = (r.left + r.right) / 2;
            let line = RECT {
                left: cx,
                top: r.top + div_inset,
                right: cx + 1,
                bottom: r.bottom - div_inset,
            };
            let b = CreateSolidBrush(rgb(78, 78, 78));
            FillRect(hdc, &line, b);
            let _ = DeleteObject(b.into());
        }
    }

    // Icons / colour swatch, on top of the cells.
    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    let icon = icon_font(dpi);
    let sw_inset = dpi_scale_dpi(4, dpi); // colour-swatch inset
    let sw_round = dpi_scale_dpi(4, dpi); // colour-swatch corner radius
    for (btn, r) in buttons {
        match btn {
            Button::Sep => {}
            Button::Color => {
                // A rounded swatch of the current colour, inset a little.
                gdip::with_aa(hdc, |g| {
                    let b = gdip::brush(color);
                    gdip::fill_round(
                        g,
                        b,
                        r.left + sw_inset,
                        r.top + sw_inset,
                        (r.right - r.left) - sw_inset * 2,
                        (r.bottom - r.top) - sw_inset * 2,
                        sw_round,
                    );
                    gdip::drop_brush(b);
                });
            }
            Button::Ocr => draw_ocr_glyph(hdc, *r, dpi),
            _ => {
                if let Some(ch) = button_glyph(*btn) {
                    let old = SelectObject(hdc, HGDIOBJ(icon.0));
                    SetTextColor(hdc, rgb(238, 238, 238));
                    let mut buf = [ch];
                    let mut rr = *r;
                    DrawTextW(
                        hdc,
                        &mut buf,
                        &mut rr,
                        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                    );
                    SelectObject(hdc, old);
                } else if let Button::Tool(t) = btn {
                    draw_vector_glyph(hdc, *r, *t);
                }
            }
        }
    }
    let _ = DeleteObject(HGDIOBJ(icon.0)); // the icon font is ours; gui_font is shared
}

/// A handle to whichever icon font this machine actually has (`crate::win::icon_font_face`)
/// at toolbar size, scaled to `dpi` (identity at 96). NOT hard-coded to `Segoe Fluent Icons`:
/// that is Windows 11 only, and GDI substitutes silently rather than failing, so on Windows 10
/// every font-glyph button rendered as an empty box (issue #21).
unsafe fn icon_font(dpi: i32) -> HFONT {
    crate::win::icon_font(dpi_scale_dpi(16, dpi))
}

/// The Segoe Fluent Icons codepoint for a button with a clean glyph (the action
/// buttons + a few tools); `None` means a geometric tool drawn as an AA vector glyph,
/// or special handling (Colour swatch / Separator).
fn button_glyph(btn: Button) -> Option<u16> {
    Some(match btn {
        Button::Tool(Tool::Pen) => 0xE70F,        // Edit (pencil)
        Button::Tool(Tool::Text) => 0xE8D2,       // Font ("A")
        Button::Tool(Tool::Highlight) => 0xE7E6,  // Highlight (marker)
        Button::Tool(Tool::Eyedropper) => 0xEF3C, // Eyedropper (colour picker)
        Button::Tool(Tool::Move) => 0xE7C2,       // Move (four-way arrows)
        Button::Undo => 0xE7A7,
        Button::Redo => 0xE7A6,
        Button::Copy => 0xE8C8,
        Button::Save => 0xE74E,   // floppy disk
        Button::Upload => 0xE753, // cloud (cloud-upload)
        Button::Close => 0xE711,  // Cancel (X)
        _ => return None,
    })
}

/// The OCR button's icon: a scan frame (four corner brackets) around two text lines —
/// the conventional "read the text in this area" mark. Drawn as a vector rather than a
/// font glyph because Segoe Fluent has no unambiguous OCR codepoint, and a wrong one
/// would render as a tofu box. Sized off the same icon-font em the neighbouring glyph
/// buttons draw with, so it reads as one of them at every DPI.
unsafe fn draw_ocr_glyph(hdc: HDC, r: RECT, dpi: i32) {
    gdip::ocr_glyph(hdc, r, rgb(232, 232, 232), dpi_scale_dpi(16, dpi));
}

/// AA vector glyphs for the geometric tools (no font glyph exists for plain shapes).
unsafe fn draw_vector_glyph(hdc: HDC, r: RECT, tool: Tool) {
    let cx = (r.left + r.right) / 2;
    let cy = (r.top + r.bottom) / 2;
    let ink = rgb(232, 232, 232);
    match tool {
        Tool::Rect => gdip::with_aa(hdc, |g| {
            let p = gdip::pen(ink, 2);
            gdip::rect(g, p, cx - 7, cy - 5, 14, 10);
            gdip::drop_pen(p);
        }),
        Tool::Ellipse => gdip::with_aa(hdc, |g| {
            let p = gdip::pen(ink, 2);
            gdip::ellipse(g, p, cx - 7, cy - 5, 14, 10);
            gdip::drop_pen(p);
        }),
        Tool::Line => gdip::with_aa(hdc, |g| {
            let p = gdip::pen(ink, 2);
            gdip::line(g, p, cx - 7, cy + 5, cx + 7, cy - 5);
            gdip::drop_pen(p);
        }),
        Tool::Arrow => gdip::with_aa(hdc, |g| {
            let p = gdip::pen(ink, 2);
            gdip::line(g, p, cx - 7, cy + 5, cx + 7, cy - 5);
            gdip::line(g, p, cx + 7, cy - 5, cx + 1, cy - 5);
            gdip::line(g, p, cx + 7, cy - 5, cx + 7, cy + 1);
            gdip::drop_pen(p);
        }),
        Tool::Number => {
            // A light badge (visible on both the dark cell and the active-blue cell)
            // with a dark digit.
            gdip::with_aa(hdc, |g| {
                let b = gdip::brush(ink);
                gdip::fill_ellipse(g, b, cx - 8, cy - 8, 16, 16);
                gdip::drop_brush(b);
            });
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, rgb(20, 20, 20));
            let mut one = [b'1' as u16];
            let mut rr = r;
            DrawTextW(
                hdc,
                &mut one,
                &mut rr,
                DT_CENTER | DT_VCENTER | DT_SINGLELINE,
            );
        }
        Tool::Pixelate => gdip::with_aa(hdc, |g| {
            // A small checkerboard mosaic — reads clearly as "pixelate / blockify".
            let cell = 3;
            let n = 4; // 4×4 grid
            let x0 = cx - (n * cell) / 2;
            let y0 = cy - (n * cell) / 2;
            let light = gdip::brush(rgb(232, 232, 232));
            let dark = gdip::brush(rgb(105, 105, 105));
            for row in 0..n {
                for col in 0..n {
                    let b = if (row + col) % 2 == 0 { light } else { dark };
                    gdip::fill_rect(g, b, x0 + col * cell, y0 + row * cell, cell, cell);
                }
            }
            gdip::drop_brush(light);
            gdip::drop_brush(dark);
        }),
        Tool::Invert => gdip::with_aa(hdc, |g| {
            let bl = gdip::brush(rgb(235, 235, 235));
            gdip::fill_rect(g, bl, cx - 7, cy - 6, 7, 12); // light half…
            gdip::drop_brush(bl);
            let bd = gdip::brush(rgb(70, 70, 70));
            gdip::fill_rect(g, bd, cx, cy - 6, 7, 12); // …dark half
            gdip::drop_brush(bd);
            let p = gdip::pen(rgb(150, 150, 150), 1); // outline so the dark half reads
            gdip::rect(g, p, cx - 7, cy - 6, 14, 12);
            gdip::drop_pen(p);
        }),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sel() -> RECT {
        RECT {
            left: 300,
            top: 200,
            right: 700,
            bottom: 500,
        }
    }

    /// Every non-separator item must be reachable by a click. A button that lays out but
    /// can't be hit is invisible to the user even though it paints, so this pins the
    /// layout/hit-test pair for the whole bar (the OCR button included).
    #[test]
    fn every_button_is_hittable_and_no_two_overlap() {
        let buttons = layout(sel(), 1920, 1080, 96);
        assert_eq!(buttons.len(), items().len());
        for (btn, r) in &buttons {
            let (cx, cy) = ((r.left + r.right) / 2, (r.top + r.bottom) / 2);
            if matches!(btn, Button::Sep) {
                assert!(hit(&buttons, cx, cy).is_none(), "a divider is clickable");
                continue;
            }
            assert!(
                hit(&buttons, cx, cy) == Some(*btn),
                "a laid-out button isn't hit-testable at its own centre"
            );
        }
        for pair in buttons.windows(2) {
            assert!(
                pair[0].1.right <= pair[1].1.left,
                "toolbar cells overlap — a click would land on the wrong action"
            );
        }
    }

    /// The OCR button ships in the action group next to Copy (copy pixels / copy words),
    /// and every button carries a tooltip — `button_tip` returning "" would show an empty
    /// bubble on hover.
    #[test]
    fn ocr_button_sits_next_to_copy_and_is_described() {
        let order: Vec<Button> = items().iter().map(|(b, _)| *b).collect();
        let copy = order
            .iter()
            .position(|b| *b == Button::Copy)
            .expect("Copy button");
        let ocr = order
            .iter()
            .position(|b| *b == Button::Ocr)
            .expect("OCR button");
        assert_eq!(
            ocr,
            copy + 1,
            "OCR must stay immediately after Copy in the action group"
        );
        for (btn, _) in items() {
            if matches!(btn, Button::Sep) {
                continue;
            }
            assert!(!button_tip(btn).is_empty());
        }
        assert!(button_tip(Button::Ocr).contains("Ctrl+T"));
    }

    /// The bar has to fit on-screen even on a small display, or the rightmost actions
    /// (Save / Upload / Close, and now OCR) hang off the edge unreachable.
    #[test]
    fn bar_stays_inside_a_small_virtual_screen() {
        let (vw, vh) = (1024, 768);
        let buttons = layout(
            RECT {
                left: 900,
                top: 700,
                right: 1000,
                bottom: 760,
            },
            vw,
            vh,
            96,
        );
        let bar = bar_rect(&buttons, 96);
        assert!(
            bar.left >= 0 && bar.top >= 0,
            "bar clipped off the top-left"
        );
        assert!(bar.right <= vw, "bar runs off the right edge");
        assert!(bar.bottom <= vh, "bar runs off the bottom edge");
    }
}
