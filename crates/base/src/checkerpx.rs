//! Composite a transparency checkerboard UNDER an RGBA8 thumbnail, in place.
//!
//! The classic right-click preview tile and the Quick preview window already draw a
//! checkerboard behind see-through images, but they draw it with GDI onto a device context
//! they own. An Explorer thumbnail has no device context: we hand the shell a bitmap and the
//! shell composites it over whatever the folder view happens to be. So the checkerboard for
//! that surface has to be burned into the pixels, which is what this does.
//!
//! Off by default (`ThumbChecker`). Correct alpha *is* the better default — Explorer shows
//! the folder background through a transparent PNG, which is what the shell is designed to
//! do — but people coming from the original SageThumbs expect the checkerboard, and without
//! it a mostly-transparent logo on a matching background genuinely does disappear.
//!
//! The result is fully opaque by construction: every pixel ends up on top of a solid
//! checker cell, so the thumbnail no longer carries transparency once this has run.

/// The two greys of the checkerboard. Deliberately light and low-contrast — this sits behind
/// the user's picture, and a loud backdrop would fight it. Matches the shades the preview
/// surfaces use closely enough that the same file looks the same in both places.
const LIGHT: (u8, u8, u8) = (255, 255, 255);
const DARK: (u8, u8, u8) = (204, 204, 204);

/// Checker cell size as a fraction of the short edge, then clamped. A fixed pixel size looks
/// coarse on a 96 px tile and invisible on a 1024 px one; scaling keeps the pattern reading
/// as "transparent" at every size Explorer asks for.
fn cell_for(edge: u32) -> u32 {
    (edge / 16).clamp(4, 32)
}

/// Composite `rgba` (straight, non-premultiplied RGBA8, `w * h * 4` bytes) over a
/// checkerboard. No-ops on a buffer too small for the claimed size, and skips the work
/// entirely when the image has no transparent pixel to reveal.
pub fn compose_under(rgba: &mut [u8], w: u32, h: u32) {
    // Release builds run with overflow-checks off (see Cargo.toml), so a plain `w * h * 4`
    // could wrap and silently defeat this truncated-buffer guard, leading to out-of-bounds
    // indexing in the pixel loop below — matching the `checked_mul` `dib.rs` and
    // `safety::composite_rgba_over_bg` already use for the same shape of buffer check.
    let Some(px) = (w as usize).checked_mul(h as usize) else {
        return;
    };
    let Some(need) = px.checked_mul(4) else {
        return;
    };
    if w == 0 || h == 0 || rgba.len() < need {
        return;
    }
    // A fully opaque thumbnail (the overwhelming majority) would come out bit-identical, so
    // the scan is strictly cheaper than the blend it avoids.
    if rgba[3..need].iter().step_by(4).all(|&a| a == 255) {
        return;
    }

    let cell = cell_for(w.min(h));
    for y in 0..h {
        blend_row(rgba, w, y, cell);
    }
}

/// Blend one image row (`y`) of `rgba` over its checkerboard cells, leaving opaque pixels
/// untouched. `cell` is the checker cell size in pixels.
fn blend_row(rgba: &mut [u8], w: u32, y: u32, cell: u32) {
    let row_dark = (y / cell) % 2 == 1;
    for x in 0..w {
        let i = ((y * w + x) * 4) as usize;
        let a = rgba[i + 3] as u32;
        if a == 255 {
            continue;
        }
        let bg = if ((x / cell) % 2 == 1) != row_dark {
            DARK
        } else {
            LIGHT
        };
        let over = |src: u8, dst: u8| ((src as u32 * a + dst as u32 * (255 - a)) / 255) as u8;
        rgba[i] = over(rgba[i], bg.0);
        rgba[i + 1] = over(rgba[i + 1], bg.1);
        rgba[i + 2] = over(rgba[i + 2], bg.2);
        rgba[i + 3] = 255;
    }
}

/// Two subtle checkerboard shades from a base menu colour: the base nudged a few
/// levels darker and a few lighter. Their average stays ≈ `bg` (so the menu tone
/// doesn't shift) and they sit only ~16 levels apart — enough to read as
/// "transparency here" without competing with the menu. Follows light/dark/accent
/// automatically since it's derived from whatever `bg` is passed.
pub fn checker_shades(bg: u32) -> (u32, u32) {
    let ch = |shift: u32| (bg >> shift) & 0xFF; // COLORREF is 0x00BBGGRR
    let (r, g, b) = (ch(0), ch(8), ch(16));
    let darker = |c: u32| c.saturating_sub(8);
    let lighter = |c: u32| (c + 8).min(255);
    let pack = |r: u32, g: u32, b: u32| r | (g << 8) | (b << 16);
    (
        pack(darker(r), darker(g), darker(b)),
        pack(lighter(r), lighter(g), lighter(b)),
    )
}

/// Fill `rc` with a two-tone checkerboard of `cell`-px squares — the backdrop a thumbnail is
/// alpha-blended onto, so transparent pixels reveal the pattern instead of disappearing into the
/// flat background colour.
///
/// `cell` is a caller choice because the two surfaces that use this are different sizes: the menu
/// tile is a ~72px thumbnail where 8px reads as texture, while the Quick preview window is
/// full-size and wants a DPI-scaled, visibly larger square.
///
/// # Safety
///
/// `hdc` must be a valid device context that stays alive for the call. Nothing is retained.
pub unsafe fn fill_checker(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    rc: &windows::Win32::Foundation::RECT,
    c0: u32,
    c1: u32,
    cell: i32,
) {
    let (left, top) = (rc.left, rc.top);
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let cell = cell.max(2);
    let b0 =
        windows::Win32::Graphics::Gdi::CreateSolidBrush(windows::Win32::Foundation::COLORREF(c0));
    let b1 =
        windows::Win32::Graphics::Gdi::CreateSolidBrush(windows::Win32::Foundation::COLORREF(c1));
    windows::Win32::Graphics::Gdi::FillRect(
        hdc,
        &windows::Win32::Foundation::RECT {
            left,
            top,
            right: left + w,
            bottom: top + h,
        },
        b0,
    );
    let mut y = 0;
    while y < h {
        let mut x = 0;
        while x < w {
            if ((x / cell) + (y / cell)) & 1 == 1 {
                let r = windows::Win32::Foundation::RECT {
                    left: left + x,
                    top: top + y,
                    right: left + (x + cell).min(w),
                    bottom: top + (y + cell).min(h),
                };
                windows::Win32::Graphics::Gdi::FillRect(hdc, &r, b1);
            }
            x += cell;
        }
        y += cell;
    }
    let _ = windows::Win32::Graphics::Gdi::DeleteObject(b0.into());
    let _ = windows::Win32::Graphics::Gdi::DeleteObject(b1.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fully_transparent_image_becomes_the_bare_checkerboard() {
        let (w, h) = (64u32, 64u32);
        let mut px = vec![0u8; (w * h * 4) as usize];
        compose_under(&mut px, w, h);
        let cell = cell_for(64);
        let at = |x: u32, y: u32| {
            let i = ((y * w + x) * 4) as usize;
            (px[i], px[i + 1], px[i + 2], px[i + 3])
        };
        assert_eq!(at(0, 0), (LIGHT.0, LIGHT.1, LIGHT.2, 255));
        assert_eq!(at(cell, 0), (DARK.0, DARK.1, DARK.2, 255));
        assert_eq!(at(cell, cell), (LIGHT.0, LIGHT.1, LIGHT.2, 255));
        assert!(
            px.iter().skip(3).step_by(4).all(|&a| a == 255),
            "opaque out"
        );
    }

    /// The early-out is a promise, not just a speed-up: an opaque thumbnail must come back
    /// byte-for-byte identical, or turning the option on would silently alter every picture.
    #[test]
    fn an_opaque_image_is_untouched() {
        let (w, h) = (64u32, 64u32);
        let mut px: Vec<u8> = (0..w * h)
            .flat_map(|i| [(i % 251) as u8, 9, 9, 255])
            .collect();
        let before = px.clone();
        compose_under(&mut px, w, h);
        assert_eq!(px, before);
    }

    #[test]
    fn semi_transparent_pixels_blend_toward_the_cell_behind_them() {
        let (w, h) = (64u32, 64u32);
        // Half-alpha black over the top-left (LIGHT) cell -> mid grey, and opaque.
        let mut px = vec![0u8; (w * h * 4) as usize];
        let (chunks, _) = px.as_chunks_mut::<4>();
        for p in chunks {
            p[3] = 128;
        }
        compose_under(&mut px, w, h);
        assert_eq!(px[0], ((255 * (255 - 128)) / 255) as u8);
        assert_eq!(px[3], 255);
    }

    #[test]
    fn does_not_panic_on_a_truncated_buffer() {
        let mut px = vec![0u8; 10];
        compose_under(&mut px, 256, 256);
    }

    /// `w * h * 4` must not wrap. `w == h == u32::MAX` fits in a 64-bit `usize`
    /// product on its own but overflows once multiplied by 4 — the exact case a plain
    /// (unchecked, release-build) multiply would silently wrap on, defeating the
    /// length check below it and reading out of bounds.
    #[test]
    fn does_not_panic_on_dimensions_whose_byte_count_overflows_usize() {
        let mut px = vec![0u8; 16];
        compose_under(&mut px, u32::MAX, u32::MAX);
    }
}
