/// I16x16 intra prediction (H.264 spec 8.3.3).
/// `mode`: 0=vertical, 1=horizontal, 2=DC, 3=plane.
/// For pixels at the top-left of the frame with no neighbors, only DC mode
/// with value 1 << (BitDepth - 1) is valid.
pub fn predict_intra_16x16(
    mode: u8,
    above: Option<&[u16]>, // 16 pixels from row above
    left: Option<&[u16]>,  // 16 pixels from column to left
    above_left: Option<u16>,
    output: &mut [u16; 256],
    bit_depth: u32,
) {
    // Unavailable-neighbour default and Clip1 maximum (spec 8.3).
    let bit_depth = bit_depth.clamp(8, 14);
    let dflt = 1u16 << (bit_depth - 1);
    let max = (1i32 << bit_depth) - 1;
    let dflt16 = [dflt; 16];
    match mode {
        0 => {
            // Vertical: copy above row to all rows
            let above = above.unwrap_or(&dflt16);
            for row in 0..16 {
                output[row * 16..row * 16 + 16].copy_from_slice(&above[..16]);
            }
        }
        1 => {
            // Horizontal: copy left column to all columns
            let left = left.unwrap_or(&dflt16);
            for row in 0..16 {
                for col in 0..16 {
                    output[row * 16 + col] = left[row];
                }
            }
        }
        2 => {
            // DC: average of available above and left samples
            let dc = match (above, left) {
                (Some(a), Some(l)) => {
                    let sum: u32 = a[..16].iter().map(|&x| x as u32).sum::<u32>()
                        + l[..16].iter().map(|&x| x as u32).sum::<u32>();
                    ((sum + 16) >> 5) as u16
                }
                (Some(a), None) => {
                    let sum: u32 = a[..16].iter().map(|&x| x as u32).sum();
                    ((sum + 8) >> 4) as u16
                }
                (None, Some(l)) => {
                    let sum: u32 = l[..16].iter().map(|&x| x as u32).sum();
                    ((sum + 8) >> 4) as u16
                }
                (None, None) => dflt,
            };
            output.fill(dc);
        }
        3 => {
            // Plane prediction
            let above = above.unwrap_or(&dflt16);
            let left = left.unwrap_or(&dflt16);
            let _p = above_left.unwrap_or(dflt);

            let mut h: i32 = 0;
            let mut v: i32 = 0;
            for i in 0..8 {
                // p[6-i, -1] and p[-1, 6-i]: when i==7, index -1 = above_left pixel
                let above_neg = if i < 7 {
                    above[6 - i] as i32
                } else {
                    _p as i32
                };
                let left_neg = if i < 7 { left[6 - i] as i32 } else { _p as i32 };
                h += (i as i32 + 1) * (above[8 + i] as i32 - above_neg);
                v += (i as i32 + 1) * (left[8 + i] as i32 - left_neg);
            }
            let a_val = 16 * (above[15] as i32 + left[15] as i32);
            let b_val = (5 * h + 32) >> 6;
            let c_val = (5 * v + 32) >> 6;

            for y in 0..16 {
                for x in 0..16 {
                    let val = (a_val + b_val * (x as i32 - 7) + c_val * (y as i32 - 7) + 16) >> 5;
                    output[y * 16 + x] = val.clamp(0, max) as u16;
                }
            }
        }
        _ => {
            output.fill(dflt);
        }
    }
}

/// I4x4 intra prediction (H.264 spec 8.3.1.2).
/// `mode`: 0-8 prediction modes.
/// `above`: 8 pixels above (4 directly above + 4 above-right), None if unavailable.
/// `left`: 4 pixels to the left, None if unavailable.
/// `above_left`: pixel diagonally above-left, None if unavailable.
/// `output`: 4x4 block in raster order (16 bytes).
pub fn predict_intra_4x4(
    mode: u8,
    above: Option<&[u16]>,
    left: Option<&[u16]>,
    above_left: Option<u16>,
    output: &mut [u16; 16],
    bit_depth: u32,
) {
    // Unavailable-neighbour default 1 << (BitDepthY - 1) (spec 8.3).
    let bit_depth = bit_depth.clamp(8, 14);
    let dflt = 1u16 << (bit_depth - 1);
    let dflt8 = [dflt; 8];
    let dflt4 = [dflt; 4];
    match mode {
        0 => {
            // Vertical
            let a = above.unwrap_or(&dflt8);
            for row in 0..4 {
                output[row * 4..row * 4 + 4].copy_from_slice(&a[..4]);
            }
        }
        1 => {
            // Horizontal
            let l = left.unwrap_or(&dflt4);
            for row in 0..4 {
                for col in 0..4 {
                    output[row * 4 + col] = l[row];
                }
            }
        }
        2 => {
            // DC
            let dc = match (above, left) {
                (Some(a), Some(l)) => {
                    let sum: u32 = a[..4].iter().map(|&x| x as u32).sum::<u32>()
                        + l[..4].iter().map(|&x| x as u32).sum::<u32>();
                    ((sum + 4) >> 3) as u16
                }
                (Some(a), None) => {
                    let sum: u32 = a[..4].iter().map(|&x| x as u32).sum();
                    ((sum + 2) >> 2) as u16
                }
                (None, Some(l)) => {
                    let sum: u32 = l[..4].iter().map(|&x| x as u32).sum();
                    ((sum + 2) >> 2) as u16
                }
                (None, None) => dflt,
            };
            output.fill(dc);
        }
        3 => {
            // Diagonal Down-Left
            let a = above.unwrap_or(&dflt8);
            for y in 0..4 {
                for x in 0..4 {
                    if x == 3 && y == 3 {
                        output[y * 4 + x] = ((a[6] as u32 + 3 * a[7] as u32 + 2) >> 2) as u16;
                    } else {
                        let i = x + y;
                        output[y * 4 + x] =
                            ((a[i] as u32 + 2 * a[i + 1] as u32 + a[i + 2] as u32 + 2) >> 2) as u16;
                    }
                }
            }
        }
        4 => {
            // Diagonal Down-Right (spec 8.3.1.2.5)
            // Build reference pixel array: [left[3], left[2], left[1], left[0],
            //                               above_left, above[0..3]]
            // pred[x,y] = (ref[3-y+x] + 2*ref[4-y+x] + ref[5-y+x] + 2) >> 2
            let a = above.unwrap_or(&dflt8);
            let l = left.unwrap_or(&dflt4);
            let p = above_left.unwrap_or(dflt);
            let r = [
                l[3] as u32,
                l[2] as u32,
                l[1] as u32,
                l[0] as u32,
                p as u32,
                a[0] as u32,
                a[1] as u32,
                a[2] as u32,
                a[3] as u32,
            ];
            for y in 0..4usize {
                for x in 0..4usize {
                    let i = 3 + x - y;
                    output[y * 4 + x] = ((r[i] + 2 * r[i + 1] + r[i + 2] + 2) >> 2) as u16;
                }
            }
        }
        5 => {
            // Vertical-Right (spec 8.3.1.2.6)
            let a = above.unwrap_or(&dflt8);
            let l = left.unwrap_or(&dflt4);
            let p = above_left.unwrap_or(dflt);
            let (lt, t0, t1, t2, t3) =
                (p as u32, a[0] as u32, a[1] as u32, a[2] as u32, a[3] as u32);
            let (l0, l1, l2) = (l[0] as u32, l[1] as u32, l[2] as u32);
            // Row 0: avg of above pairs
            output[0] = ((lt + t0 + 1) >> 1) as u16;
            output[1] = ((t0 + t1 + 1) >> 1) as u16;
            output[2] = ((t1 + t2 + 1) >> 1) as u16;
            output[3] = ((t2 + t3 + 1) >> 1) as u16;
            // Row 1: filtered above
            output[4] = ((l0 + 2 * lt + t0 + 2) >> 2) as u16;
            output[5] = ((lt + 2 * t0 + t1 + 2) >> 2) as u16;
            output[6] = ((t0 + 2 * t1 + t2 + 2) >> 2) as u16;
            output[7] = ((t1 + 2 * t2 + t3 + 2) >> 2) as u16;
            // Row 2: shifted from row 0
            output[8] = ((lt + 2 * l0 + l1 + 2) >> 2) as u16;
            output[9] = output[0]; // same as (lt + t0 + 1) >> 1
            output[10] = output[1];
            output[11] = output[2];
            // Row 3: shifted from row 1
            output[12] = ((l0 + 2 * l1 + l2 + 2) >> 2) as u16;
            output[13] = output[4]; // same as (l0 + 2*lt + t0 + 2) >> 2
            output[14] = output[5];
            output[15] = output[6];
        }
        6 => {
            // Horizontal-Down (spec 8.3.1.2.7)
            let a = above.unwrap_or(&dflt8);
            let l = left.unwrap_or(&dflt4);
            let p = above_left.unwrap_or(dflt);
            let (lt, t0, t1, t2) = (p as u32, a[0] as u32, a[1] as u32, a[2] as u32);
            let (l0, l1, l2, l3) = (l[0] as u32, l[1] as u32, l[2] as u32, l[3] as u32);
            // Row 0
            output[0] = ((lt + l0 + 1) >> 1) as u16;
            output[1] = ((l0 + 2 * lt + t0 + 2) >> 2) as u16;
            output[2] = ((lt + 2 * t0 + t1 + 2) >> 2) as u16;
            output[3] = ((t0 + 2 * t1 + t2 + 2) >> 2) as u16;
            // Row 1
            output[4] = ((l0 + l1 + 1) >> 1) as u16;
            output[5] = ((lt + 2 * l0 + l1 + 2) >> 2) as u16;
            output[6] = output[0]; // same as (lt + l0 + 1) >> 1
            output[7] = output[1]; // same as (l0 + 2*lt + t0 + 2) >> 2
                                   // Row 2
            output[8] = ((l1 + l2 + 1) >> 1) as u16;
            output[9] = ((l0 + 2 * l1 + l2 + 2) >> 2) as u16;
            output[10] = output[4]; // same as (l0 + l1 + 1) >> 1
            output[11] = output[5]; // same as (lt + 2*l0 + l1 + 2) >> 2
                                    // Row 3
            output[12] = ((l2 + l3 + 1) >> 1) as u16;
            output[13] = ((l1 + 2 * l2 + l3 + 2) >> 2) as u16;
            output[14] = output[8]; // same as (l1 + l2 + 1) >> 1
            output[15] = output[9]; // same as (l0 + 2*l1 + l2 + 2) >> 2
        }
        7 => {
            // Vertical-Left (spec 8.3.1.2.8)
            let a = above.unwrap_or(&dflt8);
            for y in 0..4 {
                for x in 0..4 {
                    let i = x + (y >> 1);
                    output[y * 4 + x] = if y % 2 == 0 {
                        ((a[i] as u32 + a[i + 1] as u32 + 1) >> 1) as u16
                    } else {
                        ((a[i] as u32 + 2 * a[i + 1] as u32 + a[i + 2] as u32 + 2) >> 2) as u16
                    };
                }
            }
        }
        8 => {
            // Horizontal-Up (spec 8.3.1.2.9)
            let l = left.unwrap_or(&dflt4);
            for y in 0..4 {
                for x in 0..4 {
                    let zh = x + 2 * y;
                    output[y * 4 + x] = if zh < 5 {
                        let i = y + (x >> 1);
                        if zh % 2 == 0 {
                            ((l[i] as u32 + l[i + 1] as u32 + 1) >> 1) as u16
                        } else {
                            ((l[i] as u32 + 2 * l[i + 1] as u32 + l[i + 2] as u32 + 2) >> 2) as u16
                        }
                    } else if zh == 5 {
                        ((l[2] as u32 + 3 * l[3] as u32 + 2) >> 2) as u16
                    } else {
                        l[3]
                    };
                }
            }
        }
        _ => {
            output.fill(dflt);
        }
    }
}

/// Chroma 8x8 intra prediction (H.264 spec 8.3.4).
/// `mode`: 0=DC, 1=horizontal, 2=vertical, 3=plane.
pub fn predict_chroma_8x8(
    mode: u8,
    above: Option<&[u16]>, // 8 pixels from row above
    left: Option<&[u16]>,  // 8 pixels from column to left
    above_left: Option<u16>,
    output: &mut [u16; 64],
    bit_depth: u32,
) {
    // Unavailable-neighbour default and Clip1 maximum (spec 8.3).
    let bit_depth = bit_depth.clamp(8, 14);
    let dflt = 1u16 << (bit_depth - 1);
    let max = (1i32 << bit_depth) - 1;
    let dflt16 = [dflt; 16];
    match mode {
        0 => {
            // DC: per-4x4-sub-block DC prediction (spec 8.3.4.1)
            // 4 quadrants: TL(rows 0-3, cols 0-3), TR(rows 0-3, cols 4-7),
            //              BL(rows 4-7, cols 0-3), BR(rows 4-7, cols 4-7)
            let (dc_tl, dc_tr, dc_bl, dc_br) = match (above, left) {
                (Some(a), Some(l)) => {
                    let sum_a_l: u32 = a[..4].iter().map(|&x| x as u32).sum::<u32>()
                        + l[..4].iter().map(|&x| x as u32).sum::<u32>();
                    let sum_a_r: u32 = a[4..8].iter().map(|&x| x as u32).sum();
                    let sum_l_b: u32 = l[4..8].iter().map(|&x| x as u32).sum();
                    (
                        ((sum_a_l + 4) >> 3) as u16,
                        ((sum_a_r + 2) >> 2) as u16,
                        ((sum_l_b + 2) >> 2) as u16,
                        ((sum_a_r + sum_l_b + 4) >> 3) as u16,
                    )
                }
                (Some(a), None) => {
                    let sum_l: u32 = a[..4].iter().map(|&x| x as u32).sum();
                    let sum_r: u32 = a[4..8].iter().map(|&x| x as u32).sum();
                    let dc_l = ((sum_l + 2) >> 2) as u16;
                    let dc_r = ((sum_r + 2) >> 2) as u16;
                    (dc_l, dc_r, dc_l, dc_r)
                }
                (None, Some(l)) => {
                    let sum_t: u32 = l[..4].iter().map(|&x| x as u32).sum();
                    let sum_b: u32 = l[4..8].iter().map(|&x| x as u32).sum();
                    let dc_t = ((sum_t + 2) >> 2) as u16;
                    let dc_b = ((sum_b + 2) >> 2) as u16;
                    (dc_t, dc_t, dc_b, dc_b)
                }
                (None, None) => (dflt, dflt, dflt, dflt),
            };
            for row in 0..4 {
                for col in 0..4 {
                    output[row * 8 + col] = dc_tl;
                }
                for col in 4..8 {
                    output[row * 8 + col] = dc_tr;
                }
            }
            for row in 4..8 {
                for col in 0..4 {
                    output[row * 8 + col] = dc_bl;
                }
                for col in 4..8 {
                    output[row * 8 + col] = dc_br;
                }
            }
        }
        1 => {
            // Horizontal
            let left = left.unwrap_or(&dflt16);
            for row in 0..8 {
                for col in 0..8 {
                    output[row * 8 + col] = left[row];
                }
            }
        }
        2 => {
            // Vertical
            let above = above.unwrap_or(&dflt16);
            for row in 0..8 {
                output[row * 8..row * 8 + 8].copy_from_slice(&above[..8]);
            }
        }
        3 => {
            // Plane
            let above = above.unwrap_or(&dflt16);
            let left = left.unwrap_or(&dflt16);
            let _p = above_left.unwrap_or(dflt);

            let mut h: i32 = 0;
            let mut v: i32 = 0;
            for i in 0..4 {
                // p[2-i, -1] and p[-1, 2-i]: when i==3, index -1 = above_left pixel
                let above_neg = if i < 3 {
                    above[2 - i] as i32
                } else {
                    _p as i32
                };
                let left_neg = if i < 3 { left[2 - i] as i32 } else { _p as i32 };
                h += (i as i32 + 1) * (above[4 + i] as i32 - above_neg);
                v += (i as i32 + 1) * (left[4 + i] as i32 - left_neg);
            }
            let a_val = 16 * (above[7] as i32 + left[7] as i32);
            let b_val = (17 * h + 16) >> 5;
            let c_val = (17 * v + 16) >> 5;

            for y in 0..8 {
                for x in 0..8 {
                    let val = (a_val + b_val * (x as i32 - 3) + c_val * (y as i32 - 3) + 16) >> 5;
                    output[y * 8 + x] = val.clamp(0, max) as u16;
                }
            }
        }
        _ => {
            output.fill(dflt);
        }
    }
}

/// I8x8 intra prediction (H.264 spec 8.3.2.2).
/// Same 9 modes as I4x4 but at 8×8 granularity with filtered reference samples.
///
/// `above`: 16 pixels above (8 directly above + 8 above-right), None if unavailable.
/// `left`: 8 pixels to the left, None if unavailable.
/// `above_left`: pixel diagonally above-left, None if unavailable.
/// `has_topright`: whether above-right 8 pixels are available.
/// `output`: 8×8 block in raster order (64 bytes).
pub fn predict_intra_8x8(
    mode: u8,
    above: Option<&[u16]>,
    left: Option<&[u16]>,
    above_left: Option<u16>,
    has_topright: bool,
    output: &mut [u16; 64],
    bit_depth: u32,
) {
    // Unavailable-neighbour default 1 << (BitDepthY - 1) (spec 8.3).
    let bit_depth = bit_depth.clamp(8, 14);
    let dflt = 1u16 << (bit_depth - 1);
    // Build filtered reference samples (low-pass: (a + 2b + c + 2) >> 2)
    let def = dflt;
    let al = above_left.unwrap_or(def) as i32;

    // Filtered left samples (l0..l7)
    let mut fl = [dflt as i32; 8];
    if let Some(l) = left {
        fl[0] = (if above_left.is_some() {
            al
        } else {
            l[0] as i32
        } + 2 * l[0] as i32
            + l[1] as i32
            + 2)
            >> 2;
        for i in 1..7 {
            fl[i] = (l[i - 1] as i32 + 2 * l[i] as i32 + l[i + 1] as i32 + 2) >> 2;
        }
        fl[7] = (l[6] as i32 + 3 * l[7] as i32 + 2) >> 2;
    }

    // Filtered top samples (t0..t7)
    let mut ft = [dflt as i32; 8];
    if let Some(a) = above {
        ft[0] = (if above_left.is_some() {
            al
        } else {
            a[0] as i32
        } + 2 * a[0] as i32
            + a[1] as i32
            + 2)
            >> 2;
        for i in 1..7 {
            ft[i] = (a[i - 1] as i32 + 2 * a[i] as i32 + a[i + 1] as i32 + 2) >> 2;
        }
        ft[7] = if has_topright {
            (a[6] as i32 + 2 * a[7] as i32 + a[8] as i32 + 2) >> 2
        } else {
            (a[6] as i32 + 3 * a[7] as i32 + 2) >> 2
        };
    }

    // Filtered top-right samples (t8..t15)
    let mut ftr = [0i32; 8];
    if let Some(a) = above {
        if has_topright {
            for i in 0..7 {
                ftr[i] = (a[7 + i] as i32 + 2 * a[8 + i] as i32 + a[9 + i] as i32 + 2) >> 2;
            }
            ftr[7] = (a[14] as i32 + 3 * a[15] as i32 + 2) >> 2;
        } else {
            ftr.fill(a[7] as i32);
        }
    }

    // Filtered top-left
    let flt = match (above_left, left, above) {
        (Some(_), Some(l), Some(a)) => (l[0] as i32 + 2 * al + a[0] as i32 + 2) >> 2,
        (Some(_), _, _) => al,
        _ => dflt as i32,
    };

    match mode {
        0 => {
            // Vertical: replicate filtered top row
            for y in 0..8 {
                for x in 0..8 {
                    output[y * 8 + x] = ft[x] as u16;
                }
            }
        }
        1 => {
            // Horizontal: replicate filtered left column
            for y in 0..8 {
                for x in 0..8 {
                    output[y * 8 + x] = fl[y] as u16;
                }
            }
        }
        2 => {
            // DC
            let dc = match (above, left) {
                (Some(_), Some(_)) => {
                    let sum: i32 = ft.iter().sum::<i32>() + fl.iter().sum::<i32>();
                    ((sum + 8) >> 4) as u16
                }
                (Some(_), None) => {
                    let sum: i32 = ft.iter().sum();
                    ((sum + 4) >> 3) as u16
                }
                (None, Some(_)) => {
                    let sum: i32 = fl.iter().sum();
                    ((sum + 4) >> 3) as u16
                }
                (None, None) => dflt,
            };
            output.fill(dc);
        }
        3 => {
            // Diagonal Down-Left
            // Combine t0..t7 and t8..t15 (top-right)
            let mut t = [0i32; 16];
            t[..8].copy_from_slice(&ft);
            t[8..].copy_from_slice(&ftr);
            for y in 0..8 {
                for x in 0..8 {
                    let i = x + y;
                    output[y * 8 + x] = if x == 7 && y == 7 {
                        ((t[14] + 3 * t[15] + 2) >> 2) as u16
                    } else {
                        ((t[i] + 2 * t[i + 1] + t[i + 2] + 2) >> 2) as u16
                    };
                }
            }
        }
        4 => {
            // Diagonal Down-Right (spec 8.3.2.2.6)
            let mut t = [0i32; 16];
            t[..8].copy_from_slice(&ft);
            t[8..].copy_from_slice(&ftr);
            for y in 0..8 {
                for x in 0..8 {
                    output[y * 8 + x] = if x > y {
                        let i = x - y - 1;
                        let p0 = if i == 0 { flt } else { t[i - 1] };
                        ((p0 + 2 * t[i] + t[i + 1] + 2) >> 2) as u16
                    } else if x < y {
                        let i = y - x - 1;
                        let p0 = if i == 0 { flt } else { fl[i - 1] };
                        ((p0 + 2 * fl[i] + fl.get(i + 1).copied().unwrap_or(fl[7]) + 2) >> 2) as u16
                    } else {
                        ((fl[0] + 2 * flt + ft[0] + 2) >> 2) as u16
                    };
                }
            }
        }
        5 => {
            // Vertical-Right
            for y in 0..8 {
                for x in 0..8 {
                    let zv = 2 * x as i32 - y as i32;
                    output[y * 8 + x] = if zv >= 0 {
                        let i = x - (y >> 1);
                        if zv & 1 == 0 {
                            ((ft.get(i.wrapping_sub(1)).copied().unwrap_or(flt) + ft[i] + 1) >> 1)
                                as u16
                        } else {
                            let p0 = if i >= 2 {
                                ft[i - 2]
                            } else if i == 1 {
                                flt
                            } else {
                                fl[0]
                            };
                            ((p0 + 2 * ft.get(i.wrapping_sub(1)).copied().unwrap_or(flt)
                                + ft[i]
                                + 2)
                                >> 2) as u16
                        }
                    } else if zv == -1 {
                        ((ft[0] + 2 * flt + fl[0] + 2) >> 2) as u16
                    } else {
                        // zVR < -1: 3-tap filter toward top-left (spec 8.3.2.2.7)
                        let i = y - 2 * x - 1;
                        let p0 = fl[i];
                        let p1 = if i >= 1 { fl[i - 1] } else { flt };
                        let p2 = if i >= 2 {
                            fl[i - 2]
                        } else if i == 1 {
                            flt
                        } else {
                            ft[0]
                        };
                        ((p0 + 2 * p1 + p2 + 2) >> 2) as u16
                    };
                }
            }
        }
        6 => {
            // Horizontal-Down
            for y in 0..8 {
                for x in 0..8 {
                    let zh = 2 * y as i32 - x as i32;
                    output[y * 8 + x] = if zh >= 0 {
                        let i = y - (x >> 1);
                        if zh & 1 == 0 {
                            ((fl.get(i.wrapping_sub(1)).copied().unwrap_or(flt) + fl[i] + 1) >> 1)
                                as u16
                        } else {
                            let p0 = if i >= 2 {
                                fl[i - 2]
                            } else if i == 1 {
                                flt
                            } else {
                                ft[0]
                            };
                            ((p0 + 2 * fl.get(i.wrapping_sub(1)).copied().unwrap_or(flt)
                                + fl[i]
                                + 2)
                                >> 2) as u16
                        }
                    } else if zh == -1 {
                        ((fl[0] + 2 * flt + ft[0] + 2) >> 2) as u16
                    } else {
                        // zHD < -1: use filtered above samples in descending order
                        // ft[-1] is flt; for index -1, use flt
                        let i = x as i32 - 2 * y as i32;
                        let get_ft = |idx: i32| -> i32 {
                            if idx < 0 {
                                flt
                            } else {
                                ft[idx as usize]
                            }
                        };
                        ((get_ft(i - 3) + 2 * get_ft(i - 2) + get_ft(i - 1) + 2) >> 2) as u16
                    };
                }
            }
        }
        7 => {
            // Vertical-Left
            let mut t = [0i32; 16];
            t[..8].copy_from_slice(&ft);
            t[8..].copy_from_slice(&ftr);
            for y in 0..8 {
                for x in 0..8 {
                    let i = x + (y >> 1);
                    output[y * 8 + x] = if y & 1 == 0 {
                        ((t[i] + t[i + 1] + 1) >> 1) as u16
                    } else {
                        ((t[i] + 2 * t[i + 1] + t[i + 2] + 2) >> 2) as u16
                    };
                }
            }
        }
        8 => {
            // Horizontal-Up
            for y in 0..8 {
                for x in 0..8 {
                    let i = y + (x >> 1);
                    output[y * 8 + x] = if i >= 7 {
                        fl[7] as u16
                    } else if x & 1 == 0 {
                        ((fl[i] + fl[i + 1] + 1) >> 1) as u16
                    } else {
                        ((fl[i] + 2 * fl[i + 1] + fl.get(i + 2).copied().unwrap_or(fl[7]) + 2) >> 2)
                            as u16
                    };
                }
            }
        }
        _ => output.fill(dflt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference implementation: compute I4x4 prediction using H.264 spec formulas
    /// with explicit p[x,-1]/p[-1,y]/p[-1,-1] indexing.
    fn spec_predict_4x4(mode: u8, above: &[u16; 8], left: &[u16; 4], above_left: u16) -> [u16; 16] {
        let pa = |x: i32| -> i32 {
            if x == -1 {
                above_left as i32
            } else {
                above[x as usize] as i32
            }
        };
        let pl = |y: i32| -> i32 {
            if y == -1 {
                above_left as i32
            } else {
                left[y as usize] as i32
            }
        };

        let mut out = [0u16; 16];
        for y in 0..4i32 {
            for x in 0..4i32 {
                let v: i32 = match mode {
                    0 => pa(x),
                    1 => pl(y),
                    3 => {
                        if x == 3 && y == 3 {
                            (pa(6) + 3 * pa(7) + 2) >> 2
                        } else {
                            (pa(x + y) + 2 * pa(x + y + 1) + pa(x + y + 2) + 2) >> 2
                        }
                    }
                    4 => {
                        if x > y {
                            (pa(x - y - 2) + 2 * pa(x - y - 1) + pa(x - y) + 2) >> 2
                        } else if x < y {
                            (pl(y - x - 2) + 2 * pl(y - x - 1) + pl(y - x) + 2) >> 2
                        } else {
                            (pa(0) + 2 * pa(-1) + pl(0) + 2) >> 2
                        }
                    }
                    5 => {
                        let zvr = 2 * x - y;
                        if zvr >= 0 && zvr % 2 == 0 {
                            let i = x - (y >> 1);
                            (pa(i - 1) + pa(i) + 1) >> 1
                        } else if zvr >= 0 {
                            let i = x - (y >> 1);
                            (pa(i - 2) + 2 * pa(i - 1) + pa(i) + 2) >> 2
                        } else if zvr == -1 {
                            (pa(0) + 2 * pa(-1) + pl(0) + 2) >> 2
                        } else {
                            // zVR < -1: use left neighbors
                            // For zVR=-3 (y=3,x=0): pl(0) + 2*pl(1) + pl(2)
                            let iy = (-1 - zvr) >> 1;
                            (pl(iy - 1) + 2 * pl(iy) + pl(iy + 1) + 2) >> 2
                        }
                    }
                    6 => {
                        let zhd = 2 * y - x;
                        if zhd >= 0 && zhd % 2 == 0 {
                            let i = y - (x >> 1);
                            (pl(i - 1) + pl(i) + 1) >> 1
                        } else if zhd >= 0 {
                            let i = y - (x >> 1);
                            (pl(i - 2) + 2 * pl(i - 1) + pl(i) + 2) >> 2
                        } else if zhd == -1 {
                            (pl(0) + 2 * pl(-1) + pa(0) + 2) >> 2
                        } else {
                            // zHD < -1: use above neighbors
                            let ix = (-1 - zhd) >> 1;
                            (pa(ix - 1) + 2 * pa(ix) + pa(ix + 1) + 2) >> 2
                        }
                    }
                    7 => {
                        let i = x + (y >> 1);
                        if y % 2 == 0 {
                            (pa(i) + pa(i + 1) + 1) >> 1
                        } else {
                            (pa(i) + 2 * pa(i + 1) + pa(i + 2) + 2) >> 2
                        }
                    }
                    8 => {
                        let zhu = x + 2 * y;
                        if zhu < 5 {
                            let i = y + (x >> 1);
                            if zhu % 2 == 0 {
                                (pl(i) + pl(i + 1) + 1) >> 1
                            } else {
                                (pl(i) + 2 * pl(i + 1) + pl(i + 2) + 2) >> 2
                            }
                        } else if zhu == 5 {
                            (pl(2) + 3 * pl(3) + 2) >> 2
                        } else {
                            pl(3)
                        }
                    }
                    _ => 128,
                };
                out[(y * 4 + x) as usize] = v.clamp(0, 255) as u16;
            }
        }
        out
    }

    #[test]
    fn test_all_directional_modes_against_spec() {
        let above: [u16; 8] = [10, 30, 50, 70, 90, 110, 130, 150];
        let left: [u16; 4] = [20, 60, 100, 140];
        let above_left: u16 = 40;

        for mode in [3u8, 4, 5, 6, 7, 8] {
            let expected = spec_predict_4x4(mode, &above, &left, above_left);
            let mut actual = [0u16; 16];
            predict_intra_4x4(
                mode,
                Some(&above[..]),
                Some(&left[..]),
                Some(above_left),
                &mut actual,
                8,
            );
            assert_eq!(
                actual, expected,
                "Mode {} mismatch.\n  actual:   {:?}\n  expected: {:?}",
                mode, actual, expected
            );
        }
    }

    #[test]
    fn test_directional_modes_uniform_input() {
        let above = [128u16; 8];
        let left = [128u16; 4];
        for mode in 0..9u8 {
            let mut output = [0u16; 16];
            predict_intra_4x4(
                mode,
                Some(&above[..]),
                Some(&left[..]),
                Some(128),
                &mut output,
                8,
            );
            assert!(
                output.iter().all(|&v| v == 128),
                "Mode {} should give all 128 for uniform input, got {:?}",
                mode,
                output
            );
        }
    }

    #[test]
    fn test_high_bit_depth_defaults() {
        // With no neighbours the DC default is 1 << (BitDepth - 1) (spec 8.3.1.2.3).
        let mut out4 = [0u16; 16];
        predict_intra_4x4(2, None, None, None, &mut out4, 10);
        assert!(out4.iter().all(|&v| v == 512));
        let mut out16 = [0u16; 256];
        predict_intra_16x16(2, None, None, None, &mut out16, 14);
        assert!(out16.iter().all(|&v| v == 8192));
        let mut outc = [0u16; 64];
        predict_chroma_8x8(0, None, None, None, &mut outc, 9);
        assert!(outc.iter().all(|&v| v == 256));
        let mut out8 = [0u16; 64];
        predict_intra_8x8(2, None, None, None, false, &mut out8, 10);
        assert!(out8.iter().all(|&v| v == 512));
    }

    #[test]
    fn test_plane_clips_to_bit_depth() {
        // A steep 10-bit gradient must clip to 1023, not 255.
        let above: Vec<u16> = (0..16).map(|i| (i * 64) as u16).collect();
        let left: Vec<u16> = (0..16).map(|i| (i * 64) as u16).collect();
        let mut out = [0u16; 256];
        predict_intra_16x16(3, Some(&above[..]), Some(&left[..]), Some(0), &mut out, 10);
        assert!(out.iter().all(|&v| v <= 1023));
        assert!(out.iter().any(|&v| v > 255));
    }
}
