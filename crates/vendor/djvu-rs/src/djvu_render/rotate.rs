//! Page rotation.

use super::*;

// ── Page rotation ───────────────────────────────────────────────────────────

/// Convert a rotation to a number of 90° CW steps (0..3).
pub(super) fn rotation_to_steps(r: crate::info::Rotation) -> u8 {
    use crate::info::Rotation;
    match r {
        Rotation::None => 0,
        Rotation::Cw90 => 1,
        Rotation::Rot180 => 2,
        Rotation::Ccw90 => 3,
    }
}

/// Convert a user rotation to a number of 90° CW steps (0..3).
pub(super) fn user_rotation_to_steps(r: UserRotation) -> u8 {
    match r {
        UserRotation::None => 0,
        UserRotation::Cw90 => 1,
        UserRotation::Rot180 => 2,
        UserRotation::Ccw90 => 3,
    }
}

/// Combine INFO chunk rotation with user rotation and return the combined
/// `info::Rotation` value.
pub(crate) fn combine_rotations(
    info: crate::info::Rotation,
    user: UserRotation,
) -> crate::info::Rotation {
    use crate::info::Rotation;
    let steps = (rotation_to_steps(info) + user_rotation_to_steps(user)) % 4;
    match steps {
        0 => Rotation::None,
        1 => Rotation::Cw90,
        2 => Rotation::Rot180,
        3 => Rotation::Ccw90,
        _ => unreachable!(),
    }
}

/// Apply page rotation to the rendered pixmap.
///
/// For 90°/270° rotations, width and height are swapped.
pub(super) fn rotate_pixmap(src: Pixmap, rotation: crate::info::Rotation) -> Pixmap {
    use crate::info::Rotation;
    match rotation {
        Rotation::None => src,
        Rotation::Cw90 => {
            let w = src.height;
            let h = src.width;
            let mut out = Pixmap::white(w, h);
            // #447: 32×32 tiled transpose. The naïve per-pixel write strides the
            // destination by `out.width*4` bytes (a cache miss per pixel); tiling
            // keeps both the source read and destination write within a few cache
            // lines per tile. 4-byte copy is exact: rendered source pixmaps carry
            // alpha=255 and `Pixmap::white` pre-fills alpha=255.
            const TILE: u32 = 32;
            let (sw, sh, ow) = (src.width as usize, src.height as usize, w as usize);
            let mut ty = 0;
            while ty < src.height {
                let mut tx = 0;
                while tx < src.width {
                    let y_end = (ty + TILE).min(src.height);
                    let x_end = (tx + TILE).min(src.width);
                    for y in ty..y_end {
                        let src_row = y as usize * sw * 4;
                        let dst_col = sh - 1 - y as usize;
                        for x in tx..x_end {
                            let si = src_row + x as usize * 4;
                            let di = (x as usize * ow + dst_col) * 4;
                            out.data[di..di + 4].copy_from_slice(&src.data[si..si + 4]);
                        }
                    }
                    tx += TILE;
                }
                ty += TILE;
            }
            out
        }
        Rotation::Rot180 => {
            let mut out = Pixmap::white(src.width, src.height);
            for y in 0..src.height {
                for x in 0..src.width {
                    let (r, g, b) = src.get_rgb(x, y);
                    out.set_rgb(src.width - 1 - x, src.height - 1 - y, r, g, b);
                }
            }
            out
        }
        Rotation::Ccw90 => {
            let w = src.height;
            let h = src.width;
            let mut out = Pixmap::white(w, h);
            // #447: 32×32 tiled transpose (see Cw90).
            const TILE: u32 = 32;
            let (sw, ow) = (src.width as usize, w as usize);
            let mut ty = 0;
            while ty < src.height {
                let mut tx = 0;
                while tx < src.width {
                    let y_end = (ty + TILE).min(src.height);
                    let x_end = (tx + TILE).min(src.width);
                    for y in ty..y_end {
                        let src_row = y as usize * sw * 4;
                        for x in tx..x_end {
                            let si = src_row + x as usize * 4;
                            let di = ((sw - 1 - x as usize) * ow + y as usize) * 4;
                            out.data[di..di + 4].copy_from_slice(&src.data[si..si + 4]);
                        }
                    }
                    tx += TILE;
                }
                ty += TILE;
            }
            out
        }
    }
}
