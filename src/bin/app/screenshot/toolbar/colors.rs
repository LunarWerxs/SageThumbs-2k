//! The colour-palette flyout: the preset swatch grid, the user's recent customs,
//! and the eyedropper cell that opens the system picker.

use super::*;

// ---- colour palette flyout -------------------------------------------------

/// Quick-pick preset colours (top row of the palette). Customs live below.
const QUICK_COLORS: &[(u8, u8, u8)] = &[
    (230, 40, 40),
    (245, 140, 30),
    (245, 205, 40),
    (70, 190, 70),
    (40, 120, 230),
    (250, 250, 250),
];

/// A cell in the colour flyout.
#[derive(Clone, Copy)]
pub(crate) enum Swatch {
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
pub(crate) fn color_flyout_layout(
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
    let panel = RECT {
        left: x,
        top: y,
        right: x + pw,
        bottom: y + ph,
    };
    let mut out = Vec::with_capacity(n as usize);
    for i in 0..n {
        let (row, col) = (i / COLS, i % COLS);
        let sx = x + pad + col * (sw + swgap);
        let sy = y + pad + row * (sw + swgap);
        let rect = RECT {
            left: sx,
            top: sy,
            right: sx + sw,
            bottom: sy + sw,
        };
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
pub(crate) unsafe fn draw_color_flyout(
    hdc: HDC,
    panel: RECT,
    items: &[(Swatch, RECT)],
    current: COLORREF,
) {
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
                ring(
                    hdc,
                    r,
                    if *c == current {
                        rgb(255, 255, 255)
                    } else {
                        accent
                    },
                );
            }
            Swatch::Custom(None) => {
                fill_c(hdc, r, rgb(48, 48, 48)); // empty placeholder…
                ring(hdc, r, accent);
                SelectObject(hdc, HGDIOBJ(gui_font().0)); // …with a "+" inviting a pick
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, rgb(175, 175, 175));
                let mut w = wide("+");
                let mut rr = *r;
                DrawTextW(
                    hdc,
                    &mut w[..1],
                    &mut rr,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
            }
            Swatch::Picker => {
                let mx = (r.left + r.right) / 2;
                let my = (r.top + r.bottom) / 2;
                fill_c(
                    hdc,
                    &RECT {
                        left: r.left,
                        top: r.top,
                        right: mx,
                        bottom: my,
                    },
                    rgb(230, 40, 40),
                );
                fill_c(
                    hdc,
                    &RECT {
                        left: mx,
                        top: r.top,
                        right: r.right,
                        bottom: my,
                    },
                    rgb(70, 190, 70),
                );
                fill_c(
                    hdc,
                    &RECT {
                        left: r.left,
                        top: my,
                        right: mx,
                        bottom: r.bottom,
                    },
                    rgb(40, 120, 230),
                );
                fill_c(
                    hdc,
                    &RECT {
                        left: mx,
                        top: my,
                        right: r.right,
                        bottom: r.bottom,
                    },
                    rgb(245, 200, 40),
                );
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
    let inner = RECT {
        left: r.left + 1,
        top: r.top + 1,
        right: r.right - 1,
        bottom: r.bottom - 1,
    };
    frame_c(hdc, &inner, c);
}
