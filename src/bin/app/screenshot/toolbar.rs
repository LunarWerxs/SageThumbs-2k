//! The floating action bar shown under the selection once a region is chosen.
//!
//! Owner-drawn and hit-tested by `overlay.rs` (no child windows — it's painted
//! straight onto the fullscreen overlay), so it stays part of the same GDI surface.
//! Buttons either pick a [`Tool`] or run an action; the overlay maps the clicked
//! [`Button`] to the right effect.

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreateSolidBrush, DeleteObject, DrawTextW, FillRect, FrameRect,
    SelectObject, SetBkMode, SetTextColor, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DT_CALCRECT,
    DT_CENTER, DT_LEFT, DT_SINGLELINE, DT_VCENTER, HDC, HFONT, HGDIOBJ, LOGFONTW, TRANSPARENT,
};

use crate::dark::rgb;
use crate::win::{dpi_scale_dpi, gui_font, wide};

use super::gdip;
use super::tools::{face_name, Tool};

/// A toolbar item. `Sep` is a non-clickable divider between groups.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum Button {
    Tool(Tool),
    Color,
    Undo,
    Redo,
    Copy,
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
fn items() -> [(Button, i32); 23] {
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
        out.push((btn, RECT { left: bx, top: by, right: bx + w, bottom: by + h }));
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
    RECT { left: first.left - pad, top: first.top - pad, right: last.right + pad, bottom: last.bottom + pad }
}

/// Which button (if any) is under `(x, y)`. Separators are not clickable.
pub(super) fn hit(buttons: &[(Button, RECT)], x: i32, y: i32) -> Option<Button> {
    buttons
        .iter()
        .find(|(b, r)| !matches!(b, Button::Sep) && x >= r.left && x < r.right && y >= r.top && y < r.bottom)
        .map(|(b, _)| *b)
}

/// One-line description of a button, shown as a hover tooltip.
pub(super) fn button_tip(btn: Button) -> &'static str {
    match btn {
        Button::Tool(Tool::Rect) => "Rectangle (R) — drag to draw",
        Button::Tool(Tool::Ellipse) => "Ellipse (O) — drag to draw",
        Button::Tool(Tool::Arrow) => "Arrow (A) — drag from tail to head",
        Button::Tool(Tool::Line) => "Line (L) — drag to draw",
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
        Button::Redo => "Redo (Ctrl+Y)",
        Button::Copy => "Copy to the clipboard",
        Button::Save => "Save a PNG to Pictures\\Screenshots",
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
    DrawTextW(hdc, &mut w[..n], &mut calc, DT_CALCRECT | DT_SINGLELINE | DT_LEFT);
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
    let r = RECT { left: x, top: y, right: x + bw, bottom: y + bh };
    let bg = CreateSolidBrush(rgb(248, 248, 240));
    FillRect(hdc, &r, bg);
    let _ = DeleteObject(bg.into());
    let border = CreateSolidBrush(rgb(120, 120, 120));
    FrameRect(hdc, &r, border);
    let _ = DeleteObject(border.into());
    SetTextColor(hdc, rgb(20, 20, 20));
    let mut tr = RECT { left: x + pad, top: y + pad, right: x + bw, bottom: y + bh };
    DrawTextW(hdc, &mut w[..n], &mut tr, DT_SINGLELINE | DT_LEFT);
}

// ---- colour palette flyout -------------------------------------------------

/// Quick-pick preset colours (top row of the palette). Customs live below.
pub(super) const QUICK_COLORS: &[(u8, u8, u8)] = &[
    (230, 40, 40),
    (245, 140, 30),
    (245, 205, 40),
    (70, 190, 70),
    (40, 120, 230),
    (250, 250, 250),
];

/// A cell in the colour flyout.
#[derive(Clone, Copy)]
pub(super) enum Swatch {
    /// A fixed preset — click selects it.
    Color(COLORREF),
    /// A remembered-custom slot — `Some` = filled (click selects), `None` = empty
    /// (click opens the picker). Marked with the customizable accent border.
    Custom(Option<COLORREF>),
    /// The four-quadrant tile that opens the native colour picker for a new custom.
    Picker,
}

const SW: i32 = 20; // swatch size
const SWGAP: i32 = 3;
const COLS: i32 = 6;

/// Lay the flyout out above the Colour button `anchor` (flipped below / clamped to
/// the virtual screen). `dpi` scales the design pixels (identity at 96). Returns the
/// panel rect + each swatch with its absolute rect.
pub(super) fn color_flyout_layout(
    anchor: RECT,
    vw: i32,
    vh: i32,
    customs: &[COLORREF],
    dpi: i32,
) -> (RECT, Vec<(Swatch, RECT)>) {
    // Row 1: 6 presets. Row 2: 4 custom slots + the picker tile = exactly two rows.
    const SLOTS: i32 = 4;
    let sw = dpi_scale_dpi(SW, dpi);
    let swgap = dpi_scale_dpi(SWGAP, dpi);
    let pad = dpi_scale_dpi(5, dpi);
    let off = dpi_scale_dpi(6, dpi);
    let nq = QUICK_COLORS.len() as i32;
    let n = nq + SLOTS + 1;
    let rows = (n + COLS - 1) / COLS;
    let pw = COLS * sw + (COLS - 1) * swgap + pad * 2;
    let ph = rows * sw + (rows - 1) * swgap + pad * 2;
    let mut x = anchor.left;
    if x + pw > vw {
        x = vw - pw;
    }
    x = x.max(0);
    let mut y = anchor.top - ph - off; // above the button…
    if y < 0 {
        y = anchor.bottom + off; // …or below if there's no room
    }
    y = y.min(vh - ph).max(0); // keep the whole panel on-screen
    let panel = RECT { left: x, top: y, right: x + pw, bottom: y + ph };
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let (row, col) = (i / COLS, i % COLS);
        let sx = x + pad + col * (sw + swgap);
        let sy = y + pad + row * (sw + swgap);
        let rect = RECT { left: sx, top: sy, right: sx + sw, bottom: sy + sw };
        let sw = if i < nq {
            let (r, g, b) = QUICK_COLORS[i as usize];
            Swatch::Color(rgb(r, g, b))
        } else if i < nq + SLOTS {
            Swatch::Custom(customs.get((i - nq) as usize).copied())
        } else {
            Swatch::Picker
        };
        out.push((sw, rect));
    }
    (panel, out)
}

/// Paint the colour flyout: a dark panel of swatches, the active colour ringed, and
/// a four-quadrant "custom" tile.
pub(super) unsafe fn draw_color_flyout(hdc: HDC, panel: RECT, items: &[(Swatch, RECT)], current: COLORREF) {
    let bg = CreateSolidBrush(rgb(32, 32, 32));
    FillRect(hdc, &panel, bg);
    let _ = DeleteObject(bg.into());
    let border = CreateSolidBrush(rgb(80, 80, 80));
    FrameRect(hdc, &panel, border);
    let _ = DeleteObject(border.into());

    // Customizable cells (custom slots + the picker) carry a light-blue accent ring;
    // presets get a plain edge. The active colour always wins with a white ring.
    let accent = rgb(95, 165, 235);
    for (sw, r) in items {
        match sw {
            Swatch::Color(c) => {
                fill_c(hdc, r, *c);
                if *c == current {
                    ring(hdc, r, rgb(255, 255, 255));
                } else {
                    frame_c(hdc, r, rgb(70, 70, 70));
                }
            }
            Swatch::Custom(Some(c)) => {
                fill_c(hdc, r, *c);
                ring(hdc, r, if *c == current { rgb(255, 255, 255) } else { accent });
            }
            Swatch::Custom(None) => {
                fill_c(hdc, r, rgb(48, 48, 48)); // empty placeholder…
                ring(hdc, r, accent);
                SelectObject(hdc, HGDIOBJ(gui_font().0)); // …with a "+" inviting a pick
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, rgb(175, 175, 175));
                let mut w = wide("+");
                let mut rr = *r;
                DrawTextW(hdc, &mut w[..1], &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
            Swatch::Picker => {
                let mx = (r.left + r.right) / 2;
                let my = (r.top + r.bottom) / 2;
                fill_c(hdc, &RECT { left: r.left, top: r.top, right: mx, bottom: my }, rgb(230, 40, 40));
                fill_c(hdc, &RECT { left: mx, top: r.top, right: r.right, bottom: my }, rgb(70, 190, 70));
                fill_c(hdc, &RECT { left: r.left, top: my, right: mx, bottom: r.bottom }, rgb(40, 120, 230));
                fill_c(hdc, &RECT { left: mx, top: my, right: r.right, bottom: r.bottom }, rgb(245, 200, 40));
                ring(hdc, r, accent);
            }
        }
    }
}

unsafe fn fill_c(hdc: HDC, r: &RECT, c: COLORREF) {
    let b = CreateSolidBrush(c);
    FillRect(hdc, r, b);
    let _ = DeleteObject(b.into());
}
unsafe fn frame_c(hdc: HDC, r: &RECT, c: COLORREF) {
    let b = CreateSolidBrush(c);
    FrameRect(hdc, r, b);
    let _ = DeleteObject(b.into());
}
/// A 2px ring of `c` around `r`.
unsafe fn ring(hdc: HDC, r: &RECT, c: COLORREF) {
    frame_c(hdc, r, c);
    let inner = RECT { left: r.left + 1, top: r.top + 1, right: r.right - 1, bottom: r.bottom - 1 };
    frame_c(hdc, &inner, c);
}

// ---- text settings flyout --------------------------------------------------

/// A short list of common Windows fonts for the lightweight font dropdown. Anything
/// else is reachable via the "Font… (more)" button → the native Font dialog.
pub(super) const PRESET_FONTS: &[&str] = &[
    "Segoe UI",
    "Arial",
    "Calibri",
    "Verdana",
    "Tahoma",
    "Consolas",
    "Times New Roman",
    "Comic Sans MS",
];

/// A clickable region of the text settings flyout.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum TextItem {
    FontField,         // toggles the font dropdown
    FontOption(usize), // a font in the open dropdown
    SizeDown,
    SizeUp,
    Bold,
    Underline,
    More, // → native Font dialog
}

const TF_PW: i32 = 200; // flyout width
const TF_ROW: i32 = 24;
const TF_OPT: i32 = 20; // dropdown option height

/// Lay the text flyout out above the Text button `anchor` (clamped on-screen). When
/// `dropdown` is set, the font option rows are included (and the panel grows). `dpi`
/// scales the design pixels (identity at 96).
pub(super) fn text_flyout_layout(anchor: RECT, vw: i32, vh: i32, dropdown: bool, dpi: i32) -> (RECT, Vec<(TextItem, RECT)>) {
    let pad = dpi_scale_dpi(6, dpi);
    let gap = dpi_scale_dpi(6, dpi);
    let off = dpi_scale_dpi(6, dpi);
    let pw = dpi_scale_dpi(TF_PW, dpi);
    let row = dpi_scale_dpi(TF_ROW, dpi);
    let opt = dpi_scale_dpi(TF_OPT, dpi);
    let inset = dpi_scale_dpi(2, dpi); // the row's bottom inset / dropdown padding
    let nf = PRESET_FONTS.len() as i32;
    let drop_h = if dropdown { nf * opt + inset * 2 } else { 0 };
    let ph = pad + row + drop_h + gap + row + gap + row + gap + row + pad;
    let mut x = anchor.left;
    if x + pw > vw {
        x = vw - pw;
    }
    x = x.max(0);
    let mut y = anchor.top - ph - off;
    if y < 0 {
        y = anchor.bottom + off;
    }
    y = y.min(vh - ph).max(0);
    let panel = RECT { left: x, top: y, right: x + pw, bottom: y + ph };
    let ix = x + pad;
    let iw = pw - pad * 2;
    let mut items = Vec::new();
    let mut cy = y + pad;
    items.push((TextItem::FontField, RECT { left: ix, top: cy, right: ix + iw, bottom: cy + row - inset }));
    cy += row;
    if dropdown {
        cy += inset;
        for i in 0..nf {
            items.push((TextItem::FontOption(i as usize), RECT { left: ix, top: cy, right: ix + iw, bottom: cy + opt }));
            cy += opt;
        }
        cy += inset;
    }
    cy += gap;
    let bw = dpi_scale_dpi(28, dpi);
    items.push((TextItem::SizeDown, RECT { left: ix, top: cy, right: ix + bw, bottom: cy + row - inset }));
    items.push((TextItem::SizeUp, RECT { left: ix + iw - bw, top: cy, right: ix + iw, bottom: cy + row - inset }));
    cy += row + gap;
    // Bold + Underline share a row (each half-width).
    let half = (iw - gap) / 2;
    items.push((TextItem::Bold, RECT { left: ix, top: cy, right: ix + half, bottom: cy + row - inset }));
    items.push((TextItem::Underline, RECT { left: ix + iw - half, top: cy, right: ix + iw, bottom: cy + row - inset }));
    cy += row + gap;
    items.push((TextItem::More, RECT { left: ix, top: cy, right: ix + iw, bottom: cy + row - inset }));
    (panel, items)
}

/// A small dark button with a centred label.
unsafe fn draw_btn(hdc: HDC, r: RECT, label: &str) {
    let bg = CreateSolidBrush(rgb(60, 60, 60));
    FillRect(hdc, &r, bg);
    let _ = DeleteObject(bg.into());
    let e = CreateSolidBrush(rgb(95, 95, 95));
    FrameRect(hdc, &r, e);
    let _ = DeleteObject(e.into());
    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetTextColor(hdc, rgb(235, 235, 235));
    let mut w = wide(label);
    let n = w.len().saturating_sub(1);
    let mut rr = r;
    DrawTextW(hdc, &mut w[..n], &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
}

/// Paint the text settings flyout for the current `font`. `dpi` scales the design
/// pixels (identity at 96).
pub(super) unsafe fn draw_text_flyout(hdc: HDC, panel: RECT, items: &[(TextItem, RECT)], font: &LOGFONTW, dpi: i32) {
    let bg = CreateSolidBrush(rgb(32, 32, 32));
    FillRect(hdc, &panel, bg);
    let _ = DeleteObject(bg.into());
    let border = CreateSolidBrush(rgb(80, 80, 80));
    FrameRect(hdc, &panel, border);
    let _ = DeleteObject(border.into());

    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    let cur_face = face_name(font);
    let size = -font.lfHeight;
    let underline = font.lfUnderline != 0;
    let bold = font.lfWeight >= 700;

    let mut down = RECT::default();
    let mut up = RECT::default();
    for (it, r) in items {
        if let TextItem::SizeDown = it {
            down = *r;
        }
        if let TextItem::SizeUp = it {
            up = *r;
        }
    }

    for (it, r) in items {
        match it {
            TextItem::FontField => {
                let b = CreateSolidBrush(rgb(55, 55, 55));
                FillRect(hdc, r, b);
                let _ = DeleteObject(b.into());
                let e = CreateSolidBrush(rgb(95, 95, 95));
                FrameRect(hdc, r, e);
                let _ = DeleteObject(e.into());
                SelectObject(hdc, HGDIOBJ(gui_font().0));
                SetTextColor(hdc, rgb(235, 235, 235));
                let mut tr = RECT { left: r.left + dpi_scale_dpi(6, dpi), top: r.top, right: r.right - dpi_scale_dpi(18, dpi), bottom: r.bottom };
                let mut w = wide(&cur_face);
                let n = w.len().saturating_sub(1);
                DrawTextW(hdc, &mut w[..n], &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                let mut cr = RECT { left: r.right - dpi_scale_dpi(16, dpi), top: r.top, right: r.right, bottom: r.bottom };
                let mut wv = wide("\u{25BE}"); // ▾
                let nv = wv.len().saturating_sub(1);
                DrawTextW(hdc, &mut wv[..nv], &mut cr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            }
            TextItem::FontOption(i) => {
                let name = PRESET_FONTS[*i];
                if name == cur_face {
                    let b = CreateSolidBrush(rgb(0, 90, 160));
                    FillRect(hdc, r, b);
                    let _ = DeleteObject(b.into());
                }
                let mut lf = LOGFONTW { lfHeight: -dpi_scale_dpi(16, dpi), ..Default::default() };
                for (k, c) in wide(name).iter().take(lf.lfFaceName.len() - 1).enumerate() {
                    lf.lfFaceName[k] = *c;
                }
                let hf = CreateFontIndirectW(&lf);
                let old = SelectObject(hdc, HGDIOBJ(hf.0));
                SetTextColor(hdc, rgb(235, 235, 235));
                let mut tr = RECT { left: r.left + 8, top: r.top, right: r.right - 4, bottom: r.bottom };
                let mut w = wide(name);
                let n = w.len().saturating_sub(1);
                DrawTextW(hdc, &mut w[..n], &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
                SelectObject(hdc, old);
                let _ = DeleteObject(HGDIOBJ(hf.0));
            }
            TextItem::SizeDown => draw_btn(hdc, *r, "-"),
            TextItem::SizeUp => draw_btn(hdc, *r, "+"),
            TextItem::Bold => {
                SelectObject(hdc, HGDIOBJ(gui_font().0));
                SetTextColor(hdc, rgb(235, 235, 235));
                let label = if bold { "[x]  Bold" } else { "[  ]  Bold" };
                let mut tr = RECT { left: r.left + 4, top: r.top, right: r.right, bottom: r.bottom };
                let mut w = wide(label);
                let n = w.len().saturating_sub(1);
                DrawTextW(hdc, &mut w[..n], &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            }
            TextItem::Underline => {
                SelectObject(hdc, HGDIOBJ(gui_font().0));
                SetTextColor(hdc, rgb(235, 235, 235));
                let label = if underline { "[x]  Underline" } else { "[  ]  Underline" };
                let mut tr = RECT { left: r.left + 4, top: r.top, right: r.right, bottom: r.bottom };
                let mut w = wide(label);
                let n = w.len().saturating_sub(1);
                DrawTextW(hdc, &mut w[..n], &mut tr, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            }
            TextItem::More => draw_btn(hdc, *r, "Font\u{2026} (more)"),
        }
    }

    // The size value, centred between the − and + buttons.
    if down.right < up.left {
        SelectObject(hdc, HGDIOBJ(gui_font().0));
        SetTextColor(hdc, rgb(255, 255, 255));
        let mut nr = RECT { left: down.right, top: down.top, right: up.left, bottom: down.bottom };
        let mut w = wide(&format!("{size} px"));
        let n = w.len().saturating_sub(1);
        DrawTextW(hdc, &mut w[..n], &mut nr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
    }
}

/// Paint the bar: a rounded backdrop, rounded per-group icon cells, the group
/// dividers, then each button's icon (a Segoe Fluent glyph, an AA vector glyph, or
/// the colour swatch).
pub(super) unsafe fn draw(hdc: HDC, buttons: &[(Button, RECT)], active: Tool, color: COLORREF, dpi: i32) {
    let bar = bar_rect(buttons, dpi);

    // Rounded backdrop + every cell background, in one anti-aliased GDI+ pass.
    let r_bar = dpi_scale_dpi(9, dpi); // bar corner radius
    let r_cell = dpi_scale_dpi(6, dpi); // per-cell corner radius
    gdip::with_aa(hdc, |g| {
        let bg = gdip::brush(rgb(28, 28, 28));
        gdip::fill_round(g, bg, bar.left, bar.top, bar.right - bar.left, bar.bottom - bar.top, r_bar);
        gdip::drop_brush(bg);
        let pen = gdip::pen(rgb(72, 72, 72), 1);
        gdip::stroke_round(g, pen, bar.left, bar.top, bar.right - bar.left, bar.bottom - bar.top, r_bar);
        gdip::drop_pen(pen);
        for (btn, r) in buttons {
            if matches!(btn, Button::Sep) {
                continue;
            }
            let on = matches!(btn, Button::Tool(t) if *t == active);
            let cb = gdip::brush(if on { rgb(0, 120, 210) } else { rgb(54, 54, 54) });
            gdip::fill_round(g, cb, r.left, r.top, r.right - r.left, r.bottom - r.top, r_cell);
            gdip::drop_brush(cb);
        }
    });

    // Group divider lines (between the rounded cells).
    let div_inset = dpi_scale_dpi(5, dpi);
    for (btn, r) in buttons {
        if let Button::Sep = btn {
            let cx = (r.left + r.right) / 2;
            let line = RECT { left: cx, top: r.top + div_inset, right: cx + 1, bottom: r.bottom - div_inset };
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
                    gdip::fill_round(g, b, r.left + sw_inset, r.top + sw_inset, (r.right - r.left) - sw_inset * 2, (r.bottom - r.top) - sw_inset * 2, sw_round);
                    gdip::drop_brush(b);
                });
            }
            _ => {
                if let Some(ch) = button_glyph(*btn) {
                    let old = SelectObject(hdc, HGDIOBJ(icon.0));
                    SetTextColor(hdc, rgb(238, 238, 238));
                    let mut buf = [ch];
                    let mut rr = *r;
                    DrawTextW(hdc, &mut buf, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                    SelectObject(hdc, old);
                } else if let Button::Tool(t) = btn {
                    draw_vector_glyph(hdc, *r, *t);
                }
            }
        }
    }
    let _ = DeleteObject(HGDIOBJ(icon.0)); // the icon font is ours; gui_font is shared
}

/// A handle to **Segoe Fluent Icons** (the default Win11 icon font) at toolbar size,
/// scaled to `dpi` (identity at 96). If the font is somehow absent, GDI substitutes a
/// default and the few font-glyph tools fall back to a placeholder box — the
/// vector-glyph tools are unaffected.
unsafe fn icon_font(dpi: i32) -> HFONT {
    let mut lf = LOGFONTW { lfHeight: -dpi_scale_dpi(16, dpi), lfWeight: 400, lfQuality: CLEARTYPE_QUALITY, lfCharSet: DEFAULT_CHARSET, ..Default::default() };
    let face = wide("Segoe Fluent Icons");
    for (i, c) in face.iter().take(lf.lfFaceName.len() - 1).enumerate() {
        lf.lfFaceName[i] = *c;
    }
    CreateFontIndirectW(&lf)
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
            DrawTextW(hdc, &mut one, &mut rr, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
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
