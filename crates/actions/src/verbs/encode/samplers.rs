//! Per-pixel sampling helpers: read pixel `(x, y)` out of ANY `DynamicImage` variant as
//! f32 / u16 / u8 RGBA, plus the numeric conversions between those depths.
//!
//! They exist so the streaming encoders never have to materialize a converted copy of
//! the whole image: each one walks the source a pixel at a time in the depth it writes.
//! The conversions are deliberately explicit rather than `as` casts - rounding and
//! saturation both matter when a 16-bit source is written as 8-bit and vice versa.

use super::*;

#[inline]
pub(super) fn u8_to_f32(value: u8) -> f32 {
    value as f32 / u8::MAX as f32
}

#[inline]
pub(super) fn u16_to_f32(value: u16) -> f32 {
    value as f32 / u16::MAX as f32
}

#[inline]
pub(super) fn u8_to_u16(value: u8) -> u16 {
    u16::from(value) * 257
}

#[inline]
pub(super) fn u16_to_u8(value: u16) -> u8 {
    ((u32::from(value) + 128) / 257) as u8
}

#[inline]
pub(super) fn f32_to_u8(value: f32) -> u8 {
    let normalized = if value.is_nan() || value >= 1.0 {
        1.0
    } else {
        value.max(0.0)
    };
    (normalized * u8::MAX as f32).round() as u8
}

#[inline]
pub(super) fn f32_to_u16(value: f32) -> u16 {
    let normalized = if value.is_nan() || value >= 1.0 {
        1.0
    } else {
        value.max(0.0)
    };
    (normalized * u16::MAX as f32).round() as u16
}

/// Sample one pixel without materializing a converted full-frame image.
///
/// Matching the concrete variants is load-bearing for HDR inputs:
/// `DynamicImage::get_pixel` returns RGBA8 and would clamp an RGB32F/RGBA32F
/// source to SDR before EXR/HDR encoding.
#[inline]
pub(super) fn rgba_f32_at(img: &DynamicImage, x: u32, y: u32) -> [f32; 4] {
    #[allow(unreachable_patterns)]
    match img {
        DynamicImage::ImageLuma8(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            let l = u8_to_f32(l);
            [l, l, l, 1.0]
        }
        DynamicImage::ImageLumaA8(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            let l = u8_to_f32(l);
            [l, l, l, u8_to_f32(a)]
        }
        DynamicImage::ImageRgb8(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u8_to_f32(r), u8_to_f32(g), u8_to_f32(b), 1.0]
        }
        DynamicImage::ImageRgba8(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u8_to_f32(r), u8_to_f32(g), u8_to_f32(b), u8_to_f32(a)]
        }
        DynamicImage::ImageLuma16(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            let l = u16_to_f32(l);
            [l, l, l, 1.0]
        }
        DynamicImage::ImageLumaA16(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            let l = u16_to_f32(l);
            [l, l, l, u16_to_f32(a)]
        }
        DynamicImage::ImageRgb16(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u16_to_f32(r), u16_to_f32(g), u16_to_f32(b), 1.0]
        }
        DynamicImage::ImageRgba16(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u16_to_f32(r), u16_to_f32(g), u16_to_f32(b), u16_to_f32(a)]
        }
        DynamicImage::ImageRgb32F(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [r, g, b, 1.0]
        }
        DynamicImage::ImageRgba32F(pixels) => pixels.get_pixel(x, y).0,
        _ => {
            let [r, g, b, a] = image::GenericImageView::get_pixel(img, x, y).0;
            [u8_to_f32(r), u8_to_f32(g), u8_to_f32(b), u8_to_f32(a)]
        }
    }
}

#[inline]
pub(super) fn rgba_u16_at(img: &DynamicImage, x: u32, y: u32) -> [u16; 4] {
    #[allow(unreachable_patterns)]
    match img {
        DynamicImage::ImageLuma8(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            let l = u8_to_u16(l);
            [l, l, l, u16::MAX]
        }
        DynamicImage::ImageLumaA8(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            let l = u8_to_u16(l);
            [l, l, l, u8_to_u16(a)]
        }
        DynamicImage::ImageRgb8(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u16::MAX]
        }
        DynamicImage::ImageRgba8(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u8_to_u16(a)]
        }
        DynamicImage::ImageLuma16(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            [l, l, l, u16::MAX]
        }
        DynamicImage::ImageLumaA16(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            [l, l, l, a]
        }
        DynamicImage::ImageRgb16(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [r, g, b, u16::MAX]
        }
        DynamicImage::ImageRgba16(pixels) => pixels.get_pixel(x, y).0,
        DynamicImage::ImageRgb32F(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [f32_to_u16(r), f32_to_u16(g), f32_to_u16(b), u16::MAX]
        }
        DynamicImage::ImageRgba32F(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [f32_to_u16(r), f32_to_u16(g), f32_to_u16(b), f32_to_u16(a)]
        }
        _ => {
            let [r, g, b, a] = image::GenericImageView::get_pixel(img, x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u8_to_u16(a)]
        }
    }
}

#[inline]
pub(super) fn rgba_u8_at(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
    #[allow(unreachable_patterns)]
    match img {
        DynamicImage::ImageLuma8(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            [l, l, l, u8::MAX]
        }
        DynamicImage::ImageLumaA8(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            [l, l, l, a]
        }
        DynamicImage::ImageRgb8(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [r, g, b, u8::MAX]
        }
        DynamicImage::ImageRgba8(pixels) => pixels.get_pixel(x, y).0,
        DynamicImage::ImageLuma16(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            let l = u16_to_u8(l);
            [l, l, l, u8::MAX]
        }
        DynamicImage::ImageLumaA16(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            let l = u16_to_u8(l);
            [l, l, l, u16_to_u8(a)]
        }
        DynamicImage::ImageRgb16(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u16_to_u8(r), u16_to_u8(g), u16_to_u8(b), u8::MAX]
        }
        DynamicImage::ImageRgba16(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u16_to_u8(r), u16_to_u8(g), u16_to_u8(b), u16_to_u8(a)]
        }
        DynamicImage::ImageRgb32F(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [f32_to_u8(r), f32_to_u8(g), f32_to_u8(b), u8::MAX]
        }
        DynamicImage::ImageRgba32F(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [f32_to_u8(r), f32_to_u8(g), f32_to_u8(b), f32_to_u8(a)]
        }
        _ => image::GenericImageView::get_pixel(img, x, y).0,
    }
}
