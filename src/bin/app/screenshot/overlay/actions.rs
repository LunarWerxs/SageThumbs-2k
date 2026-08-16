//! What happens when a capture is FINISHED: compositing the selected region, and the
//! four things we can then do with it (clipboard, file, OCR helper, upload helper),
//! plus the toolbar-button dispatch that chooses between them.
//!
//! The spawn helpers own a real invariant: a composited region is a picture of the
//! user's screen written to a temp file, so if the helper process fails to start,
//! nothing else would ever clean it up and this module deletes it itself.

use super::*;

/// Intersect `r` with `sel` — both in the same (screen) coordinate space. Used to keep a
/// region-effect shape's rect from reaching past the selection edge when `compose` bakes it
/// into the cropped output (see the comment at its call site). Yields an empty/inverted rect
/// (`right < left` and/or `bottom < top`) when `r` doesn't overlap `sel` at all; callers
/// already treat too-small a rect as a no-op (e.g. `tools::draw_pixelate`'s `w < 4 || h < 4`
/// guard), so that case draws nothing rather than needing a special case here.
fn clamp_rect(r: RECT, sel: RECT) -> RECT {
    RECT {
        left: r.left.max(sel.left),
        top: r.top.max(sel.top),
        right: r.right.min(sel.right),
        bottom: r.bottom.min(sel.bottom),
    }
}

/// Composite the selected region (snapshot + annotations) into an offscreen DC
/// and pull its top-down BGRA pixels. Returns `(pixels, w, h)` — the callers route
/// it to the clipboard and/or a PNG.
pub(super) unsafe fn compose(s: &Shot) -> Option<(Vec<u8>, i32, i32)> {
    let sel = s.sel?;
    let (w, h) = (sel.right - sel.left, sel.bottom - sel.top);
    if w <= 0 || h <= 0 {
        return None;
    }
    let screen = GetDC(None);
    let comp = CreateCompatibleDC(Some(screen));
    let cbmp = CreateCompatibleBitmap(screen, w, h);
    ReleaseDC(None, screen);
    let oldbmp = SelectObject(comp, HGDIOBJ(cbmp.0));
    let _ = BitBlt(comp, 0, 0, w, h, Some(s.shot), sel.left, sel.top, SRCCOPY);
    // Offset the annotations (screen space) into region space. We pass the shift
    // explicitly rather than via SetViewportOrgEx because GDI+ (the anti-aliased
    // drawing) ignores the DC's viewport origin — only plain GDI honours it.
    //
    // The live preview clips its draw to `sel` (paint.rs's SaveDC + IntersectClipRect), but a
    // shape's STORED rect is never bounded to the selection — only its on-screen paint was
    // clipped. `comp` is exactly `sel`'s own w×h, so a rect that reaches past the selection
    // edge asks the region-effect tools to read/write beyond it: Pixelate in particular
    // *samples* pixels via StretchBlt, and a source rect past `comp`'s own bitmap reads
    // whatever GDI's surface clamp gives it — a visual glitch in the saved/copied output.
    // Clamp those rects to `sel` here rather than in `tools::draw_shape`, so every region
    // effect the compose stage bakes in stays inside the bitmap it's actually drawing into.
    for sh in &s.shapes {
        match sh {
            tools::Shape::Pixelate { r } => tools::draw_shape(
                comp,
                -sel.left,
                -sel.top,
                &tools::Shape::Pixelate {
                    r: clamp_rect(*r, sel),
                },
            ),
            tools::Shape::Highlight { r, color } => tools::draw_shape(
                comp,
                -sel.left,
                -sel.top,
                &tools::Shape::Highlight {
                    r: clamp_rect(*r, sel),
                    color: *color,
                },
            ),
            tools::Shape::Invert { r } => tools::draw_shape(
                comp,
                -sel.left,
                -sel.top,
                &tools::Shape::Invert {
                    r: clamp_rect(*r, sel),
                },
            ),
            _ => tools::draw_shape(comp, -sel.left, -sel.top, sh),
        }
    }

    // 64-bit size math (w/h are already > 0 above); bail on an absurd selection so
    // the i32 product can't overflow into an undersized buffer for GetDIBits.
    let n = w as i64 * h as i64 * 4;
    if n > i32::MAX as i64 {
        // Free what we already created; the success path below does, and an early
        // return that does not is a GDI handle leak in a process the user can run
        // hundreds of times a day.
        let _ = DeleteDC(comp);
        let _ = DeleteObject(cbmp.into());
        return None;
    }
    // Pull top-down BGRA (negative biHeight) — shared with capture_instant/capture_hwnd_bgra.
    let buf = window_shot::pull_top_down_bgra(comp, cbmp, w, h, n as usize);
    SelectObject(comp, oldbmp);
    let _ = DeleteDC(comp);
    let _ = DeleteObject(HGDIOBJ(cbmp.0));
    Some((buf?, w, h))
}

/// Copy the composited capture to the clipboard. Returns whether it actually landed there
/// (`true` for the automation no-op too, so callers don't treat a blocked automation run as
/// a failure). (Caller commits in-progress text first.)
pub(super) unsafe fn finish_copy(s: &Shot) -> bool {
    if s.automation.is_some() {
        return true;
    }
    let Some((buf, w, h)) = compose(s) else {
        return false;
    };
    let ok = output::copy_dib_to_clipboard(&buf, w, h);
    if !ok {
        // This used to be silently discarded: every caller (Enter, Ctrl+C, the toolbar Copy
        // button) destroyed the window right after regardless, so a clipboard failure closed
        // the editor with nothing copied and zero feedback. Mirror capture_instant's toast so
        // the failure is at least visible, whichever path triggered it.
        crate::win::notify_toast(
            "SageThumbs 2K",
            crate::win::t("toast_shot_fail_clip"),
            std::time::Duration::from_secs(5),
        );
    }
    ok
}

/// Composite the capture to a throwaway temp PNG and hand it to a helper process
/// (`--upload <png>` / `--ocr <png>`), which owns the file from then on and deletes it
/// once it has read it.
///
/// Out-of-process because both jobs take a beat (a network round-trip; the WinRT OCR engine
/// spinning up) and the overlay is about to be destroyed — doing either here would freeze a
/// fullscreen topmost window while it worked. If the helper never starts we delete the PNG
/// ourselves: it is a picture of the user's screen, and nothing else would ever clean it up.
pub(super) unsafe fn compose_and_spawn(s: &Shot, mode: &str) {
    if let Some((buf, w, h)) = compose(s) {
        if let Some(path) = output::save_temp_png(&buf, w, h) {
            if !crate::screenshot::spawn_self(&[mode, &path]) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Hand the composited capture to the OCR helper process (`--ocr <png>`), which reads
/// the text out of it, puts it on the clipboard, and shows the result window.
/// (Caller commits in-progress text first.)
pub(super) unsafe fn finish_ocr(s: &Shot) {
    if s.automation.is_some() {
        return;
    }
    compose_and_spawn(s, "--ocr");
}

/// Save the composited capture. With the "fixed save folder" option on, auto-saves a
/// timestamped PNG into the configured folder (Desktop by default) and returns true.
/// Otherwise prompts via a Save-As dialog and returns true iff the user picked a path
/// and it saved — false on cancel, so the caller can leave the overlay open. (Caller
/// commits in-progress text first.)
pub(super) unsafe fn finish_save(hwnd: HWND, s: &Shot) -> bool {
    if s.automation.is_some() {
        return false;
    }
    let Some((buf, w, h)) = compose(s) else {
        return false;
    };
    if sagethumbs2k_core::settings::screenshot_use_save_dir() {
        let dir = crate::screenshot::effective_save_dir();
        let ok = output::save_png_to_dir(std::path::Path::new(&dir), &buf, w, h);
        if !ok {
            // A `false` here is a DISK failure (full/unwritable/missing folder), NOT a cancel
            // (the Save-As path can't run in this branch). Tell the user — otherwise the caller
            // treats false as "keep editing" and the capture silently never lands.
            with_modal(hwnd, || {
                let m = wide(&crate::win::t("shot_save_failed").replace("{dir}", &dir));
                let cap = wide("SageThumbs 2K");
                MessageBoxW(
                    Some(hwnd),
                    PCWSTR(m.as_ptr()),
                    PCWSTR(cap.as_ptr()),
                    MB_OK | MB_ICONWARNING,
                );
            });
        }
        ok
    } else {
        let mut saved = false;
        // Drop the overlay's always-on-top so the picker isn't trapped behind the
        // fullscreen capture window (it pumps its own modal loop while shown).
        with_modal(hwnd, || {
            if let Some(path) = crate::win::pick_save_png(
                hwnd,
                &crate::screenshot::effective_save_dir(),
                &output::timestamped_name(),
            ) {
                saved = output::save_png_to_path(std::path::Path::new(&path), &buf, w, h);
                if !saved {
                    // A write failure here looks IDENTICAL to a user Cancel (both leave `saved`
                    // false) unless we say something — the fixed-folder branch above already
                    // warns on its own failure; this path had no equivalent.
                    let dir = std::path::Path::new(&path)
                        .parent()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let m = wide(&crate::win::t("shot_save_failed").replace("{dir}", &dir));
                    let cap = wide("SageThumbs 2K");
                    MessageBoxW(
                        Some(hwnd),
                        PCWSTR(m.as_ptr()),
                        PCWSTR(cap.as_ptr()),
                        MB_OK | MB_ICONWARNING,
                    );
                }
            }
        });
        saved
    }
}

/// Handle a toolbar button click. Returns true if it destroyed the window (the
/// caller must then stop touching `s`/`hwnd`).
pub(super) unsafe fn handle_button(hwnd: HWND, s: &mut Shot, btn: Button) -> bool {
    let blocked_status = match btn {
        Button::Copy => Some("blocked-copy"),
        Button::Ocr => Some("blocked-ocr"),
        Button::Save => Some("blocked-save"),
        Button::Upload => Some("blocked-upload"),
        _ => None,
    };
    if blocked_status.is_some_and(|status| block_automation_output(s, status)) {
        return false;
    }

    match btn {
        Button::Tool(Tool::Text) => {
            if s.tool == Tool::Text {
                // Already active → toggle the text settings flyout.
                s.text_flyout = !s.text_flyout;
                if !s.text_flyout {
                    s.font_dropdown = false;
                }
            } else {
                commit_text(s);
                s.tool = Tool::Text;
                s.selected = None;
                s.move_from = None;
                s.text_flyout = true; // open settings when the Text tool is picked
            }
            s.color_flyout = false;
            false
        }
        Button::Tool(t) => {
            commit_text(s);
            s.tool = t;
            s.selected = None;
            s.move_from = None;
            s.typing_drag = false;
            s.text_flyout = false;
            s.font_dropdown = false;
            s.color_flyout = false;
            false
        }
        Button::Color => {
            s.color_flyout = !s.color_flyout;
            s.text_flyout = false;
            s.font_dropdown = false;
            false
        }
        Button::Undo => {
            if let Some(sh) = s.shapes.pop() {
                s.redo.push(sh);
            }
            s.selected = None;
            s.move_from = None;
            false
        }
        Button::Redo => {
            if let Some(sh) = s.redo.pop() {
                s.shapes.push(sh);
            }
            s.selected = None;
            s.move_from = None;
            false
        }
        Button::Copy => {
            commit_text(s);
            if finish_copy(s) {
                let _ = DestroyWindow(hwnd);
                true
            } else {
                false // clipboard failure (toast already shown) -> keep editing, like Save-As cancel
            }
        }
        Button::Ocr => {
            commit_text(s);
            finish_ocr(s);
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Save => {
            commit_text(s);
            if finish_save(hwnd, s) {
                let _ = DestroyWindow(hwnd);
                true
            } else {
                false // Save-As cancelled → keep the overlay open for more edits
            }
        }
        Button::Upload => {
            commit_text(s);
            compose_and_spawn(s, "--upload");
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Close => {
            let _ = DestroyWindow(hwnd);
            true
        }
        Button::Sep => false, // not clickable (hit() skips separators)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A shape dragged (or moved) so its rect crosses the selection edge must be clamped
    /// INTO the selection before compose() draws it — the live preview only clips the paint,
    /// never the stored rect, so this is the only place the bound actually gets enforced.
    #[test]
    fn clamp_rect_keeps_shape_bounds_inside_the_selection() {
        let sel = RECT {
            left: 100,
            top: 100,
            right: 300,
            bottom: 300,
        };

        // Straddles every edge — must come back exactly at `sel`'s bounds.
        let straddling = RECT {
            left: 50,
            top: 50,
            right: 350,
            bottom: 350,
        };
        assert_eq!(clamp_rect(straddling, sel), sel);

        // Fully inside — must pass through unchanged.
        let inside = RECT {
            left: 150,
            top: 150,
            right: 200,
            bottom: 200,
        };
        assert_eq!(clamp_rect(inside, sel), inside);

        // Entirely outside — collapses to an empty/inverted rect (right < left), which the
        // region-effect draw functions already treat as "draw nothing" (e.g. draw_pixelate's
        // `w < 4 || h < 4` guard), not something clamp_rect itself needs to special-case.
        let outside = RECT {
            left: 400,
            top: 400,
            right: 450,
            bottom: 450,
        };
        let c = clamp_rect(outside, sel);
        assert!(c.right - c.left <= 0 || c.bottom - c.top <= 0);
    }
}
