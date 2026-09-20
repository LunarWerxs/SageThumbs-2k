//! The eyedropper magnifier.
//!
//! A port of the standalone `--eyedropper` picker's loupe into the overlay. We cannot
//! just launch that tool from here: it would snapshot the *dimmed* overlay and pick
//! washed-out colours. Instead the same magnifier is drawn from the overlay's own
//! BRIGHT snapshot, so the zoomed pixels and the sampled colour are both true.

use super::*;

/// The Eyedropper: read the colour of the frozen-screenshot pixel under `p`, make it
/// the active drawing colour, and normally copy `#RRGGBB` to the clipboard. The
/// synthetic automation route samples locally but deliberately leaves the clipboard
/// untouched. Client coords map 1:1 to the snapshot (the overlay spans the virtual
/// screen from its top-left), so we read straight from `s.shot`. A `CLR_INVALID` read
/// (cursor past the bitmap edge) is ignored.
pub(super) unsafe fn sample_pixel(s: &mut Shot, p: POINT) {
    let c = GetPixel(s.shot, p.x, p.y);
    if c.0 == 0xFFFF_FFFF {
        return; // CLR_INVALID — outside the snapshot
    }
    s.cur_color = c;
    if let Some(state) = s.automation.as_mut() {
        state.status = "sampled-no-clipboard";
    } else {
        let (r, g, b) = (c.0 & 0xFF, (c.0 >> 8) & 0xFF, (c.0 >> 16) & 0xFF);
        let _ = crate::win::set_clipboard_text(&format!("#{r:02X}{g:02X}{b:02X}"));
    }
    s.eye_copied = true; // flash the loupe confirmation until the next move
}

// ---- Eyedropper magnifier loupe --------------------------------------------
// A port of the standalone `--eyedropper` picker's loupe into the overlay. We
// can't just launch that tool here: it would snapshot the *dimmed* overlay and
// pick washed-out colours. Instead we draw the same magnifier from the overlay's
// own BRIGHT snapshot (`s.shot`) so the zoomed pixels + sampled colour are true.

pub(super) const LOUPE_K: i32 = 7; // half-window: a (2K+1)² block of screen pixels shown
pub(super) const LOUPE_SPAN: i32 = 2 * LOUPE_K + 1; // 15 px sampled across
pub(super) const LOUPE_MAG: i32 = 150; // magnified loupe size (design px) → 10× zoom
pub(super) const LOUPE_LBL: i32 = 46; // label strip height (design px): hex row + hint row

/// Sample `(r, g, b)` from the bright snapshot DC at `(x, y)` (clamped in-bounds).
pub(super) unsafe fn shot_sample(shot: HDC, x: i32, y: i32, vw: i32, vh: i32) -> (u8, u8, u8) {
    let x = x.clamp(0, (vw - 1).max(0));
    let y = y.clamp(0, (vh - 1).max(0));
    let c = GetPixel(shot, x, y).0; // 0x00BBGGRR, or CLR_INVALID
    crate::eyedropper::colorref_to_rgb(c)
}

/// The loupe box (magnifier + label strip) for a cursor at `(cx, cy)`, nudged to
/// stay fully on the virtual screen. `mag`/`lbl`/`gap` are already DPI-scaled.
pub(super) fn loupe_box(cx: i32, cy: i32, vw: i32, vh: i32, mag: i32, lbl: i32, gap: i32) -> RECT {
    crate::eyedropper::nudge_box(cx, cy, vw, vh, mag, mag + lbl, gap)
}

/// The on-screen rect the loupe occupies for a cursor at `(cx, cy)` — used to
/// invalidate just the old/new loupe area on each move instead of the whole screen.
pub(super) unsafe fn loupe_rect(s: &Shot, cx: i32, cy: i32) -> RECT {
    let dpi = match s.sel {
        Some(sel) => shot_dpi_for_sel(s, sel),
        None => shot_dpi_for_sel(
            s,
            RECT {
                left: cx,
                top: cy,
                right: cx + 1,
                bottom: cy + 1,
            },
        ),
    };
    let mag = crate::win::dpi_scale_dpi(LOUPE_MAG, dpi);
    let lbl = crate::win::dpi_scale_dpi(LOUPE_LBL, dpi);
    let gap = crate::win::dpi_scale_dpi(18, dpi);
    loupe_box(cx, cy, s.vw, s.vh, mag, lbl, gap)
}

/// Draw the magnifier loupe near `(cx, cy)` into `hdc`, zooming the bright `shot`.
/// Mirrors the standalone picker: nearest-neighbour 10× block, a red ring on the
/// picked pixel, then a swatch + `#RRGGBB` + status hint. `copied` flips the hint to
/// a confirmation right after a pick; automation calls it "sampled" because its
/// clipboard fence is active.
// Geometry + state are all distinct scalars the GDI draw needs; bundling them into a
// struct would just move the arg list, so allow the count here.
#[allow(clippy::too_many_arguments)]
pub(super) unsafe fn draw_loupe(
    hdc: HDC,
    shot: HDC,
    cx: i32,
    cy: i32,
    vw: i32,
    vh: i32,
    dpi: i32,
    copied: bool,
    sampled_only: bool,
) {
    let mag = crate::win::dpi_scale_dpi(LOUPE_MAG, dpi);
    let lbl = crate::win::dpi_scale_dpi(LOUPE_LBL, dpi);
    let gap = crate::win::dpi_scale_dpi(18, dpi);
    let pad = crate::win::dpi_scale_dpi(5, dpi);
    let swsz = crate::win::dpi_scale_dpi(16, dpi);
    let lb = loupe_box(cx, cy, vw, vh, mag, lbl, gap);
    let (bx, by) = (lb.left, lb.top);

    // The LOUPE_SPAN² sample window around the cursor, shifted to stay fully inside
    // the snapshot so StretchBlt never reads out of bounds (cursor at a screen edge).
    // `kx`/`ky` are the cursor pixel's cell within that (possibly shifted) window.
    let sx = (cx - LOUPE_K).clamp(0, (vw - LOUPE_SPAN).max(0));
    let sy = (cy - LOUPE_K).clamp(0, (vh - LOUPE_SPAN).max(0));
    let kx = (cx - sx).clamp(0, LOUPE_SPAN - 1);
    let ky = (cy - sy).clamp(0, LOUPE_SPAN - 1);

    // Magnified pixels — nearest-neighbour so each screen pixel is a crisp block.
    SetStretchBltMode(hdc, COLORONCOLOR);
    let _ = StretchBlt(
        hdc,
        bx,
        by,
        mag,
        mag,
        Some(shot),
        sx,
        sy,
        LOUPE_SPAN,
        LOUPE_SPAN,
        SRCCOPY,
    );

    // Red ring on the cursor's cell (the pixel that gets picked). Boundaries are taken
    // per-edge from the StretchBlt grid so the ring stays aligned at any DPI.
    let cc = RECT {
        left: bx + kx * mag / LOUPE_SPAN,
        top: by + ky * mag / LOUPE_SPAN,
        right: bx + (kx + 1) * mag / LOUPE_SPAN,
        bottom: by + (ky + 1) * mag / LOUPE_SPAN,
    };
    let red = CreateSolidBrush(rgb(255, 40, 40));
    FrameRect(hdc, &cc, red);
    let _ = DeleteObject(red.into());

    // Label strip: swatch + hex (row 1), then a status hint (row 2).
    let (r, g, b) = shot_sample(shot, cx, cy, vw, vh);
    let strip = RECT {
        left: bx,
        top: by + mag,
        right: bx + mag,
        bottom: by + mag + lbl,
    };
    let lbg = CreateSolidBrush(rgb(24, 24, 24));
    FillRect(hdc, &strip, lbg);
    let _ = DeleteObject(lbg.into());
    let sw = RECT {
        left: bx + pad,
        top: by + mag + pad,
        right: bx + pad + swsz,
        bottom: by + mag + pad + swsz,
    };
    let swb = CreateSolidBrush(rgb(r, g, b));
    FillRect(hdc, &sw, swb);
    let _ = DeleteObject(swb.into());

    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(240, 240, 240));
    let mut hex = wide(&format!("#{r:02X}{g:02X}{b:02X}"));
    let hn = hex.len().saturating_sub(1);
    let mut hr = RECT {
        left: bx + pad * 2 + swsz,
        top: by + mag,
        right: bx + mag,
        bottom: by + mag + swsz + pad * 2,
    };
    DrawTextW(
        hdc,
        &mut hex[..hn],
        &mut hr,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
    let (hint_txt, hint_col) = match (sampled_only, copied) {
        (true, true) => ("Sampled \u{2713}", rgb(120, 220, 120)),
        (true, false) => ("Click to sample", rgb(150, 150, 150)),
        (false, true) => ("Copied \u{2713}", rgb(120, 220, 120)),
        (false, false) => ("Click to copy", rgb(150, 150, 150)),
    };
    SetTextColor(hdc, hint_col);
    let mut hint = wide(hint_txt);
    let hin = hint.len().saturating_sub(1);
    let mut hir = RECT {
        left: bx + pad,
        top: by + mag + swsz + pad,
        right: bx + mag,
        bottom: by + mag + lbl,
    };
    DrawTextW(
        hdc,
        &mut hint[..hin],
        &mut hir,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );

    // Outer + magnifier borders.
    let border = CreateSolidBrush(rgb(0, 0, 0));
    FrameRect(
        hdc,
        &RECT {
            left: bx,
            top: by,
            right: bx + mag,
            bottom: by + mag + lbl,
        },
        border,
    );
    FrameRect(
        hdc,
        &RECT {
            left: bx,
            top: by,
            right: bx + mag,
            bottom: by + mag,
        },
        border,
    );
    let _ = DeleteObject(border.into());
}
