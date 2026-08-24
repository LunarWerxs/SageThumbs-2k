//! All overlay drawing: the dimmed backdrop, the selection badge, the hint strip, the
//! annotation layer, and the synthetic canvas the automation route paints instead of a
//! real screen grab.
//!
//! Pure output - it reads [`super::Shot`] and never mutates it.

use super::*;

thread_local! {
    /// The off-screen frame surface `shot_paint` builds each frame into, cached across
    /// paints instead of a fresh `CreateCompatibleDC`+`CreateCompatibleBitmap(vw, vh)` on
    /// EVERY `WM_PAINT` — tens to ~130MB on a multi-4K rig, paid again and again for paints
    /// that `input.rs`'s handlers deliberately kept small (e.g. mousemove invalidates only
    /// the old/new cursor rect). `(dc, bitmap, w, h)`; `w`/`h` are the size it was built at,
    /// so a virtual-screen size change (a monitor plugged/unplugged mid-session) is still
    /// caught and rebuilt rather than blitting into a surface that no longer covers it.
    static FRAME: std::cell::Cell<Option<(HDC, HBITMAP, i32, i32)>> =
        const { std::cell::Cell::new(None) };
}

/// Pure decision logic for `frame_dc`'s cache check, factored out so the "never blit into a
/// surface that no longer covers the frame" rule is testable without a live HDC.
fn frame_cache_hit(cached: Option<(i32, i32)>, vw: i32, vh: i32) -> bool {
    cached == Some((vw, vh))
}

/// The cached frame DC, sized (and kept selected with) a bitmap covering at least the
/// current virtual screen — created on first use, rebuilt only if `vw`/`vh` no longer match.
/// This process is a fresh `--screenshot` launch per capture (see `overlay.rs`), so unlike a
/// long-lived window there is no "session end" to free it at other than process exit, which
/// reclaims it for free — the same assumption the rest of this one-shot process already leans on.
unsafe fn frame_dc(hdc: HDC, vw: i32, vh: i32) -> HDC {
    if let Some((mem, bmp, w, h)) = FRAME.with(|f| f.get()) {
        if frame_cache_hit(Some((w, h)), vw, vh) {
            return mem;
        }
        // Stale size — drop it and fall through to build a fresh one below.
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
    }
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, vw, vh);
    // Selected once, for the surface's whole cached lifetime — there is no per-paint
    // deselect/reselect any more (nothing else ever uses this DC).
    SelectObject(mem, HGDIOBJ(bmp.0));
    FRAME.with(|f| f.set(Some((mem, bmp, vw, vh))));
    mem
}

pub(super) unsafe fn shot_paint(hwnd: HWND) {
    let s = &mut *shot_ptr(hwnd);
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);

    // Build the whole frame off-screen, then blit it once — this is what kills the
    // flicker (the screen was being assembled in several visible steps before).
    let mem = frame_dc(hdc, s.vw, s.vh);

    // Dimmed screen everywhere; the selection shows through at full brightness.
    let _ = BitBlt(mem, 0, 0, s.vw, s.vh, Some(s.dimmed), 0, 0, SRCCOPY);
    let sel = match s.sel {
        Some(r) => r,
        None if s.sel_dragging => tools::norm(s.sel_anchor, s.cur),
        // Idle with a window under the cursor: preview that window's rect exactly as a
        // drag-in-progress paints — bright inside the dim, framed below — so "a click
        // captures THIS" is shown rather than explained.
        None => s.win_hint.unwrap_or(RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        }),
    };
    if sel.right > sel.left && sel.bottom > sel.top {
        let _ = BitBlt(
            mem,
            sel.left,
            sel.top,
            sel.right - sel.left,
            sel.bottom - sel.top,
            Some(s.shot),
            sel.left,
            sel.top,
            SRCCOPY,
        );
    }

    // Annotations + the move-selection highlight + the in-progress shape + caret.
    // The overlay paints in screen space, so no coordinate offset (0, 0). Clip them
    // to the committed selection so the live preview matches the cropped output —
    // nothing drawn "into the void" outside the capture region (which would vanish
    // on copy/save). SaveDC/RestoreDC brackets the clip so the UI chrome below is
    // unclipped.
    let dc_state = SaveDC(mem);
    if let Some(r) = s.sel {
        let _ = IntersectClipRect(mem, r.left, r.top, r.right, r.bottom);
    }
    for sh in &s.shapes {
        tools::draw_shape(mem, 0, 0, sh);
    }
    if let Some(sh) = s.selected.and_then(|i| s.shapes.get(i)) {
        let bb = tools::shape_bbox(sh);
        let r = RECT {
            left: bb.left - 3,
            top: bb.top - 3,
            right: bb.right + 3,
            bottom: bb.bottom + 3,
        };
        tools::frame(mem, r, rgb(0, 200, 90), 1);
    }
    if let Some(a) = s.draw_from {
        let shift = shift_active(s);
        let b = tools::drag_endpoint(s.tool, a, s.cur, shift);
        tools::draw_inprogress(mem, 0, 0, s.tool, a, b, s.color(), s.thickness, &s.pen_pts);
    }
    if let Some((at, buf)) = &s.typing {
        tools::draw_text(mem, 0, 0, *at, buf, s.color(), &s.text_font, true);
    }
    let _ = RestoreDC(mem, dc_state);

    // Selection outline + the floating toolbar (once committed) + the hint strip.
    if sel.right > sel.left && sel.bottom > sel.top {
        tools::frame(mem, sel, rgb(0, 174, 255), 1);
    }
    // Live "W × H" pixel readout while dragging the region, so the user can size things
    // precisely. Drag-only: once committed, the toolbar + hint strip take over the space.
    if s.sel_dragging && sel.right > sel.left && sel.bottom > sel.top {
        draw_dim_badge(mem, s, sel);
    }
    if let Some(committed) = s.sel {
        let dpi = shot_dpi_for_sel(s, committed);
        let buttons = toolbar::layout(committed, s.vw, s.vh, dpi);
        toolbar::draw(mem, &buttons, s.tool, s.color(), dpi);
        if s.color_flyout {
            // The colour palette flyout (takes precedence over a tooltip).
            if let Some((_, cbr)) = buttons.iter().find(|(b, _)| *b == Button::Color) {
                let (panel, sw) = toolbar::color_flyout_layout(*cbr, s.vw, s.vh, &s.customs, dpi);
                toolbar::draw_color_flyout(mem, panel, &sw, s.color());
            }
        } else if s.text_flyout {
            // The text settings flyout.
            if let Some((_, tbr)) = buttons.iter().find(|(b, _)| *b == Button::Tool(Tool::Text)) {
                let (panel, its) =
                    toolbar::text_flyout_layout(*tbr, s.vw, s.vh, s.font_dropdown, dpi);
                toolbar::draw_text_flyout(mem, panel, &its, &s.text_font, dpi);
            }
        } else if s.tip_show {
            // Hover tooltip (after the short delay) over the hovered button.
            if let Some(btn) = s.hover_btn {
                if let Some((_, r)) = buttons.iter().find(|(b, _)| *b == btn) {
                    toolbar::draw_tooltip(mem, *r, toolbar::button_tip(btn), s.vw, s.vh, dpi);
                }
            }
        }
    }
    draw_hint(mem, s);

    // The Eyedropper magnifier follows the cursor, on top of everything. Sized for
    // the monitor under the cursor (committed selection if there is one, else the
    // cursor point); drawn from the bright snapshot so the zoom shows true colours.
    if s.tool == Tool::Eyedropper {
        let dpi = match s.sel {
            Some(sel) => shot_dpi_for_sel(s, sel),
            None => shot_dpi_for_sel(
                s,
                RECT {
                    left: s.cur.x,
                    top: s.cur.y,
                    right: s.cur.x + 1,
                    bottom: s.cur.y + 1,
                },
            ),
        };
        draw_loupe(
            mem,
            s.shot,
            s.cur.x,
            s.cur.y,
            s.vw,
            s.vh,
            dpi,
            s.eye_copied,
            s.automation.is_some(),
        );
    }

    // One blit to the window — only the actually-invalidated rect (`ps.rcPaint`, from
    // BeginPaint above), since `mem` is cached now: a small invalidate (e.g. mousemove's
    // old/new cursor rect) no longer pays to move the whole virtual-screen frame either.
    let pr = ps.rcPaint;
    let (pw, ph) = (pr.right - pr.left, pr.bottom - pr.top);
    if pw > 0 && ph > 0 {
        let _ = BitBlt(
            hdc,
            pr.left,
            pr.top,
            pw,
            ph,
            Some(mem),
            pr.left,
            pr.top,
            SRCCOPY,
        );
    }
    let _ = EndPaint(hwnd, &ps);
    if s.automation.is_some() {
        let _ = GdiFlush();
        publish_automation_title(hwnd, s);
    }
}

/// Paint an opaque, unmistakably synthetic full-virtual-screen surface for UI
/// automation. The background fill happens first and covers every pixel; no live
/// desktop BitBlt occurs anywhere on this route.
pub(super) unsafe fn draw_automation_canvas(dc: HDC, w: i32, h: i32) {
    let full = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    let bg = CreateSolidBrush(rgb(232, 238, 245));
    FillRect(dc, &full, bg);
    let _ = DeleteObject(bg.into());

    // Broad alternating bands plus a regular grid make line angle/length changes
    // easy to see in an external screenshot at any desktop resolution.
    let band_h = (h / 4).max(1);
    for (i, color) in [
        rgb(221, 234, 244),
        rgb(236, 230, 246),
        rgb(226, 242, 232),
        rgb(247, 237, 220),
    ]
    .into_iter()
    .enumerate()
    {
        let top = i as i32 * band_h;
        let bottom = if i == 3 { h } else { (top + band_h).min(h) };
        let brush = CreateSolidBrush(color);
        FillRect(
            dc,
            &RECT {
                left: 0,
                top,
                right: w,
                bottom,
            },
            brush,
        );
        let _ = DeleteObject(brush.into());
    }

    let grid = CreateSolidBrush(rgb(185, 197, 210));
    for x in (0..w).step_by(80) {
        FillRect(
            dc,
            &RECT {
                left: x,
                top: 0,
                right: (x + 1).min(w),
                bottom: h,
            },
            grid,
        );
    }
    for y in (0..h).step_by(80) {
        FillRect(
            dc,
            &RECT {
                left: 0,
                top: y,
                right: w,
                bottom: (y + 1).min(h),
            },
            grid,
        );
    }
    let _ = DeleteObject(grid.into());

    let banner = RECT {
        left: 32.min(w),
        top: 32.min(h),
        right: (w - 32).max(32.min(w)),
        bottom: 104.min(h),
    };
    let banner_bg = CreateSolidBrush(rgb(28, 43, 59));
    FillRect(dc, &banner, banner_bg);
    let _ = DeleteObject(banner_bg.into());
    let banner_border = CreateSolidBrush(rgb(0, 174, 255));
    FrameRect(dc, &banner, banner_border);
    let _ = DeleteObject(banner_border.into());

    SelectObject(dc, HGDIOBJ(gui_font().0));
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, rgb(245, 248, 252));
    let title = wide("SYNTHETIC FULL-SCREEN AUTOMATION CANVAS");
    let _ = TextOutW(
        dc,
        banner.left + 18,
        banner.top + 16,
        &title[..title.len().saturating_sub(1)],
    );
    SetTextColor(dc, rgb(175, 205, 230));
    let subtitle =
        wide("Safe test surface: no desktop pixels, clipboard, files, dialogs, or uploads");
    let _ = TextOutW(
        dc,
        banner.left + 18,
        banner.top + 42,
        &subtitle[..subtitle.len().saturating_sub(1)],
    );
}

/// Alpha-blend a ~55%-opacity black layer over `dc` (a 1×1 black source stretched
/// over the whole area) — turning the snapshot copy into the dimmed surround.
pub(super) unsafe fn apply_dim(dc: HDC, w: i32, h: i32) {
    let tmp = CreateCompatibleDC(Some(dc));
    let bmp = CreateCompatibleBitmap(dc, 1, 1);
    let old = SelectObject(tmp, HGDIOBJ(bmp.0));
    let br = CreateSolidBrush(rgb(0, 0, 0));
    FillRect(
        tmp,
        &RECT {
            left: 0,
            top: 0,
            right: 1,
            bottom: 1,
        },
        br,
    );
    let _ = DeleteObject(br.into());
    let bf = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 140,
        AlphaFormat: 0,
    };
    let _ = AlphaBlend(dc, 0, 0, w, h, tmp, 0, 0, 1, 1, bf);
    SelectObject(tmp, old);
    let _ = DeleteObject(HGDIOBJ(bmp.0));
    let _ = DeleteDC(tmp);
}

/// A small "W × H" pixel-size readout drawn while the region is being dragged, so the
/// user can gauge the exact size of what they're capturing. The overlay maps 1:1 to the
/// virtual screen, so `sel`'s width/height ARE the true output pixel dimensions — no DPI
/// conversion on the numbers (only the chip's own chrome scales). Styled like the hint
/// strip (dark chip, light text) with a hairline border for legibility over any content;
/// anchored at the selection's top-left, just above the rect (or just inside it when the
/// region hugs the top of the screen).
pub(super) unsafe fn draw_dim_badge(hdc: HDC, s: &Shot, sel: RECT) {
    let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
    let dpi = shot_dpi_for_sel(s, sel);
    let txt = format!("{w} \u{00d7} {h}");
    SelectObject(hdc, HGDIOBJ(gui_font().0));
    // Measure the text so the chip hugs it (DT_CALCRECT writes the extent into `calc`).
    let mut buf = wide(&txt);
    let n = buf.len().saturating_sub(1);
    let mut calc = RECT::default();
    DrawTextW(
        hdc,
        &mut buf[..n],
        &mut calc,
        DT_CALCRECT | DT_SINGLELINE | DT_LEFT,
    );
    let padx = crate::win::dpi_scale_dpi(8, dpi);
    let pady = crate::win::dpi_scale_dpi(3, dpi);
    let gap = crate::win::dpi_scale_dpi(6, dpi);
    let bw = (calc.right - calc.left) + padx * 2;
    let bh = (calc.bottom - calc.top) + pady * 2;
    let bx = sel.left.min(s.vw - bw).max(0);
    let by = if sel.top - bh - gap >= 0 {
        sel.top - bh - gap // just above the selection
    } else {
        (sel.top + gap).min(s.vh - bh).max(0) // no room above → just inside the top
    };
    let bar = RECT {
        left: bx,
        top: by,
        right: bx + bw,
        bottom: by + bh,
    };
    let bg = CreateSolidBrush(rgb(20, 20, 20));
    FillRect(hdc, &bar, bg);
    let _ = DeleteObject(bg.into());
    let border = CreateSolidBrush(rgb(90, 90, 90));
    FrameRect(hdc, &bar, border);
    let _ = DeleteObject(border.into());
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(235, 235, 235));
    let mut tr = RECT {
        left: bx + padx,
        top: by,
        right: bx + bw,
        bottom: by + bh,
    };
    DrawTextW(
        hdc,
        &mut buf[..n],
        &mut tr,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE,
    );
}

/// The instructional strip. Pinned to the selection's top-left once a region is
/// committed (so it's right by what you're working on, not stranded in the screen
/// corner); shown at the screen corner during the initial drag.
pub(super) unsafe fn draw_hint(hdc: HDC, s: &Shot) {
    let c = s.cur_color.0;
    let (cr, cg, cb) = (c & 0xFF, (c >> 8) & 0xFF, (c >> 16) & 0xFF);
    // `[ ]` controls text size for the Text tool, line thickness otherwise.
    let sz = if s.tool == Tool::Text {
        format!("text {}", -s.text_font.lfHeight)
    } else {
        format!("size {}", s.thickness)
    };
    let txt = if s.sel.is_none() {
        match (s.ocr_mode, s.sel_dragging) {
            // OCR launch mode: the drag is the whole interaction, so say what it does.
            (true, false) => crate::win::t("shot_hint_ocr_drag").to_string(),
            (true, true) => crate::win::t("shot_hint_ocr_release").to_string(),
            (false, false) => crate::win::t("shot_hint_drag").to_string(),
            (false, true) => crate::win::t("shot_hint_release").to_string(),
        }
    } else {
        let snap = if matches!(s.tool, Tool::Line | Tool::Arrow) {
            if let Some(state) = s.automation.as_ref() {
                if state.forced_shift {
                    "  ·  F8 snap 45° ON"
                } else {
                    "  ·  F8 snap 45° OFF"
                }
            } else {
                "  ·  Shift snaps 45°"
            }
        } else {
            ""
        };
        format!(
            "[{tool}]  ·  [ ] {sz}  ·  #{cr:02X}{cg:02X}{cb:02X}{snap}  ·  Ctrl-drag moves  ·  Enter copy  ·  Ctrl+T text  ·  Ctrl+S save  ·  Esc close",
            tool = s.tool.label(),
        )
    };
    // Size the strip for the monitor it sits on: the selection's monitor once
    // committed, else the monitor under the in-progress drag (or the cursor before a
    // drag). Falls back to 96 (identity), so a standard display is unchanged.
    let dpi = match s.sel {
        Some(sel) => shot_dpi_for_sel(s, sel),
        None if s.sel_dragging => shot_dpi_for_sel(s, tools::norm(s.sel_anchor, s.cur)),
        None => shot_dpi_for_sel(
            s,
            RECT {
                left: s.cur.x,
                top: s.cur.y,
                right: s.cur.x + 1,
                bottom: s.cur.y + 1,
            },
        ),
    };
    // Wide enough for the longest hint string (the committed-selection one, which grew
    // with Ctrl+T) — a short bar leaves the tail of the text spilling onto the capture.
    let bar_w = s.vw.min(crate::win::dpi_scale_dpi(1080, dpi));
    let bar_h = crate::win::dpi_scale_dpi(26, dpi);
    let gap = crate::win::dpi_scale_dpi(6, dpi); // gap above the selection
    let inset = crate::win::dpi_scale_dpi(4, dpi); // inset when there's no room above
                                                   // Anchor to the selection's top-left if committed; else the screen corner.
    let (bx, by) = match s.sel {
        Some(sel) => {
            let x = sel.left.min(s.vw - bar_w).max(0);
            let y = if sel.top - bar_h - gap >= 0 {
                sel.top - bar_h - gap // just above the selection
            } else {
                (sel.top + inset).min(s.vh - bar_h) // no room above → just inside the top
            };
            (x, y)
        }
        None => (0, 0),
    };
    let bg = CreateSolidBrush(rgb(20, 20, 20));
    let bar = RECT {
        left: bx,
        top: by,
        right: bx + bar_w,
        bottom: by + bar_h,
    };
    FillRect(hdc, &bar, bg);
    let _ = DeleteObject(bg.into());
    SelectObject(hdc, HGDIOBJ(gui_font().0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, rgb(235, 235, 235));
    let w = wide(&txt);
    let tx = crate::win::dpi_scale_dpi(10, dpi); // text left padding
    let ty = crate::win::dpi_scale_dpi(5, dpi); // text top padding
    let _ = TextOutW(hdc, bx + tx, by + ty, &w[..w.len().saturating_sub(1)]);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// No cached surface at all (the first paint of a session) must always rebuild.
    #[test]
    fn frame_cache_hit_is_false_when_nothing_is_cached() {
        assert!(!frame_cache_hit(None, 1920, 1080));
    }

    /// A cached surface exactly the current virtual-screen size is reused.
    #[test]
    fn frame_cache_hit_is_true_when_the_cached_size_matches() {
        assert!(frame_cache_hit(Some((1920, 1080)), 1920, 1080));
    }

    /// The bug this fix must never reintroduce: blitting into a surface built for a
    /// DIFFERENT (smaller) virtual screen than the one being painted now — a monitor
    /// plugged in mid-session grows `vw`/`vh`, and reusing the old, too-small bitmap would
    /// either fail the blit or silently crop the frame instead of rebuilding.
    #[test]
    fn frame_cache_hit_is_false_when_the_virtual_screen_size_changed() {
        assert!(!frame_cache_hit(Some((1920, 1080)), 3840, 2160));
        assert!(!frame_cache_hit(Some((1920, 1080)), 1920, 1081)); // height-only mismatch
        assert!(!frame_cache_hit(Some((1920, 1080)), 1921, 1080)); // width-only mismatch
    }
}
