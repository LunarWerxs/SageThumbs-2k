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
    let panel = super::anchor_panel(anchor, vw, vh, pw, ph, off);
    let x = panel.left;
    let y = panel.top;
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

/// The Bold/Underline row caption: a plain-text checkbox glyph plus the localized name
/// (audit F29, 2026-09-06) - the pre-fix code hardcoded "Bold"/"Underline" so the checkbox
/// never varied with the active language.
pub(crate) fn checkbox_label(checked: bool, name_key: &str) -> String {
    let mark = if checked { "[x]" } else { "[  ]" };
    format!("{mark}  {}", crate::win::t(name_key))
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

/// Draw a left-aligned checkbox caption (`[x] Name`) in row `r`.
unsafe fn draw_checkbox_row(hdc: HDC, r: &RECT, checked: bool, name_key: &str) {
    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetTextColor(hdc, rgb(235, 235, 235));
    let label = checkbox_label(checked, name_key);
    let mut tr = RECT {
        left: r.left + 4,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    };
    let mut w = wide(&label);
    let n = w.len().saturating_sub(1);
    DrawTextW(
        hdc,
        &mut w[..n],
        &mut tr,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
}

/// Paint the text settings flyout for the current `font`. `dpi` scales the design
/// pixels (identity at 96).
///
/// `focus` is the index (into `items`) of the keyboard-focused row, or `None`. Its ring is
/// drawn INSIDE the row: the font dropdown's option rows abut with no gap between them, so
/// an outset ring would spill onto the row above and below and read as three focused items.
pub(crate) unsafe fn draw_text_flyout(
    hdc: HDC,
    panel: RECT,
    items: &[(TextItem, RECT)],
    font: &LOGFONTW,
    dpi: i32,
    focus: Option<usize>,
) {
    super::draw_panel_bg(hdc, &panel);

    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    let cur_face = face_name(font);
    let size = -font.lfHeight;
    let underline = font.lfUnderline != 0;
    let bold = font.lfWeight >= 700;

    let (down, up) = size_button_rects(items);

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
            TextItem::Bold => draw_checkbox_row(hdc, r, bold, "shot_text_bold"),
            TextItem::Underline => draw_checkbox_row(hdc, r, underline, "shot_text_underline"),
            TextItem::More => draw_btn(hdc, *r, crate::win::t("shot_text_more_fonts")),
        }
    }

    // After every row has painted itself, so the ring is never half-covered.
    if let Some((_, r)) = focus.and_then(|i| items.get(i)) {
        super::draw_focus_ring_inside(hdc, *r);
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

/// Locate the SizeDown/SizeUp rows the size readout is centred between.
fn size_button_rects(items: &[(TextItem, RECT)]) -> (RECT, RECT) {
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
    (down, up)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A Text button comfortably below the middle of a large screen, so the flyout opens
    /// above it and nothing is clamped.
    fn anchor() -> RECT {
        RECT {
            left: 100,
            top: 800,
            right: 200,
            bottom: 828,
        }
    }

    fn rect_of(items: &[(TextItem, RECT)], want: TextItem) -> RECT {
        items
            .iter()
            .find(|(it, _)| *it == want)
            .map(|(_, r)| *r)
            .unwrap()
    }

    fn option_rows(items: &[(TextItem, RECT)]) -> Vec<RECT> {
        items
            .iter()
            .filter_map(|(it, r)| match it {
                TextItem::FontOption(_) => Some(*r),
                _ => None,
            })
            .collect()
    }

    /// The collapsed flyout is the fixed chrome only: font field, the two size buttons,
    /// the Bold/Underline halves and the "More" escape hatch, in that painting order.
    #[test]
    fn the_closed_flyout_carries_the_six_chrome_rows_and_no_font_options() {
        let (_panel, items) = text_flyout_layout(anchor(), 1920, 1080, false, 96);
        assert_eq!(items.len(), 6);
        assert!(items
            .iter()
            .all(|(it, _)| !matches!(it, TextItem::FontOption(_))));
        assert!(items[0].0 == TextItem::FontField);
        assert!(items[1].0 == TextItem::SizeDown);
        assert!(items[2].0 == TextItem::SizeUp);
        assert!(items[3].0 == TextItem::Bold);
        assert!(items[4].0 == TextItem::Underline);
        assert!(items[5].0 == TextItem::More);
    }

    /// Opening the dropdown adds exactly one clickable option per preset font, indexed in
    /// order, and grows the panel by the option block's height (dropdown rows + their two
    /// inset pads) without changing its width.
    #[test]
    fn the_open_dropdown_adds_one_row_per_preset_font_and_grows_the_panel() {
        let (closed, closed_items) = text_flyout_layout(anchor(), 1920, 1080, false, 96);
        let (open, open_items) = text_flyout_layout(anchor(), 1920, 1080, true, 96);
        let nf = PRESET_FONTS.len() as i32;
        assert_eq!(open_items.len(), closed_items.len() + PRESET_FONTS.len());

        let indices: Vec<usize> = open_items
            .iter()
            .filter_map(|(it, _)| match it {
                TextItem::FontOption(i) => Some(*i),
                _ => None,
            })
            .collect();
        assert_eq!(indices, (0..PRESET_FONTS.len()).collect::<Vec<_>>());

        let inset = crate::win::dpi_scale_dpi(2, 96);
        let drop_h = nf * crate::win::dpi_scale_dpi(TF_OPT, 96) + inset * 2;
        assert_eq!(
            (open.bottom - open.top) - (closed.bottom - closed.top),
            drop_h
        );
        assert_eq!(open.right - open.left, closed.right - closed.left);
    }

    /// The dropdown rows abut with no vertical gap: the focus ring is drawn INSIDE the
    /// focused row precisely because an outset ring would spill onto both neighbours and
    /// read as three focused items. All rows also share the same inner column.
    #[test]
    fn font_option_rows_abut_so_a_focus_ring_stays_inside_its_row() {
        let (_panel, items) = text_flyout_layout(anchor(), 1920, 1080, true, 96);
        let rows = option_rows(&items);
        assert!(rows.len() > 1);
        for pair in rows.windows(2) {
            assert_eq!(pair[0].bottom, pair[1].top, "dropdown rows must abut");
            assert_eq!(pair[0].left, pair[1].left);
            assert_eq!(pair[0].right, pair[1].right);
        }
    }

    /// Bold and Underline share one row as equal halves: each takes half the inner width,
    /// they do not overlap, and together they still span exactly the font field's column.
    #[test]
    fn bold_and_underline_split_the_row_and_span_the_font_field_width() {
        let (_panel, items) = text_flyout_layout(anchor(), 1920, 1080, false, 96);
        let field = rect_of(&items, TextItem::FontField);
        let bold = rect_of(&items, TextItem::Bold);
        let under = rect_of(&items, TextItem::Underline);
        assert_eq!(bold.top, under.top);
        assert_eq!(bold.bottom, under.bottom);
        assert_eq!(bold.left, field.left);
        assert_eq!(under.right, field.right);
        assert_eq!(bold.right - bold.left, under.right - under.left);
        assert!(bold.right <= under.left, "the two captions overlap");
    }

    /// A flyout opened against the right edge and with no room above must be pulled back
    /// on-screen and flipped below its button — an off-screen panel is unreachable.
    #[test]
    fn the_panel_is_clamped_back_onto_a_small_screen() {
        let anchor = RECT {
            left: 350,
            top: 250,
            right: 380,
            bottom: 265,
        };
        let (panel, _) = text_flyout_layout(anchor, 400, 600, true, 96);
        assert_eq!(
            panel.right, 400,
            "the panel must not overhang the right edge"
        );
        assert_eq!(panel.left, 200);
        assert!(
            panel.top >= anchor.bottom,
            "with no room above, the panel must flip below the button"
        );
        assert!(panel.bottom <= 600);
        assert!(panel.left >= 0 && panel.top >= 0);
    }

    /// Every design pixel scales with the DPI: the panel and every item offset from the
    /// panel origin double when the scale doubles. At 96 the layout is the design itself.
    #[test]
    fn the_whole_layout_scales_with_the_dpi() {
        let (p96, i96) = text_flyout_layout(anchor(), 1920, 1080, true, 96);
        let (p192, i192) = text_flyout_layout(anchor(), 1920, 1080, true, 192);
        assert_eq!(p96.right - p96.left, crate::win::dpi_scale_dpi(TF_PW, 96));
        assert_eq!(i96.len(), i192.len());
        assert_eq!(p192.right - p192.left, 2 * (p96.right - p96.left));
        assert_eq!(p192.bottom - p192.top, 2 * (p96.bottom - p96.top));
        for ((t96, r96), (t192, r192)) in i96.iter().zip(i192.iter()) {
            assert!(t96 == t192);
            assert_eq!(r192.left - p192.left, 2 * (r96.left - p96.left));
            assert_eq!(r192.top - p192.top, 2 * (r96.top - p96.top));
            assert_eq!(r192.right - p192.left, 2 * (r96.right - p96.left));
            assert_eq!(r192.bottom - p192.top, 2 * (r96.bottom - p96.top));
        }
    }

    /// `size_button_rects` finds the two stepper rows wherever they sit, and answers with
    /// the zero rect when a list has none (the caller then skips the centred readout
    /// rather than drawing it against a stale rect).
    #[test]
    fn size_button_rects_locates_the_size_rows_and_defaults_when_absent() {
        let items = vec![
            (TextItem::FontField, RECT::default()),
            (
                TextItem::SizeDown,
                RECT {
                    left: 10,
                    top: 20,
                    right: 30,
                    bottom: 40,
                },
            ),
            (TextItem::Bold, RECT::default()),
            (
                TextItem::SizeUp,
                RECT {
                    left: 50,
                    top: 20,
                    right: 70,
                    bottom: 40,
                },
            ),
        ];
        let (down, up) = size_button_rects(&items);
        assert_eq!(
            down,
            RECT {
                left: 10,
                top: 20,
                right: 30,
                bottom: 40
            }
        );
        assert_eq!(
            up,
            RECT {
                left: 50,
                top: 20,
                right: 70,
                bottom: 40
            }
        );

        let none: Vec<(TextItem, RECT)> = Vec::new();
        let (down, up) = size_button_rects(&none);
        assert_eq!(down, RECT::default());
        assert_eq!(up, RECT::default());
    }

    /// The checkbox caption is the state glyph plus the localized name looked up from the
    /// locale table (audit F29) — never the raw key echoed back.
    #[test]
    fn checkbox_label_marks_the_state_and_reads_the_localized_name() {
        st2k_base::i18n::ensure_init(); // first, so its one-time pick cannot undo "en"
        st2k_base::i18n::apply_override_or_system(Some("en"));
        let name = crate::win::t("shot_text_bold");
        assert!(!name.is_empty(), "the key must resolve to a real caption");
        assert_eq!(
            checkbox_label(true, "shot_text_bold"),
            format!("[x]  {name}")
        );
        assert_eq!(
            checkbox_label(false, "shot_text_bold"),
            format!("[  ]  {name}")
        );
        assert!(
            !checkbox_label(true, "shot_text_bold").contains("shot_text_bold"),
            "the key leaked into the caption instead of being translated"
        );
    }
}
