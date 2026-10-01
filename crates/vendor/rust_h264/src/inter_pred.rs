//! H.264 inter prediction / motion compensation (spec 8.4.2).
//!
//! Generates predicted blocks for P/B slices by interpolating pixels from
//! reference frames at quarter-pel (luma) or eighth-pel (chroma) precision.

use crate::dpb::DecodedPicture;

/// Fetch a luma sample from the reference picture with boundary clipping.
/// Out-of-bounds coordinates are clamped to the picture edge (spec 8.4.2.2.1).
#[inline]
fn ref_luma(pic: &DecodedPicture, x: i32, y: i32) -> i32 {
    let cx = x.clamp(0, pic.width as i32 - 1) as usize;
    let cy = y.clamp(0, pic.height as i32 - 1) as usize;
    pic.y[cy * pic.width as usize + cx] as i32
}

/// Fetch a chroma sample with boundary clipping.
#[inline]
fn ref_chroma(plane: &[u16], width: usize, height: usize, x: i32, y: i32) -> i32 {
    if width == 0 || height == 0 {
        return 0;
    }
    let cx = x.clamp(0, width as i32 - 1) as usize;
    let cy = y.clamp(0, height as i32 - 1) as usize;
    let idx = cy * width + cx;
    if idx >= plane.len() {
        return 0;
    }
    plane[idx] as i32
}

/// Clip1 (spec 5.7): clamp to `0..=max` where `max = (1 << BitDepth) - 1`.
#[inline]
fn clip_pel(v: i32, max: i32) -> u16 {
    v.clamp(0, max) as u16
}

/// Clip1Y maximum for a reference picture's luma bit depth.
#[inline]
fn luma_max(pic: &DecodedPicture) -> i32 {
    (1i32 << pic.bit_depth_luma.clamp(8, 14)) - 1
}

#[inline]
fn avg(a: u16, b: u16) -> u16 {
    ((a as u32 + b as u32 + 1) >> 1) as u16
}

/// 6-tap FIR filter coefficient application on 6 consecutive samples.
#[inline(always)]
fn fir6(s: &[u16], i: usize) -> i32 {
    s[i] as i32 - 5 * s[i + 1] as i32 + 20 * s[i + 2] as i32 + 20 * s[i + 3] as i32
        - 5 * s[i + 4] as i32
        + s[i + 5] as i32
}

/// 6-tap FIR on i32 intermediates (for hv second pass).
#[inline(always)]
fn fir6_i32(s0: i32, s1: i32, s2: i32, s3: i32, s4: i32, s5: i32) -> i32 {
    s0 - 5 * s1 + 20 * s2 + 20 * s3 - 5 * s4 + s5
}

/// 6-tap horizontal half-pel filter at integer position (x, y).
/// Returns the Clip1Y result. Used only for boundary blocks.
fn half_pel_h(pic: &DecodedPicture, x: i32, y: i32, max: i32) -> u16 {
    let s = |dx: i32| ref_luma(pic, x + dx, y);
    clip_pel(
        (s(-2) - 5 * s(-1) + 20 * s(0) + 20 * s(1) - 5 * s(2) + s(3) + 16) >> 5,
        max,
    )
}

/// 6-tap vertical half-pel filter at integer position (x, y).
/// Used only for boundary blocks.
fn half_pel_v(pic: &DecodedPicture, x: i32, y: i32, max: i32) -> u16 {
    let s = |dy: i32| ref_luma(pic, x, y + dy);
    clip_pel(
        (s(-2) - 5 * s(-1) + 20 * s(0) + 20 * s(1) - 5 * s(2) + s(3) + 16) >> 5,
        max,
    )
}

/// Diagonal half-pel. Used only for boundary blocks.
fn half_pel_hv(pic: &DecodedPicture, x: i32, y: i32, max: i32) -> u16 {
    let mut h = [0i32; 6];
    for (i, dy) in (-2..=3).enumerate() {
        let s = |dx: i32| ref_luma(pic, x + dx, y + dy);
        h[i] = s(-2) - 5 * s(-1) + 20 * s(0) + 20 * s(1) - 5 * s(2) + s(3);
    }
    let val = h[0] - 5 * h[1] + 20 * h[2] + 20 * h[3] - 5 * h[4] + h[5];
    clip_pel((val + 512) >> 10, max)
}

/// Per-pixel interpolation fallback for boundary blocks.
fn luma_interp(pic: &DecodedPicture, x: i32, y: i32, frac_x: i32, frac_y: i32, max: i32) -> u16 {
    match (frac_x, frac_y) {
        (0, 0) => ref_luma(pic, x, y) as u16,
        (2, 0) => half_pel_h(pic, x, y, max),
        (0, 2) => half_pel_v(pic, x, y, max),
        (2, 2) => half_pel_hv(pic, x, y, max),
        (1, 0) => avg(ref_luma(pic, x, y) as u16, half_pel_h(pic, x, y, max)),
        (3, 0) => avg(half_pel_h(pic, x, y, max), ref_luma(pic, x + 1, y) as u16),
        (0, 1) => avg(ref_luma(pic, x, y) as u16, half_pel_v(pic, x, y, max)),
        (0, 3) => avg(half_pel_v(pic, x, y, max), ref_luma(pic, x, y + 1) as u16),
        (2, 1) => avg(half_pel_h(pic, x, y, max), half_pel_hv(pic, x, y, max)),
        (2, 3) => avg(half_pel_hv(pic, x, y, max), half_pel_h(pic, x, y + 1, max)),
        (1, 2) => avg(half_pel_v(pic, x, y, max), half_pel_hv(pic, x, y, max)),
        (3, 2) => avg(half_pel_hv(pic, x, y, max), half_pel_v(pic, x + 1, y, max)),
        (1, 1) => avg(half_pel_h(pic, x, y, max), half_pel_v(pic, x, y, max)),
        (3, 1) => avg(half_pel_h(pic, x, y, max), half_pel_v(pic, x + 1, y, max)),
        (1, 3) => avg(half_pel_v(pic, x, y, max), half_pel_h(pic, x, y + 1, max)),
        (3, 3) => avg(
            half_pel_v(pic, x + 1, y, max),
            half_pel_h(pic, x, y + 1, max),
        ),
        _ => unreachable!(),
    }
}

/// Row-based horizontal half-pel filter for in-bounds blocks.
/// Reads `w` output pixels from row at `src` (which must have `w + 5` accessible bytes).
#[inline(always)]
fn row_half_pel_h(src: &[u16], out: &mut [u16], w: usize, max: i32) {
    for i in 0..w {
        out[i] = clip_pel((fir6(src, i) + 16) >> 5, max);
    }
}

/// Row-based vertical half-pel filter for in-bounds blocks.
/// `rows` contains 6 row slices (y-2..y+3), each at least `w` bytes.
#[inline(always)]
fn row_half_pel_v(rows: [&[u16]; 6], out: &mut [u16], w: usize, max: i32) {
    for i in 0..w {
        let val = rows[0][i] as i32 - 5 * rows[1][i] as i32
            + 20 * rows[2][i] as i32
            + 20 * rows[3][i] as i32
            - 5 * rows[4][i] as i32
            + rows[5][i] as i32;
        out[i] = clip_pel((val + 16) >> 5, max);
    }
}

/// Row-based diagonal half-pel (hv) for in-bounds blocks.
/// `rows` contains 6 row slices (y-2..y+3), each with `w + 5` accessible bytes.
#[allow(clippy::needless_range_loop)]
#[inline(always)]
fn row_half_pel_hv(rows: [&[u16]; 6], out: &mut [u16], w: usize, max: i32) {
    // First pass: horizontal filter on each of 6 rows → i32 intermediates
    // We need w intermediate values per row
    for i in 0..w {
        let h0 = fir6(rows[0], i);
        let h1 = fir6(rows[1], i);
        let h2 = fir6(rows[2], i);
        let h3 = fir6(rows[3], i);
        let h4 = fir6(rows[4], i);
        let h5 = fir6(rows[5], i);
        let val = fir6_i32(h0, h1, h2, h3, h4, h5);
        out[i] = clip_pel((val + 512) >> 10, max);
    }
}

/// Check if a block with the given filter margins is fully within bounds.
/// For half-pel filters, margin is 3 (needs x-2..x+w+2, y-2..y+h+2).
/// For full-pel, margin_left/top=0, margin_right/bottom=0.
#[allow(clippy::too_many_arguments)]
#[inline(always)]
fn block_in_bounds(
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    pic_w: i32,
    pic_h: i32,
    margin_left: i32,
    margin_top: i32,
    margin_right: i32,
    margin_bottom: i32,
) -> bool {
    x - margin_left >= 0
        && y - margin_top >= 0
        && x + w + margin_right <= pic_w
        && y + h + margin_bottom <= pic_h
}

/// Luma motion compensation with explicit reference stride and buffer offset.
/// For field-coded MBAFF MBs, pass `ref_stride = ref_pic.width * 2`,
/// `ref_y_offset = ref_pic.width` (for bottom field) or `0` (for top field),
/// and `y` in field-line units.
#[allow(clippy::too_many_arguments)]
pub fn luma_mc_stride(
    ref_pic: &DecodedPicture,
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    block_w: usize,
    block_h: usize,
    output: &mut [u16],
    ref_stride: usize,
    ref_y_offset: usize,
) {
    let max = luma_max(ref_pic);
    // Guard against malformed block sizes that would overrun the output buffer
    if block_w == 0 || block_h == 0 || block_w * block_h > output.len() {
        return;
    }
    let frac_x = dx.rem_euclid(4);
    let frac_y = dy.rem_euclid(4);
    // Integer part: arithmetic right shift gives floor division for negative values
    let x_int = x + (dx >> 2);
    let y_int = y + (dy >> 2);

    let pic_w = ref_pic.width as i32;
    let pic_h = if ref_stride == ref_pic.width as usize {
        ref_pic.height as i32
    } else {
        // Field-coded: effective height is half the frame
        (ref_pic.height / 2) as i32
    };
    let stride = ref_stride;
    let bw = block_w as i32;
    let bh = block_h as i32;
    let ref_y = &ref_pic.y[ref_y_offset..];

    // Determine margins needed for the filter type
    // Half-pel filters need 2 pixels before and 3 after the block
    let needs_h = frac_x != 0; // horizontal filter needed
    let needs_v = frac_y != 0; // vertical filter needed
    let margin_l = if needs_h { 2 } else { 0 };
    let margin_r = if needs_h { 3 } else { 0 };
    let margin_t = if needs_v { 2 } else { 0 };
    let margin_b = if needs_v { 3 } else { 0 };

    // Quarter-pel positions that average with an offset integer/half-pel sample
    // may need +1 in a direction
    let extra_r: i32 = match frac_x {
        3 => 1,
        _ => 0,
    };
    let extra_b: i32 = match frac_y {
        3 => 1,
        _ => 0,
    };

    if block_in_bounds(
        x_int,
        y_int,
        bw + extra_r,
        bh + extra_b,
        pic_w,
        pic_h,
        margin_l,
        margin_t,
        margin_r,
        margin_b,
    ) {
        // Fast path: entire block + filter margins are in bounds
        // Access reference buffer directly without per-pixel clamping
        luma_mc_inner(
            ref_y,
            stride,
            x_int as usize,
            y_int as usize,
            block_w,
            block_h,
            frac_x,
            frac_y,
            output,
            max,
        );
    } else if ref_y_offset > 0 || stride != ref_pic.width as usize {
        // Field-coded boundary fallback: extract field lines into a temporary
        // DecodedPicture and use the standard interpolation on it.
        let field_h = pic_h as usize;
        let field_w = pic_w as usize;
        let mut field_buf = vec![0u16; field_w * field_h];
        for r in 0..field_h {
            let src_off = ref_y_offset + r * stride;
            let dst_off = r * field_w;
            for c in 0..field_w {
                if src_off + c < ref_pic.y.len() {
                    field_buf[dst_off + c] = ref_pic.y[src_off + c];
                }
            }
        }
        let field_pic = DecodedPicture {
            y: field_buf,
            u: vec![],
            v: vec![],
            width: field_w as u32,
            height: field_h as u32,
            pic_order_cnt: 0,
            frame_num: 0,
            mv_l0: vec![],
            ref_idx_l0: vec![],
            ref_poc_l0: vec![],
            mv_l1: vec![],
            ref_idx_l1: vec![],
            mb_width: 0,
            is_intra: false,
            structure: crate::dpb::PictureStructure::Frame,
            bit_depth_luma: ref_pic.bit_depth_luma,
            bit_depth_chroma: ref_pic.bit_depth_chroma,
        };
        for row in 0..block_h {
            for col in 0..block_w {
                output[row * block_w + col] = luma_interp(
                    &field_pic,
                    x_int + col as i32,
                    y_int + row as i32,
                    frac_x,
                    frac_y,
                    max,
                );
            }
        }
    } else {
        // Boundary fallback: per-pixel with clamping
        for row in 0..block_h {
            for col in 0..block_w {
                output[row * block_w + col] = luma_interp(
                    ref_pic,
                    x_int + col as i32,
                    y_int + row as i32,
                    frac_x,
                    frac_y,
                    max,
                );
            }
        }
    }
}

/// Inner loop for in-bounds luma MC. All reference accesses are unchecked
/// (bounds already verified by caller). Dispatches on fractional position
/// once, then processes all rows with direct buffer access.
#[allow(clippy::too_many_arguments)]
fn luma_mc_inner(
    ref_y: &[u16],
    stride: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    frac_x: i32,
    frac_y: i32,
    output: &mut [u16],
    max: i32,
) {
    // Helper: get a row slice starting at (x + dx, y + dy) with length len
    let row = |dy: isize, dx: isize, len: usize| -> &[u16] {
        let off = (y as isize + dy) as usize * stride + (x as isize + dx) as usize;
        &ref_y[off..off + len]
    };

    // Helper: get 6 vertically adjacent rows for vertical/diagonal filters
    let vrows = |dx: isize, dy_base: isize, len: usize| -> [&[u16]; 6] {
        [
            row(dy_base - 2, dx, len),
            row(dy_base - 1, dx, len),
            row(dy_base, dx, len),
            row(dy_base + 1, dx, len),
            row(dy_base + 2, dx, len),
            row(dy_base + 3, dx, len),
        ]
    };

    match (frac_x, frac_y) {
        (0, 0) => {
            // Full-pel copy
            for r in 0..h {
                let src = row(r as isize, 0, w);
                output[r * w..(r + 1) * w].copy_from_slice(src);
            }
        }
        (2, 0) => {
            // Half-pel horizontal
            for r in 0..h {
                let src = row(r as isize, -2, w + 5);
                row_half_pel_h(src, &mut output[r * w..], w, max);
            }
        }
        (0, 2) => {
            // Half-pel vertical
            for r in 0..h {
                let rows = vrows(0, r as isize, w);
                row_half_pel_v(rows, &mut output[r * w..], w, max);
            }
        }
        (2, 2) => {
            // Half-pel diagonal
            for r in 0..h {
                let rows = vrows(-2, r as isize, w + 5);
                row_half_pel_hv(rows, &mut output[r * w..], w, max);
            }
        }
        (1, 0) => {
            // Quarter-pel: avg(integer, half_h)
            for r in 0..h {
                let int_row = row(r as isize, 0, w);
                let src_h = row(r as isize, -2, w + 5);
                for i in 0..w {
                    let hp = clip_pel((fir6(src_h, i) + 16) >> 5, max);
                    output[r * w + i] = avg(int_row[i], hp);
                }
            }
        }
        (3, 0) => {
            // Quarter-pel: avg(half_h, integer+1)
            for r in 0..h {
                let int_row = row(r as isize, 1, w);
                let src_h = row(r as isize, -2, w + 5);
                for i in 0..w {
                    let hp = clip_pel((fir6(src_h, i) + 16) >> 5, max);
                    output[r * w + i] = avg(hp, int_row[i]);
                }
            }
        }
        (0, 1) => {
            // Quarter-pel: avg(integer, half_v)
            for r in 0..h {
                let int_row = row(r as isize, 0, w);
                let rows = vrows(0, r as isize, w);
                for i in 0..w {
                    let val = rows[0][i] as i32 - 5 * rows[1][i] as i32
                        + 20 * rows[2][i] as i32
                        + 20 * rows[3][i] as i32
                        - 5 * rows[4][i] as i32
                        + rows[5][i] as i32;
                    let hp = clip_pel((val + 16) >> 5, max);
                    output[r * w + i] = avg(int_row[i], hp);
                }
            }
        }
        (0, 3) => {
            // Quarter-pel: avg(half_v, integer_below)
            for r in 0..h {
                let int_row = row(r as isize + 1, 0, w);
                let rows = vrows(0, r as isize, w);
                for i in 0..w {
                    let val = rows[0][i] as i32 - 5 * rows[1][i] as i32
                        + 20 * rows[2][i] as i32
                        + 20 * rows[3][i] as i32
                        - 5 * rows[4][i] as i32
                        + rows[5][i] as i32;
                    let hp = clip_pel((val + 16) >> 5, max);
                    output[r * w + i] = avg(hp, int_row[i]);
                }
            }
        }
        (2, 1) => {
            // avg(half_h, half_hv)
            for r in 0..h {
                let src_h = row(r as isize, -2, w + 5);
                let rows_hv = vrows(-2, r as isize, w + 5);
                for i in 0..w {
                    let h_val = clip_pel((fir6(src_h, i) + 16) >> 5, max);
                    let h0 = fir6(rows_hv[0], i);
                    let h1 = fir6(rows_hv[1], i);
                    let h2 = fir6(rows_hv[2], i);
                    let h3 = fir6(rows_hv[3], i);
                    let h4 = fir6(rows_hv[4], i);
                    let h5 = fir6(rows_hv[5], i);
                    let hv_val = clip_pel((fir6_i32(h0, h1, h2, h3, h4, h5) + 512) >> 10, max);
                    output[r * w + i] = avg(h_val, hv_val);
                }
            }
        }
        (2, 3) => {
            // avg(half_hv, half_h_below)
            for r in 0..h {
                let src_h = row(r as isize + 1, -2, w + 5);
                let rows_hv = vrows(-2, r as isize, w + 5);
                for i in 0..w {
                    let hv_val = clip_pel(
                        (fir6_i32(
                            fir6(rows_hv[0], i),
                            fir6(rows_hv[1], i),
                            fir6(rows_hv[2], i),
                            fir6(rows_hv[3], i),
                            fir6(rows_hv[4], i),
                            fir6(rows_hv[5], i),
                        ) + 512)
                            >> 10,
                        max,
                    );
                    let h_val = clip_pel((fir6(src_h, i) + 16) >> 5, max);
                    output[r * w + i] = avg(hv_val, h_val);
                }
            }
        }
        (1, 2) => {
            // avg(half_v, half_hv)
            for r in 0..h {
                let rows_v = vrows(0, r as isize, w);
                let rows_hv = vrows(-2, r as isize, w + 5);
                for i in 0..w {
                    let v_val = clip_pel(
                        (rows_v[0][i] as i32 - 5 * rows_v[1][i] as i32
                            + 20 * rows_v[2][i] as i32
                            + 20 * rows_v[3][i] as i32
                            - 5 * rows_v[4][i] as i32
                            + rows_v[5][i] as i32
                            + 16)
                            >> 5,
                        max,
                    );
                    let hv_val = clip_pel(
                        (fir6_i32(
                            fir6(rows_hv[0], i),
                            fir6(rows_hv[1], i),
                            fir6(rows_hv[2], i),
                            fir6(rows_hv[3], i),
                            fir6(rows_hv[4], i),
                            fir6(rows_hv[5], i),
                        ) + 512)
                            >> 10,
                        max,
                    );
                    output[r * w + i] = avg(v_val, hv_val);
                }
            }
        }
        (3, 2) => {
            // avg(half_hv, half_v_right)
            for r in 0..h {
                let rows_v = vrows(1, r as isize, w);
                let rows_hv = vrows(-2, r as isize, w + 5);
                for i in 0..w {
                    let hv_val = clip_pel(
                        (fir6_i32(
                            fir6(rows_hv[0], i),
                            fir6(rows_hv[1], i),
                            fir6(rows_hv[2], i),
                            fir6(rows_hv[3], i),
                            fir6(rows_hv[4], i),
                            fir6(rows_hv[5], i),
                        ) + 512)
                            >> 10,
                        max,
                    );
                    let v_val = clip_pel(
                        (rows_v[0][i] as i32 - 5 * rows_v[1][i] as i32
                            + 20 * rows_v[2][i] as i32
                            + 20 * rows_v[3][i] as i32
                            - 5 * rows_v[4][i] as i32
                            + rows_v[5][i] as i32
                            + 16)
                            >> 5,
                        max,
                    );
                    output[r * w + i] = avg(hv_val, v_val);
                }
            }
        }
        (1, 1) => {
            // avg(half_h, half_v)
            for r in 0..h {
                let src_h = row(r as isize, -2, w + 5);
                let rows_v = vrows(0, r as isize, w);
                for i in 0..w {
                    let h_val = clip_pel((fir6(src_h, i) + 16) >> 5, max);
                    let v_val = clip_pel(
                        (rows_v[0][i] as i32 - 5 * rows_v[1][i] as i32
                            + 20 * rows_v[2][i] as i32
                            + 20 * rows_v[3][i] as i32
                            - 5 * rows_v[4][i] as i32
                            + rows_v[5][i] as i32
                            + 16)
                            >> 5,
                        max,
                    );
                    output[r * w + i] = avg(h_val, v_val);
                }
            }
        }
        (3, 1) => {
            // avg(half_h, half_v_right)
            for r in 0..h {
                let src_h = row(r as isize, -2, w + 5);
                let rows_v = vrows(1, r as isize, w);
                for i in 0..w {
                    let h_val = clip_pel((fir6(src_h, i) + 16) >> 5, max);
                    let v_val = clip_pel(
                        (rows_v[0][i] as i32 - 5 * rows_v[1][i] as i32
                            + 20 * rows_v[2][i] as i32
                            + 20 * rows_v[3][i] as i32
                            - 5 * rows_v[4][i] as i32
                            + rows_v[5][i] as i32
                            + 16)
                            >> 5,
                        max,
                    );
                    output[r * w + i] = avg(h_val, v_val);
                }
            }
        }
        (1, 3) => {
            // avg(half_v, half_h_below)
            for r in 0..h {
                let src_h = row(r as isize + 1, -2, w + 5);
                let rows_v = vrows(0, r as isize, w);
                for i in 0..w {
                    let v_val = clip_pel(
                        (rows_v[0][i] as i32 - 5 * rows_v[1][i] as i32
                            + 20 * rows_v[2][i] as i32
                            + 20 * rows_v[3][i] as i32
                            - 5 * rows_v[4][i] as i32
                            + rows_v[5][i] as i32
                            + 16)
                            >> 5,
                        max,
                    );
                    let h_val = clip_pel((fir6(src_h, i) + 16) >> 5, max);
                    output[r * w + i] = avg(v_val, h_val);
                }
            }
        }
        (3, 3) => {
            // avg(half_v_right, half_h_below)
            for r in 0..h {
                let src_h = row(r as isize + 1, -2, w + 5);
                let rows_v = vrows(1, r as isize, w);
                for i in 0..w {
                    let v_val = clip_pel(
                        (rows_v[0][i] as i32 - 5 * rows_v[1][i] as i32
                            + 20 * rows_v[2][i] as i32
                            + 20 * rows_v[3][i] as i32
                            - 5 * rows_v[4][i] as i32
                            + rows_v[5][i] as i32
                            + 16)
                            >> 5,
                        max,
                    );
                    let h_val = clip_pel((fir6(src_h, i) + 16) >> 5, max);
                    output[r * w + i] = avg(v_val, h_val);
                }
            }
        }
        _ => unreachable!(),
    }
}

/// Perform chroma motion compensation for one plane (U or V).
///
/// Chroma MVs use the same quarter-pel values as luma, but since chroma is
/// half spatial resolution (4:2:0), these become eighth-pel for chroma.
/// Uses bilinear interpolation (spec 8.4.2.2.2).
///
/// `x`, `y`: chroma block top-left in full chroma-pel coordinates.
/// `dx`, `dy`: motion vector in eighth-pel units (= luma quarter-pel MV).
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc(
    ref_plane: &[u16],
    ref_width: usize,
    ref_height: usize,
    x: i32,
    y: i32,
    dx: i32,
    dy: i32,
    block_w: usize,
    block_h: usize,
    output: &mut [u16],
) {
    // Guard against malformed block sizes that would overrun the output buffer
    if block_w == 0 || block_h == 0 || block_w * block_h > output.len() {
        return;
    }
    // For field-coded MBAFF, the effective height is derived from the available
    // plane data, which may be smaller than the frame chroma height.
    let ref_height = if ref_width > 0 {
        ref_height.min(ref_plane.len() / ref_width)
    } else {
        ref_height
    };
    let frac_x = dx.rem_euclid(8);
    let frac_y = dy.rem_euclid(8);
    let x_int = x + (dx >> 3);
    let y_int = y + (dy >> 3);

    // Full-pel fast path: direct copy when no interpolation needed
    if frac_x == 0 && frac_y == 0 {
        let w = ref_width as i32;
        let h = ref_height as i32;
        if x_int >= 0
            && y_int >= 0
            && x_int + block_w as i32 <= w
            && y_int + block_h as i32 <= h
            && (y_int as usize + block_h) * ref_width <= ref_plane.len()
        {
            let mut src_off = y_int as usize * ref_width + x_int as usize;
            for row in 0..block_h {
                if src_off + block_w > ref_plane.len() {
                    return;
                }
                output[row * block_w..(row + 1) * block_w]
                    .copy_from_slice(&ref_plane[src_off..src_off + block_w]);
                src_off += ref_width;
            }
        } else {
            for row in 0..block_h {
                for col in 0..block_w {
                    output[row * block_w + col] = ref_chroma(
                        ref_plane,
                        ref_width,
                        ref_height,
                        x_int + col as i32,
                        y_int + row as i32,
                    ) as u16;
                }
            }
        }
        return;
    }

    // In-bounds fast path: needs x_int..x_int+w+1 and y_int..y_int+h+1
    let w_i32 = ref_width as i32;
    let h_i32 = ref_height as i32;
    let in_bounds = x_int >= 0
        && y_int >= 0
        && (x_int + block_w as i32) < w_i32
        && (y_int + block_h as i32) < h_i32;

    if in_bounds {
        let c00: i32 = (8 - frac_x) * (8 - frac_y);
        let c01: i32 = frac_x * (8 - frac_y);
        let c10: i32 = (8 - frac_x) * frac_y;
        let c11: i32 = frac_x * frac_y;

        let x_u = x_int as usize;
        let y_u = y_int as usize;
        let top_off = y_u * ref_width + x_u;

        // Guard against ref_plane size mismatch (e.g. SPS change in malformed stream)
        let last_idx = top_off + block_h * ref_width + block_w;
        if last_idx >= ref_plane.len() {
            return; // ref plane too small, skip MC
        }

        for row in 0..block_h {
            let row_top = top_off + row * ref_width;
            let row_bot = row_top + ref_width;
            for i in 0..block_w {
                let val = c00 * ref_plane[row_top + i] as i32
                    + c01 * ref_plane[row_top + i + 1] as i32
                    + c10 * ref_plane[row_bot + i] as i32
                    + c11 * ref_plane[row_bot + i + 1] as i32;
                output[row * block_w + i] = ((val + 32) >> 6) as u16;
            }
        }
        return;
    }

    // Boundary fallback: per-pixel with clamping
    for row in 0..block_h {
        for col in 0..block_w {
            let xf = x_int + col as i32;
            let yf = y_int + row as i32;
            let a = ref_chroma(ref_plane, ref_width, ref_height, xf, yf);
            let b = ref_chroma(ref_plane, ref_width, ref_height, xf + 1, yf);
            let c = ref_chroma(ref_plane, ref_width, ref_height, xf, yf + 1);
            let d = ref_chroma(ref_plane, ref_width, ref_height, xf + 1, yf + 1);

            let val = (8 - frac_x) * (8 - frac_y) * a
                + frac_x * (8 - frac_y) * b
                + (8 - frac_x) * frac_y * c
                + frac_x * frac_y * d;
            output[row * block_w + col] = ((val + 32) >> 6) as u16;
        }
    }
}

/// Bi-prediction averaging (spec 8.4.2.3.2).
/// `output[i] = (pred_l0[i] + pred_l1[i] + 1) >> 1` for each pixel.
pub fn bi_pred_avg(pred_l0: &[u16], pred_l1: &[u16], output: &mut [u16]) {
    for (o, (&a, &b)) in output.iter_mut().zip(pred_l0.iter().zip(pred_l1.iter())) {
        *o = ((a as u32 + b as u32 + 1) >> 1) as u16;
    }
}

/// Apply explicit weighted prediction to uni-directional MC output (spec 8.4.2.3.1).
/// `output[i] = Clip1((pred[i] * weight + (1 << (log2_denom - 1))) >> log2_denom + offset)`
/// When log2_denom == 0, the rounding term is 0. `offset` must already be
/// scaled by `1 << (BitDepth - 8)`; `max` is `(1 << BitDepth) - 1`.
pub fn weighted_uni(pred: &mut [u16], log2_denom: u32, weight: i32, offset: i32, max: i32) {
    if log2_denom == 0 {
        for p in pred.iter_mut() {
            *p = (*p as i32)
                .wrapping_mul(weight)
                .wrapping_add(offset)
                .clamp(0, max) as u16;
        }
    } else {
        let round = 1i32 << (log2_denom - 1);
        for p in pred.iter_mut() {
            *p = ((*p as i32).wrapping_mul(weight).wrapping_add(round) >> log2_denom)
                .wrapping_add(offset)
                .clamp(0, max) as u16;
        }
    }
}

/// Apply explicit weighted bi-prediction (spec 8.4.2.3.2).
/// Formula: `Clip1((p0*w0 + p1*w1 + round) >> (denom+1) + (o0+o1+1)>>1)`
/// Offsets must already be scaled by `1 << (BitDepth - 8)`.
#[allow(clippy::too_many_arguments)]
pub fn weighted_bi(
    pred_l0: &[u16],
    pred_l1: &[u16],
    output: &mut [u16],
    log2_denom: u32,
    w0: i32,
    o0: i32,
    w1: i32,
    o1: i32,
    max: i32,
) {
    let round = 1i32 << log2_denom;
    let offset = o0.wrapping_add(o1).wrapping_add(1) >> 1;
    let shift = log2_denom + 1;
    for (o, (&a, &b)) in output.iter_mut().zip(pred_l0.iter().zip(pred_l1.iter())) {
        *o = ((a as i32)
            .wrapping_mul(w0)
            .wrapping_add((b as i32).wrapping_mul(w1))
            .wrapping_add(round)
            >> shift)
            .wrapping_add(offset)
            .clamp(0, max) as u16;
    }
}

/// Apply implicit weighted bi-prediction for B-slices (spec 8.4.2.3.2).
/// Uses POC-distance-derived weights with fixed log2_denom=5.
pub fn weighted_bi_implicit(
    pred_l0: &[u16],
    pred_l1: &[u16],
    output: &mut [u16],
    w0: i32,
    w1: i32,
    max: i32,
) {
    let round = 1i32 << 5; // 1 << log2_denom where log2_denom=5
    for (o, (&a, &b)) in output.iter_mut().zip(pred_l0.iter().zip(pred_l1.iter())) {
        *o = (a as i32)
            .wrapping_mul(w0)
            .wrapping_add((b as i32).wrapping_mul(w1))
            .wrapping_add(round)
            .wrapping_shr(6)
            .clamp(0, max) as u16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::rc::Rc;

    fn make_ref_pic(width: u32, height: u32, y_data: Vec<u16>) -> Rc<DecodedPicture> {
        make_ref_pic_bd(width, height, y_data, 8)
    }

    fn make_ref_pic_bd(
        width: u32,
        height: u32,
        y_data: Vec<u16>,
        bit_depth: u8,
    ) -> Rc<DecodedPicture> {
        let uv_size = (width / 2 * height / 2) as usize;
        Rc::new(DecodedPicture {
            y: y_data,
            u: vec![128; uv_size],
            v: vec![128; uv_size],
            width,
            height,
            frame_num: 0,
            pic_order_cnt: 0,
            mv_l0: vec![],
            ref_idx_l0: vec![],
            ref_poc_l0: vec![],
            mv_l1: vec![],
            ref_idx_l1: vec![],
            mb_width: width / 16,
            is_intra: false,
            structure: crate::dpb::PictureStructure::Frame,
            bit_depth_luma: bit_depth,
            bit_depth_chroma: bit_depth,
        })
    }

    #[test]
    fn test_integer_pel_copy() {
        // 8x8 reference with gradient
        let mut y = vec![0u16; 64];
        for r in 0..8 {
            for c in 0..8 {
                y[r * 8 + c] = (r * 16 + c * 4) as u16;
            }
        }
        let pic = make_ref_pic(8, 8, y.clone());

        // Integer-pel MC (dx=0, dy=0) should be a direct copy
        let mut out = vec![0u16; 16]; // 4x4 block at (2,1)
        luma_mc_stride(&pic, 1, 2, 0, 0, 4, 4, &mut out, pic.width as usize, 0);
        for r in 0..4 {
            for c in 0..4 {
                assert_eq!(out[r * 4 + c], y[(r + 2) * 8 + (c + 1)]);
            }
        }
    }

    #[test]
    fn test_integer_pel_with_mv() {
        // 8x8 uniform reference
        let y = vec![42u16; 64];
        let pic = make_ref_pic(8, 8, y);

        // MV = (4, 8) in quarter-pel = (1, 2) full-pel offset
        let mut out = vec![0u16; 16];
        luma_mc_stride(&pic, 0, 0, 4, 8, 4, 4, &mut out, pic.width as usize, 0);
        // With uniform reference, all outputs should be 42
        assert!(out.iter().all(|&v| v == 42));
    }

    #[test]
    fn test_half_pel_horizontal() {
        // 16x1 reference: known values for 6-tap filter
        let mut y = vec![128u16; 16];
        // Set a step edge: left half = 0, right half = 255
        for c in 0..8 {
            y[c] = 0;
        }
        for c in 8..16 {
            y[c] = 255;
        }
        let pic = make_ref_pic(16, 1, y);

        // Half-pel horizontal at x=7 (the edge): MV dx=2 (half-pel), dy=0
        let mut out = [0u16; 1];
        luma_mc_stride(&pic, 7, 0, 2, 0, 1, 1, &mut out, pic.width as usize, 0);
        // 6-tap at x=7: samples at x=5..10 = [0, 0, 0, 255, 255, 255]
        // (0 - 0 + 0 + 20*255 - 5*255 + 255 + 16) >> 5 = (0 + 5100 - 1275 + 255 + 16) >> 5
        // = 4096 >> 5 = 128
        assert_eq!(out[0], 128);
    }

    #[test]
    fn test_half_pel_vertical() {
        // 1x16 reference with step edge at row 8
        let mut y = vec![0u16; 16];
        for r in 8..16 {
            y[r] = 255;
        }
        let pic = make_ref_pic(1, 16, y);

        let mut out = [0u16; 1];
        luma_mc_stride(&pic, 0, 7, 0, 2, 1, 1, &mut out, pic.width as usize, 0);
        // Same as horizontal but vertical: should also give 128
        assert_eq!(out[0], 128);
    }

    #[test]
    fn test_uniform_ref_all_frac_positions() {
        // Uniform reference: all fractional positions should give the same value
        let y = vec![100u16; 256];
        let pic = make_ref_pic(16, 16, y);

        for frac_x in 0..4 {
            for frac_y in 0..4 {
                let mut out = [0u16; 1];
                luma_mc_stride(
                    &pic,
                    4,
                    4,
                    frac_x,
                    frac_y,
                    1,
                    1,
                    &mut out,
                    pic.width as usize,
                    0,
                );
                assert_eq!(
                    out[0], 100,
                    "frac ({},{}) should give 100 for uniform ref",
                    frac_x, frac_y
                );
            }
        }
    }

    #[test]
    fn test_chroma_mc_integer() {
        let plane = vec![200u16; 64]; // 8x8 chroma
        let mut out = vec![0u16; 16]; // 4x4 block
        chroma_mc(&plane, 8, 8, 0, 0, 0, 0, 4, 4, &mut out);
        assert!(out.iter().all(|&v| v == 200));
    }

    #[test]
    fn test_chroma_mc_half_pel() {
        // 4x1 chroma: [0, 255, 0, 255]
        let plane = vec![0, 255, 0, 255];
        let mut out = [0u16; 1];
        // dx=4 = half-pel (4/8 = 0.5), dy=0
        chroma_mc(&plane, 4, 1, 0, 0, 4, 0, 1, 1, &mut out);
        // Bilinear: (8-4)*8*0 + 4*8*255 + 0 + 0 = 8160. (8160+32)>>6 = 128 (approx)
        assert_eq!(out[0], 128);
    }

    #[test]
    fn test_boundary_clipping() {
        // 4x4 reference, MV pointing outside
        let y: Vec<u16> = (0..16).collect();
        let pic = make_ref_pic(4, 4, y);

        // MC at (0,0) with MV=(-4, -4) in quarter-pel = (-1, -1) full-pel
        // Should clamp to (0,0) and read the top-left corner value
        let mut out = [0u16; 1];
        luma_mc_stride(&pic, 0, 0, -4, -4, 1, 1, &mut out, pic.width as usize, 0);
        assert_eq!(out[0], 0); // clamped to (0,0)
    }

    #[test]
    fn test_negative_mv_fractional() {
        // Verify negative MV fractional extraction
        let y = vec![128u16; 256];
        let pic = make_ref_pic(16, 16, y);

        // MV = (-1, -1) quarter-pel: frac should be (3, 3), int offset = (-1, -1)
        let mut out = [0u16; 1];
        luma_mc_stride(&pic, 8, 8, -1, -1, 1, 1, &mut out, pic.width as usize, 0);
        // Uniform ref, so result should be 128 regardless
        assert_eq!(out[0], 128);
    }

    #[test]
    fn test_half_pel_clips_to_10bit() {
        // 10-bit step edge: the 6-tap overshoot must clip to 1023, not 255.
        let mut y = vec![0u16; 16];
        for c in 8..16 {
            y[c] = 1023;
        }
        let pic = make_ref_pic_bd(16, 1, y, 10);
        let mut out = [0u16; 1];
        luma_mc_stride(&pic, 8, 0, 2, 0, 1, 1, &mut out, pic.width as usize, 0);
        assert_eq!(out[0], 1023);
    }

    #[test]
    fn test_weighted_uni_high_bit_depth() {
        let mut p = [1000u16, 10];
        // offset already scaled by 1 << (10 - 8)
        weighted_uni(&mut p, 0, 1, 40 << 2, 1023);
        assert_eq!(p, [1023, 170]);
    }
}
