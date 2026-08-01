//! The text-settings flyout: font dropdown, size stepper, bold/underline toggles
//! and the escape hatch to the native Font dialog.

use super::*;

// ---- text settings flyout --------------------------------------------------

/// A short list of common Windows fonts for the lightweight font dropdown. Anything
/// else is reachable via the "Font… (more)" button → the native Font dialog.
pub(crate) const PRESET_FONTS: &[&str] = &[
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
pub(crate) enum TextItem {
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
pub(crate) fn text_flyout_layout(
    anchor: RECT,
    vw: i32,
    vh: i32,
    dropdown: bool,
    dpi: i32,
) -> (RECT, Vec<(TextItem, RECT)>) {
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
    let panel = RECT {
        left: x,
        top: y,
        right: x + pw,
        bottom: y + ph,
    };
    let ix = x + pad;
    let iw = pw - pad * 2;
    let mut items = Vec::new();
    let mut cy = y + pad;
    items.push((
        TextItem::FontField,
        RECT {
            left: ix,
            top: cy,
            right: ix + iw,
            bottom: cy + row - inset,
        },
    ));
    cy += row;
    if dropdown {
        cy += inset;
        for i in 0..nf {
            items.push((
                TextItem::FontOption(i as usize),
                RECT {
                    left: ix,
                    top: cy,
                    right: ix + iw,
                    bottom: cy + opt,
                },
            ));
            cy += opt;
        }
        cy += inset;
    }
    cy += gap;
    let bw = dpi_scale_dpi(28, dpi);
    items.push((
        TextItem::SizeDown,
        RECT {
            left: ix,
            top: cy,
            right: ix + bw,
            bottom: cy + row - inset,
        },
    ));
    items.push((
        TextItem::SizeUp,
        RECT {
            left: ix + iw - bw,
            top: cy,
            right: ix + iw,
            bottom: cy + row - inset,
        },
    ));
    cy += row + gap;
    // Bold + Underline share a row (each half-width).
    let half = (iw - gap) / 2;
    items.push((
        TextItem::Bold,
        RECT {
            left: ix,
            top: cy,
            right: ix + half,
            bottom: cy + row - inset,
        },
    ));
    items.push((
        TextItem::Underline,
        RECT {
            left: ix + iw - half,
            top: cy,
            right: ix + iw,
            bottom: cy + row - inset,
        },
    ));
    cy += row + gap;
    items.push((
        TextItem::More,
        RECT {
            left: ix,
            top: cy,
            right: ix + iw,
            bottom: cy + row - inset,
        },
    ));
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
    DrawTextW(
        hdc,
        &mut w[..n],
        &mut rr,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
}

/// Paint the text settings flyout for the current `font`. `dpi` scales the design
/// pixels (identity at 96).
pub(crate) unsafe fn draw_text_flyout(
    hdc: HDC,
    panel: RECT,
    items: &[(TextItem, RECT)],
    font: &LOGFONTW,
    dpi: i32,
) {
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
                let mut tr = RECT {
                    left: r.left + dpi_scale_dpi(6, dpi),
                    top: r.top,
                    right: r.right - dpi_scale_dpi(18, dpi),
                    bottom: r.bottom,
                };
                let mut w = wide(&cur_face);
                let n = w.len().saturating_sub(1);
                DrawTextW(
                    hdc,
                    &mut w[..n],
                    &mut tr,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
                let mut cr = RECT {
                    left: r.right - dpi_scale_dpi(16, dpi),
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                };
                let mut wv = wide("\u{25BE}"); // ▾
                let nv = wv.len().saturating_sub(1);
                DrawTextW(
                    hdc,
                    &mut wv[..nv],
                    &mut cr,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
            }
            TextItem::FontOption(i) => {
                let name = PRESET_FONTS[*i];
                if name == cur_face {
                    let b = CreateSolidBrush(rgb(0, 90, 160));
                    FillRect(hdc, r, b);
                    let _ = DeleteObject(b.into());
                }
                let mut lf = LOGFONTW {
                    lfHeight: -dpi_scale_dpi(16, dpi),
                    ..Default::default()
                };
                for (k, c) in wide(name).iter().take(lf.lfFaceName.len() - 1).enumerate() {
                    lf.lfFaceName[k] = *c;
                }
                let hf = CreateFontIndirectW(&lf);
                let old = SelectObject(hdc, HGDIOBJ(hf.0));
                SetTextColor(hdc, rgb(235, 235, 235));
                let mut tr = RECT {
                    left: r.left + 8,
                    top: r.top,
                    right: r.right - 4,
                    bottom: r.bottom,
                };
                let mut w = wide(name);
                let n = w.len().saturating_sub(1);
                DrawTextW(
                    hdc,
                    &mut w[..n],
                    &mut tr,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
                SelectObject(hdc, old);
                let _ = DeleteObject(HGDIOBJ(hf.0));
            }
            TextItem::SizeDown => draw_btn(hdc, *r, "-"),
            TextItem::SizeUp => draw_btn(hdc, *r, "+"),
            TextItem::Bold => {
                SelectObject(hdc, HGDIOBJ(gui_font().0));
                SetTextColor(hdc, rgb(235, 235, 235));
                let label = if bold { "[x]  Bold" } else { "[  ]  Bold" };
                let mut tr = RECT {
                    left: r.left + 4,
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                };
                let mut w = wide(label);
                let n = w.len().saturating_sub(1);
                DrawTextW(
                    hdc,
                    &mut w[..n],
                    &mut tr,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
            }
            TextItem::Underline => {
                SelectObject(hdc, HGDIOBJ(gui_font().0));
                SetTextColor(hdc, rgb(235, 235, 235));
                let label = if underline {
                    "[x]  Underline"
                } else {
                    "[  ]  Underline"
                };
                let mut tr = RECT {
                    left: r.left + 4,
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                };
                let mut w = wide(label);
                let n = w.len().saturating_sub(1);
                DrawTextW(
                    hdc,
                    &mut w[..n],
                    &mut tr,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE,
                );
            }
            TextItem::More => draw_btn(hdc, *r, "Font\u{2026} (more)"),
        }
    }

    // The size value, centred between the − and + buttons.
    if down.right < up.left {
        SelectObject(hdc, HGDIOBJ(gui_font().0));
        SetTextColor(hdc, rgb(255, 255, 255));
        let mut nr = RECT {
            left: down.right,
            top: down.top,
            right: up.left,
            bottom: down.bottom,
        };
        let mut w = wide(&format!("{size} px"));
        let n = w.len().saturating_sub(1);
        DrawTextW(
            hdc,
            &mut w[..n],
            &mut nr,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
        );
    }
}
