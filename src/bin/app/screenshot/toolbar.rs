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
use crate::win::{dpi_scale_dpi, gui_font, t, wide};

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

/// The keyboard focus ring's colour, shared by the bar and both flyouts so "you are here"
/// looks the same wherever focus currently is. Amber on purpose, because every other state
/// on this chrome is already spoken for: the active tool owns a blue cell fill, the palette
/// marks the current colour with a white ring and its customizable cells with a light-blue
/// one, and hover shows a tooltip rather than changing a cell. A keyboard user has to be
/// able to tell focus from all three at a glance.
const FOCUS_RING: (u8, u8, u8) = (255, 190, 40);

/// The first toolbar item that can take keyboard focus, or `None` if the bar has none.
/// Separators are painted dividers, never focus stops, exactly as `hit` refuses to click one.
pub(super) fn first_focusable(items: &[(Button, RECT)]) -> Option<usize> {
    items.iter().position(|(b, _)| !matches!(b, Button::Sep))
}

/// The next (`forward`) or previous focusable item after `from`, wrapping around the bar.
/// `None` only when nothing on the bar can take focus at all.
///
/// Written as a bounded walk of exactly `items.len()` candidates rather than the obvious
/// `loop { i = next(i); if focusable(i) { break } }`: an empty bar, or one that somehow held
/// nothing but separators, spins that loop forever, and this binary is built with
/// `panic = "abort"` so there is no unwinding to rescue a user from a hung fullscreen
/// topmost window. The last candidate a full sweep examines is `from` itself, which is what
/// makes a single-focusable-item bar answer "stay put" instead of "nowhere to go".
pub(super) fn step_focus(items: &[(Button, RECT)], from: usize, forward: bool) -> Option<usize> {
    let n = items.len();
    if n == 0 {
        return None;
    }
    let from = from.min(n - 1); // a stale index must clamp, never index out of bounds
    for step in 1..=n {
        let i = if forward {
            (from + step) % n
        } else {
            (from + n - (step % n)) % n
        };
        if !matches!(items[i].0, Button::Sep) {
            return Some(i);
        }
    }
    None
}

/// Move `from` by `step` places through a flat list of `len` items, wrapping both ways.
/// Horizontal movement passes ±1; vertical movement in a grid passes ±the column count, so
/// Up/Down land in the row above/below. `None` only for an empty list.
///
/// The wrap is modular over the FLAT list rather than column-preserving, and that is a
/// deliberate choice about ragged grids: the colour palette is 6 wide but holds 11 cells (6
/// presets, then 4 custom slots plus the picker), so column 5 of the top row has nothing
/// beneath it. A column-preserving wrap gives that cell a dead Down key, which is a worse
/// outcome for a keyboard-only user than landing one column over. Modular stepping always
/// moves, and repeated presses still visit every cell.
pub(super) fn wrap_step(len: usize, from: usize, step: isize) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let n = len as isize;
    let from = from.min(len - 1) as isize; // clamp a stale index rather than wrap it oddly
    Some((from + step).rem_euclid(n) as usize)
}

/// How many items sit in the laid-out grid's first row, i.e. its column count.
///
/// MEASURED off the rects rather than assumed, because the two flyouts are different
/// shapes: the colour palette really is a 6-wide grid, while the text flyout is a stack of
/// rows (some of which happen to hold two half-width controls). Deriving it here means the
/// arrow keys work in both without either layout file having to declare its shape a second
/// time, and a layout change cannot silently desynchronise from a hard-coded constant.
/// A single-column result makes vertical movement identical to Tab, which is the right
/// behaviour for a stack.
pub(super) fn grid_cols(rects: &[RECT]) -> usize {
    let Some(first) = rects.first() else {
        return 0;
    };
    rects
        .iter()
        .take_while(|r| r.top == first.top)
        .count()
        .max(1)
}

/// Paint the keyboard focus ring for one item of a flyout panel, as a 2px amber frame drawn
/// just INSIDE `r`.
///
/// Inside, unlike the toolbar's ring, because a flyout's rows can abut with no gap at all
/// (the font dropdown's option rows share edges), so an outset ring would bleed onto the
/// neighbouring item and read as though two things were focused. Inset costs nothing: it
/// paints over the item's own first two pixels and changes no metric.
pub(super) unsafe fn draw_focus_ring_inside(hdc: HDC, r: RECT) {
    let (fr, fg, fb) = FOCUS_RING;
    let c = rgb(fr, fg, fb);
    for inset in 0..2 {
        let b = CreateSolidBrush(c);
        FrameRect(
            hdc,
            &RECT {
                left: r.left + inset,
                top: r.top + inset,
                right: r.right - inset,
                bottom: r.bottom - inset,
            },
            b,
        );
        let _ = DeleteObject(b.into());
    }
}

/// Paint the keyboard focus ring for one item, as a 2px amber frame drawn just OUTSIDE `r`.
///
/// Outside, so it can never be confused with the rings the colour palette already draws
/// INSIDE a swatch to mean "this is the current colour" (white) or "this cell is
/// customizable" (light blue). It lands in the gap the layout already leaves between cells
/// (3 design px, scaled) and inside the panel's own padding (5 design px), so it overlaps
/// nothing and, being paint only, moves no rect: `layout` is untouched.
pub(super) unsafe fn draw_focus_ring_outside(hdc: HDC, r: RECT) {
    let (fr, fg, fb) = FOCUS_RING;
    let c = rgb(fr, fg, fb);
    for out in 1..=2 {
        let b = CreateSolidBrush(c);
        FrameRect(
            hdc,
            &RECT {
                left: r.left - out,
                top: r.top - out,
                right: r.right + out,
                bottom: r.bottom + out,
            },
            b,
        );
        let _ = DeleteObject(b.into());
    }
}

/// One-line description of a button, shown as a hover tooltip. Localized (audit F29,
/// 2026-09-06): every branch reads its sentence from the locale table; the tool branches carry
/// a `{key}` slot for the tool's single-letter keyboard shortcut, filled in HERE (never by a
/// translation) because it names a real key on the keyboard, not a word.
pub(super) fn button_tip(btn: Button) -> String {
    let key = |tpl: &str, letter: &str| t(tpl).replace("{key}", letter);
    match btn {
        Button::Tool(Tool::Rect) => key("shot_tip_rect", "R"),
        Button::Tool(Tool::Ellipse) => key("shot_tip_ellipse", "O"),
        Button::Tool(Tool::Arrow) => key("shot_tip_arrow", "A"),
        Button::Tool(Tool::Line) => key("shot_tip_line", "L"),
        Button::Tool(Tool::Pen) => key("shot_tip_pen", "P"),
        Button::Tool(Tool::Text) => key("shot_tip_text", "T"),
        Button::Tool(Tool::Number) => key("shot_tip_number", "N"),
        Button::Tool(Tool::Highlight) => key("shot_tip_highlight", "H"),
        Button::Tool(Tool::Pixelate) => key("shot_tip_pixelate", "B"),
        Button::Tool(Tool::Invert) => key("shot_tip_invert", "I"),
        Button::Tool(Tool::Eyedropper) => key("shot_tip_eyedropper", "E"),
        Button::Tool(Tool::Move) => key("shot_tip_move", "M"),
        Button::Color => key("shot_tip_color", "K"),
        Button::Undo => t("shot_tip_undo").to_string(),
        Button::Redo => t("shot_tip_redo").to_string(),
        Button::Copy => t("shot_tip_copy").to_string(),
        Button::Ocr => t("shot_tip_ocr").to_string(),
        Button::Save => t("shot_tip_save").to_string(),
        Button::Upload => t("shot_tip_upload").to_string(),
        Button::Close => t("shot_tip_close").to_string(),
        Button::Sep => String::new(),
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
///
/// `focus` is the index (into `buttons`) of the keyboard-focused cell, or `None` when focus
/// is unset or currently inside a flyout. It only adds a ring; it changes no metric, so a
/// capture that never takes focus paints exactly the same pixels it always did.
pub(super) unsafe fn draw(
    hdc: HDC,
    buttons: &[(Button, RECT)],
    active: Tool,
    color: COLORREF,
    dpi: i32,
    focus: Option<usize>,
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
        // The keyboard focus ring, in the same anti-aliased pass so it follows the cells'
        // rounded corners, and drawn AFTER every cell fill so a neighbouring fill cannot
        // paint over it. It sits in the gap outside the cell (the bar's own padding at the
        // ends), which keeps it clear of the blue "active tool" fill that occupies the cell
        // itself, and it moves nothing: `layout` never sees it.
        if let Some((_, r)) = focus.and_then(|i| buttons.get(i)) {
            let out = dpi_scale_dpi(2, dpi);
            let (fr, fg, fb) = FOCUS_RING;
            let pen = gdip::pen(rgb(fr, fg, fb), 2);
            gdip::stroke_round(
                g,
                pen,
                r.left - out,
                r.top - out,
                (r.right - r.left) + out * 2,
                (r.bottom - r.top) + out * 2,
                r_cell + out,
            );
            gdip::drop_pen(pen);
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
    ///
    /// `button_tip` is now locale-dependent (audit F29), so this forces English explicitly
    /// rather than trusting whatever the test machine's Windows UI language happens to be —
    /// the assertion below checks a literal English substring.
    #[test]
    fn ocr_button_sits_next_to_copy_and_is_described() {
        sagethumbs2k_core::i18n::apply_override_or_system(Some("en"));
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

    /// Audit F29: every tooltip must come from the locale table, not a hardcoded literal.
    /// Compares `button_tip` against `t(key)` (with the tool letter substituted the same way
    /// `button_tip` itself does it) for every button — a hardcoded `&'static str` could not
    /// track a key it never looks up. Also proves the `{key}` placeholder actually gets filled:
    /// a missing `.replace()` would leave the literal text `{key}` in the tooltip.
    #[test]
    fn button_tip_reads_the_locale_table_and_fills_the_key_placeholder() {
        sagethumbs2k_core::i18n::apply_override_or_system(Some("en"));
        let pairs = [
            (Button::Tool(Tool::Rect), "shot_tip_rect", "R"),
            (Button::Tool(Tool::Ellipse), "shot_tip_ellipse", "O"),
            (Button::Tool(Tool::Arrow), "shot_tip_arrow", "A"),
            (Button::Tool(Tool::Line), "shot_tip_line", "L"),
            (Button::Tool(Tool::Pen), "shot_tip_pen", "P"),
            (Button::Tool(Tool::Text), "shot_tip_text", "T"),
            (Button::Tool(Tool::Number), "shot_tip_number", "N"),
            (Button::Tool(Tool::Highlight), "shot_tip_highlight", "H"),
            (Button::Tool(Tool::Pixelate), "shot_tip_pixelate", "B"),
            (Button::Tool(Tool::Invert), "shot_tip_invert", "I"),
            (Button::Tool(Tool::Eyedropper), "shot_tip_eyedropper", "E"),
            (Button::Tool(Tool::Move), "shot_tip_move", "M"),
            (Button::Color, "shot_tip_color", "K"),
        ];
        for (btn, key, letter) in pairs {
            let tip = button_tip(btn);
            assert_eq!(tip, crate::win::t(key).replace("{key}", letter));
            assert!(
                !tip.contains("{key}"),
                "the {{key}} placeholder was never substituted in {key}"
            );
            assert!(tip.contains(letter), "{key} lost its shortcut letter");
        }
        for (btn, key) in [
            (Button::Undo, "shot_tip_undo"),
            (Button::Redo, "shot_tip_redo"),
            (Button::Copy, "shot_tip_copy"),
            (Button::Ocr, "shot_tip_ocr"),
            (Button::Save, "shot_tip_save"),
            (Button::Upload, "shot_tip_upload"),
            (Button::Close, "shot_tip_close"),
        ] {
            assert_eq!(button_tip(btn), crate::win::t(key));
        }
    }

    /// Keyboard focus has to reach every button the mouse can click, in both directions,
    /// and has to step OVER the dividers rather than parking on one: a focus ring around a
    /// painted line, with Space doing nothing, reads as a broken toolbar.
    #[test]
    fn keyboard_focus_walks_the_bar_both_ways_and_never_lands_on_a_divider() {
        let bar = layout(sel(), 1920, 1080, 96);
        let focusable: Vec<usize> = bar
            .iter()
            .enumerate()
            .filter(|(_, (b, _))| !matches!(b, Button::Sep))
            .map(|(i, _)| i)
            .collect();
        assert!(
            focusable.len() > 1,
            "this bar is supposed to have real buttons"
        );
        assert_eq!(first_focusable(&bar), focusable.first().copied());

        // From every focusable seat, one step forward lands on the next focusable seat and
        // one step back lands on the previous one, with the dividers between them skipped
        // and both ends wrapping.
        for (n, &i) in focusable.iter().enumerate() {
            let ahead = focusable[(n + 1) % focusable.len()];
            let behind = focusable[(n + focusable.len() - 1) % focusable.len()];
            assert_eq!(step_focus(&bar, i, true), Some(ahead));
            assert_eq!(step_focus(&bar, i, false), Some(behind));
        }

        // A full lap must visit every focusable button exactly once and close back on the
        // first, which is the property a user actually feels: hold Tab and you get round the
        // whole bar without a repeat and without a dead stop.
        let mut seen = Vec::new();
        let mut cur = first_focusable(&bar).expect("a focusable button");
        for _ in 0..focusable.len() {
            seen.push(cur);
            cur = step_focus(&bar, cur, true).expect("the walk must always find a seat");
        }
        seen.sort_unstable();
        assert_eq!(seen, focusable);
        assert_eq!(
            cur, focusable[0],
            "a full lap must close back on the first button"
        );
    }

    /// The degenerate lists. A `loop { i = next(i) }` written the obvious way spins forever
    /// on an empty bar or one holding nothing but dividers, and this binary is built with
    /// `panic = "abort"`, so a hang here freezes a fullscreen topmost window with no way out.
    /// The walk must ANSWER "nowhere to go" instead.
    #[test]
    fn keyboard_focus_cannot_panic_or_spin_on_a_degenerate_bar() {
        let empty: Vec<(Button, RECT)> = Vec::new();
        assert_eq!(first_focusable(&empty), None);
        assert_eq!(step_focus(&empty, 0, true), None);
        assert_eq!(step_focus(&empty, 7, false), None); // a stale index must clamp, not index

        let one = vec![(Button::Copy, RECT::default())];
        assert_eq!(first_focusable(&one), Some(0));
        assert_eq!(step_focus(&one, 0, true), Some(0));
        assert_eq!(step_focus(&one, 0, false), Some(0));

        let dividers = vec![
            (Button::Sep, RECT::default()),
            (Button::Sep, RECT::default()),
        ];
        assert_eq!(first_focusable(&dividers), None);
        assert_eq!(step_focus(&dividers, 0, true), None);
        assert_eq!(step_focus(&dividers, 1, false), None);
    }

    /// The flyouts have no dividers, so they step by index, but they still have to wrap at
    /// both ends and survive a list length that changed underneath a stale index (the text
    /// flyout grows by eight rows the moment the font dropdown expands).
    #[test]
    fn wrap_step_moves_through_a_flyout_list_and_wraps_at_both_ends() {
        assert_eq!(wrap_step(11, 0, 1), Some(1));
        assert_eq!(wrap_step(11, 10, 1), Some(0)); // off the end, back to the start
        assert_eq!(wrap_step(11, 0, -1), Some(10)); // off the start, round to the end

        // A vertical arrow in the 6 wide palette steps a whole row. The last row is ragged
        // (11 cells in a 6 wide grid), and the wrap there is through the flat list on
        // purpose: a column-preserving wrap would leave the top row's last cell with a Down
        // key that does nothing at all.
        assert_eq!(wrap_step(11, 0, 6), Some(6));
        assert_eq!(wrap_step(11, 5, 6), Some(0));
        assert_eq!(wrap_step(11, 6, -6), Some(0));

        assert_eq!(wrap_step(0, 0, 1), None, "an empty list has nowhere to go");
        assert_eq!(wrap_step(1, 0, 1), Some(0));
        assert_eq!(wrap_step(1, 0, -1), Some(0));
        assert_eq!(wrap_step(4, 99, 1), Some(0)); // stale index clamps to the last, then steps
    }

    /// The arrow keys measure the grid off the laid-out rects rather than assuming a shape,
    /// so this pins what that measurement actually returns for the two real flyouts. Get it
    /// wrong for the palette and Up/Down move one swatch instead of one row; get it wrong for
    /// the text flyout and they jump over most of the settings.
    #[test]
    fn grid_cols_is_measured_from_the_real_flyout_layouts() {
        let bar = layout(sel(), 1920, 1080, 96);
        let (_, color_cell) = bar
            .iter()
            .find(|(b, _)| *b == Button::Color)
            .copied()
            .expect("the Colour button");
        let (_, swatches) = color_flyout_layout(color_cell, 1920, 1080, &[], 96);
        let rects: Vec<RECT> = swatches.iter().map(|(_, r)| *r).collect();
        assert_eq!(grid_cols(&rects), 6, "the palette is a 6 wide grid");

        let (_, text_cell) = bar
            .iter()
            .find(|(b, _)| *b == Button::Tool(Tool::Text))
            .copied()
            .expect("the Text button");
        let (_, items) = text_flyout_layout(text_cell, 1920, 1080, true, 96);
        let rects: Vec<RECT> = items.iter().map(|(_, r)| *r).collect();
        assert_eq!(
            grid_cols(&rects),
            1,
            "the text flyout is a stack of rows, so a vertical step is one row"
        );

        assert_eq!(grid_cols(&[]), 0);
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
