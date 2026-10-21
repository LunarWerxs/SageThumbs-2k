//! The HDR capture's tone curve (issue #67): one HDR monitor's scRGB down to the 8-bit sRGB
//! picture the editor shows, at that monitor's SDR white level.
//!
//! Pure functions, apart from the Win32 capture in `hdr.rs` that feeds them, so this module
//! builds without the `hdr-capture` feature and its tests run in CI's default-feature suite.
#![cfg_attr(
    not(feature = "hdr-capture"),
    allow(dead_code, reason = "called only by the hdr-capture feature's hdr.rs")
)]

use super::hdrsave::{half_to_f32, SCRGB_NITS};

/// The peak assumed for a monitor whose EDID reports none (DXGI then says 0): a common
/// HDR panel's, so highlights still roll off instead of all landing on white.
const FALLBACK_PEAK_NITS: f32 = 1000.0;

/// One HDR monitor's display levels, in scRGB units (`1.0` = 80 nits).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Levels {
    /// Where SDR white sits: the user's "SDR content brightness".
    pub(super) white: f32,
    /// The brightest the panel shows (`IDXGIOutput6::GetDesc1().MaxLuminance`).
    pub(super) peak: f32,
}

impl Levels {
    /// From Windows' raw `SDRWhiteLevel` (1000 = 80 nits; `None` when Windows would not say,
    /// which keeps the old 80-nit assumption) and DXGI's `MaxLuminance` in nits.
    pub(super) fn new(sdr_white_level: Option<u32>, max_luminance_nits: f32) -> Levels {
        let white = sdr_white_level
            .filter(|&v| v > 0)
            .map_or(1.0, |v| v as f32 / 1000.0);
        let peak_nits = if max_luminance_nits.is_finite() && max_luminance_nits > 0.0 {
            max_luminance_nits
        } else {
            FALLBACK_PEAK_NITS
        };
        Levels {
            white,
            peak: peak_nits / SCRGB_NITS,
        }
    }
}

/// Is `a` the same GDI device name as `b` (`\\.\DISPLAY1`)? Compared up to each one's NUL,
/// because nothing promises what follows it in the fixed 32-character buffers.
pub(super) fn same_device(a: &[u16; 32], b: &[u16; 32]) -> bool {
    let name = |s: &[u16; 32]| -> Vec<u16> { s.iter().copied().take_while(|&c| c != 0).collect() };
    let (a, b) = (name(a), name(b));
    !a.is_empty() && a == b
}

/// scRGB half-float rows -> top-down 8-bit BGRA at `levels`, plus the RGB halves themselves
/// (alpha dropped, row pitch removed) for the HDR save formats.
///
/// # Safety
/// `data` must point to at least `row_pitch * h` readable bytes.
pub(super) unsafe fn tone_map(
    data: *const u8,
    row_pitch: usize,
    w: usize,
    h: usize,
    levels: Levels,
) -> (Vec<u8>, Vec<u16>) {
    let mut bgra = vec![0u8; w * h * 4];
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let row = data.add(y * row_pitch) as *const u16;
        for x in 0..w {
            let px = [0, 1, 2].map(|c| row.add(x * 4 + c).read_unaligned());
            rgb.extend_from_slice(&px);
            let [r, g, b] = map_pixel(px.map(half_to_f32), levels);
            let o = (y * w + x) * 4;
            // GDI's DIB layout is BGRA.
            bgra[o..o + 4].copy_from_slice(&[b, g, r, 255]);
        }
    }
    (bgra, rgb)
}

/// One linear scRGB pixel -> sRGB bytes.
///
/// Divided by the monitor's SDR white first, so desktop white is `1.0` and every SDR pixel
/// comes out as the sRGB byte it was composed from. Above SDR white there is nowhere left in
/// 8 bits to go, so a highlight keeps its hue (its channels scaled together by the brightest
/// one, never clipped one by one, which turns orange into yellow) and fades toward white as it
/// nears the monitor's peak, the way film handles highlights: brighter reads as whiter.
fn map_pixel(rgb: [f32; 3], levels: Levels) -> [u8; 3] {
    let c = rgb.map(|v| {
        if v.is_finite() && v > 0.0 {
            v / levels.white
        } else {
            0.0
        }
    });
    let m = c[0].max(c[1]).max(c[2]);
    if m <= 1.0 {
        return c.map(srgb_byte);
    }
    let fade = highlight_fade(m, levels.peak / levels.white);
    c.map(|v| {
        let hue = v / m;
        srgb_byte(hue + (1.0 - hue) * fade)
    })
}

/// How far a highlight whose brightest channel is `m` times SDR white has faded to white:
/// 0 at SDR white, 1 at the monitor's `peak` (also relative to SDR white), logarithmic in
/// between because brightness is perceived that way. A peak at or below SDR white leaves no
/// room, so every highlight is white.
fn highlight_fade(m: f32, peak: f32) -> f32 {
    if peak <= 1.0 {
        return 1.0;
    }
    (m.ln() / peak.ln()).clamp(0.0, 1.0)
}

/// Linear `0.0..=1.0` -> one sRGB byte.
fn srgb_byte(v: f32) -> u8 {
    let v = v.clamp(0.0, 1.0);
    let s = if v <= 0.003_130_8 {
        12.92 * v
    } else {
        1.055 * v.powf(1.0 / 2.4) - 0.055
    };
    (s * 255.0 + 0.5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The common SDR white levels: Windows' 80-nit default up to a 480-nit slider.
    const WHITE_LEVELS: [u32; 5] = [1000, 2500, 3500, 5000, 6000];

    fn levels(raw_white: u32) -> Levels {
        Levels::new(Some(raw_white), 1000.0)
    }

    /// sRGB byte -> linear `0.0..=1.0`.
    fn linear(byte: u8) -> f32 {
        let c = byte as f32 / 255.0;
        if c <= 0.040_45 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    }

    /// Issue #67: desktop white arrives at the user's SDR white level, not at 1.0, and must
    /// come out white, not clipped from far below it.
    #[test]
    fn sdr_white_maps_to_white_at_every_white_level() {
        for raw in WHITE_LEVELS {
            let lv = levels(raw);
            assert_eq!(
                map_pixel([lv.white; 3], lv),
                [255; 3],
                "SDRWhiteLevel {raw}"
            );
            assert!(
                map_pixel([lv.white * 0.95; 3], lv)[0] < 255,
                "just under white must not clip at SDRWhiteLevel {raw}"
            );
        }
    }

    /// Every SDR byte composed at the user's white level comes back as the same byte, so an
    /// HDR desktop's capture matches a BitBlt of an SDR one. Before #67, 0.18 scRGB (an sRGB
    /// mid-grey at 80 nits) came out near 0.31 on a 200-nit desktop.
    #[test]
    fn sdr_content_comes_back_byte_for_byte() {
        for raw in WHITE_LEVELS {
            let lv = levels(raw);
            for byte in [0u8, 1, 18, 64, 118, 128, 200, 254] {
                let scrgb = linear(byte) * lv.white;
                assert_eq!(
                    map_pixel([scrgb, scrgb * 0.5, scrgb * 0.25], lv)[0],
                    byte,
                    "byte {byte} at SDRWhiteLevel {raw}"
                );
            }
        }
    }

    /// Above SDR white a coloured highlight keeps its hue and fades toward white as it nears
    /// the peak: its brightest channel stays at 255, its others rise strictly and stay below
    /// 255 until the peak, and the channel order (the hue) never changes.
    #[test]
    fn highlights_stay_ordered_below_white_until_peak() {
        let lv = levels(2500); // 200-nit SDR white, 1000-nit peak
        let hue = [1.0, 0.5, 0.25];
        let mut last = map_pixel(hue.map(|c| c * lv.white), lv);
        let mut k = 1.1f32;
        while k * lv.white < lv.peak {
            let out = map_pixel(hue.map(|c| c * k * lv.white), lv);
            assert_eq!(out[0], 255, "the brightest channel is white at {k}x");
            assert!(
                out[1] >= last[1] && out[2] >= last[2],
                "brighter never darkens at {k}x"
            );
            assert!(
                out[1] < 255 && out[2] < 255,
                "still coloured below the peak at {k}x"
            );
            assert!(
                out[0] >= out[1] && out[1] >= out[2],
                "hue order kept at {k}x"
            );
            last = out;
            k *= 1.1;
        }
        assert!(
            last[2] > map_pixel(hue.map(|c| c * lv.white), lv)[2],
            "it did fade"
        );
        assert_eq!(
            map_pixel(hue.map(|c| c * lv.peak), lv),
            [255; 3],
            "white at the peak"
        );
    }

    /// Mid-tones stay ordered and the curve is continuous across SDR white.
    #[test]
    fn tone_curve_is_monotonic_through_sdr_white() {
        let lv = levels(3500);
        let mut last = 0u8;
        for i in 0..=400 {
            let v = i as f32 / 100.0 * lv.white;
            let out = map_pixel([v, v * 0.3, 0.0], lv)[1];
            assert!(out >= last, "green fell at {v}");
            last = out;
        }
    }

    /// No white level from Windows keeps the old 80-nit assumption; no peak from the EDID
    /// assumes a common panel's; a peak at or below SDR white turns every highlight white.
    #[test]
    fn levels_fall_back_sensibly() {
        assert_eq!(
            Levels::new(None, 0.0),
            Levels {
                white: 1.0,
                peak: FALLBACK_PEAK_NITS / SCRGB_NITS
            }
        );
        assert_eq!(Levels::new(Some(0), f32::NAN).white, 1.0);
        assert_eq!(Levels::new(Some(5000), 600.0).peak, 7.5);
        let dim = Levels::new(Some(6000), 400.0); // 480-nit white on a 400-nit panel
        assert_eq!(
            map_pixel([dim.white * 2.0, dim.white, 0.0], dim),
            [255, 255, 255]
        );
    }

    /// Garbage samples collapse to black, NOT to white. That matches
    /// `decode::tone_map_float`, which has always treated a non-finite or negative
    /// float sample as zero, so EXR/HDR decoding and HDR capture agree rather than
    /// each inventing their own rule.
    #[test]
    fn nonsense_input_is_clamped_not_propagated() {
        let lv = levels(2500);
        for v in [f32::NAN, -5.0, f32::NEG_INFINITY, f32::INFINITY] {
            assert_eq!(map_pixel([v; 3], lv), [0; 3], "{v}");
        }
        assert_eq!(map_pixel([f32::NAN, lv.white, -1.0], lv), [0, 255, 0]);
    }

    /// The DXGI and DisplayConfig names match up to their NULs, whatever follows.
    #[test]
    fn device_names_match_up_to_the_nul() {
        let name = |s: &str, junk: u16| -> [u16; 32] {
            let mut out = [junk; 32];
            for (i, c) in s.encode_utf16().chain([0]).enumerate() {
                out[i] = c;
            }
            out
        };
        assert!(same_device(
            &name(r"\\.\DISPLAY1", 0),
            &name(r"\\.\DISPLAY1", 7)
        ));
        assert!(!same_device(
            &name(r"\\.\DISPLAY1", 0),
            &name(r"\\.\DISPLAY12", 0)
        ));
        assert!(
            !same_device(&[0; 32], &[0; 32]),
            "two empty names are no match"
        );
    }

    /// A full row round-trip through the same layout the GPU hands us, including
    /// a RowPitch that is wider than the visible width (which it usually is).
    #[test]
    fn tone_map_honours_row_pitch_and_writes_bgra() {
        let (w, h) = (2usize, 2usize);
        let pitch = 32usize; // deliberately > w * 4 * 2 bytes
        let mut src = vec![0u8; pitch * h];
        // Pixel (0,0) = pure red at SDR white; (1,0) = pure blue.
        let put = |buf: &mut [u8], x: usize, y: usize, rgba: [u16; 4]| {
            let base = y * pitch + x * 8;
            for (i, v) in rgba.iter().enumerate() {
                buf[base + i * 2..base + i * 2 + 2].copy_from_slice(&v.to_le_bytes());
            }
        };
        put(&mut src, 0, 0, [0x4100, 0, 0, 0x3C00]); // r=2.5: white on a 200-nit desktop
        put(&mut src, 1, 0, [0, 0, 0x4100, 0x3C00]); // blue at the same white

        let (out, rgb) = unsafe { tone_map(src.as_ptr(), pitch, w, h, levels(2500)) };
        assert_eq!(out.len(), w * h * 4);
        assert_eq!(&out[0..4], &[0, 0, 255, 255], "pixel 0 should be BGRA red");
        assert_eq!(&out[4..8], &[255, 0, 0, 255], "pixel 1 should be BGRA blue");
        assert!(
            out[8..].iter().step_by(4).all(|&b| b == 0),
            "row 1 stays black"
        );
        assert_eq!(
            rgb.len(),
            w * h * 3,
            "the float copy drops alpha and the pitch"
        );
        assert_eq!(&rgb[0..6], &[0x4100, 0, 0, 0, 0, 0x4100]);
    }
}
