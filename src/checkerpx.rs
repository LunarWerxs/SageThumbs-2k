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
    let px = (w as usize) * (h as usize);
    if w == 0 || h == 0 || rgba.len() < px * 4 {
        return;
    }
    // A fully opaque thumbnail (the overwhelming majority) would come out bit-identical, so
    // the scan is strictly cheaper than the blend it avoids.
    if rgba[3..px * 4].iter().step_by(4).all(|&a| a == 255) {
        return;
    }

    let cell = cell_for(w.min(h));
    for y in 0..h {
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
        assert!(px.iter().skip(3).step_by(4).all(|&a| a == 255), "opaque out");
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
        for p in px.chunks_exact_mut(4) {
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
}
