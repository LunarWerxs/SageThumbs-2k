//! Shared helpers for the DjVuLibre interop example harnesses.
//!
//! `interop_pixdiff` (round-5 #14) and `diff_fuzz` (DIFF_FUZZ) both need to
//! parse a `ddjvu -format=ppm` PPM and diff it against one of our own
//! [`djvu_rs::Pixmap`]s. This module holds that single implementation so the
//! two binaries do not carry copy-pasted diff logic that could silently drift
//! apart (e.g. one gets a bucket-threshold tweak the other doesn't).
//!
//! Not every consumer uses every helper, hence `#![allow(dead_code)]`: each
//! example pulls this file in via `#[path = "support/mod.rs"] mod support;`,
//! so it is compiled once per binary and unused items would otherwise warn.
#![allow(dead_code)]
#![allow(deprecated)] // `RenderOptions::scale` — retained-for-compat, see the field's own doc comment

use djvu_rs::djvu_render::{RenderOptions, Resampling, UserRotation};

/// Render options for a native-resolution, faithful (non-permissive) render —
/// the configuration both interop harnesses compare against `ddjvu`'s default
/// output.
pub fn native_opts(w: u32, h: u32) -> RenderOptions {
    RenderOptions {
        width: w,
        height: h,
        scale: 1.0,
        bold: 0,
        aa: false,
        rotation: UserRotation::None,
        permissive: false,
        resampling: Resampling::Bilinear,
        mask_aa: false,
    }
}

/// Parse a binary PPM (P6). Returns `(width, height, rgb_bytes)`.
pub fn parse_ppm(data: &[u8]) -> Option<(usize, usize, Vec<u8>)> {
    if data.get(0..2)? != b"P6" {
        return None;
    }
    let mut pos = 2usize;
    // Read three ASCII integers (width, height, maxval), skipping whitespace and
    // '#' comment lines.
    let mut nums = [0usize; 3];
    for slot in nums.iter_mut() {
        loop {
            let b = *data.get(pos)?;
            if b == b'#' {
                while *data.get(pos)? != b'\n' {
                    pos += 1;
                }
            } else if b.is_ascii_whitespace() {
                pos += 1;
            } else {
                break;
            }
        }
        let mut v = 0usize;
        while let Some(&b) = data.get(pos) {
            if b.is_ascii_digit() {
                v = v * 10 + (b - b'0') as usize;
                pos += 1;
            } else {
                break;
            }
        }
        *slot = v;
    }
    // Exactly one whitespace byte separates the header from the pixel data.
    pos += 1;
    let (w, h, maxval) = (nums[0], nums[1], nums[2]);
    if maxval != 255 {
        return None;
    }
    let need = w * h * 3;
    let rgb = data.get(pos..pos + need)?.to_vec();
    Some((w, h, rgb))
}

/// Per-pixel, per-channel absolute difference distribution between an RGBA
/// pixmap (ours) and an RGB reference buffer (ddjvu's PPM).
pub struct DiffStats {
    pub w: usize,
    pub h: usize,
    pub mean_abs: f64,
    pub max_abs: u8,
    pub p50: u8,
    pub p95: u8,
    pub p99: u8,
    pub pct_gt2: f64,
    pub pct_gt8: f64,
    pub pct_gt32: f64,
}

pub fn diff_stats(w: usize, h: usize, ours_rgba: &[u8], ref_rgb: &[u8]) -> DiffStats {
    let mut hist = [0u64; 256];
    let mut sum: u64 = 0;
    let mut max_abs = 0u8;
    let n_channels = w * h * 3;
    for p in 0..(w * h) {
        for c in 0..3 {
            let a = ours_rgba[p * 4 + c];
            let b = ref_rgb[p * 3 + c];
            let d = a.abs_diff(b);
            hist[d as usize] += 1;
            sum += d as u64;
            if d > max_abs {
                max_abs = d;
            }
        }
    }
    let total = n_channels as u64;
    let pctile = |frac: f64| -> u8 {
        let target = (frac * total as f64) as u64;
        let mut acc = 0u64;
        for (d, &count) in hist.iter().enumerate() {
            acc += count;
            if acc >= target {
                return d as u8;
            }
        }
        255
    };
    let count_gt = |t: usize| -> f64 {
        let over: u64 = hist[(t + 1)..].iter().sum();
        100.0 * over as f64 / total as f64
    };
    DiffStats {
        w,
        h,
        mean_abs: sum as f64 / total as f64,
        max_abs,
        p50: pctile(0.50),
        p95: pctile(0.95),
        p99: pctile(0.99),
        pct_gt2: count_gt(2),
        pct_gt8: count_gt(8),
        pct_gt32: count_gt(32),
    }
}
