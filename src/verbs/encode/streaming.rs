//! Hand-rolled STREAMING encoders for the formats where the `image` crate would need a
//! full converted copy of the image in memory first: OpenEXR, Radiance HDR, farbfeld,
//! PAM and PPM.
//!
//! Each writes straight to the output in one pass, pulling pixels through the
//! [`super::samplers`] helpers, so peak memory is a scanline (or one EXR compression
//! tile) rather than a second full frame. The RGBE path additionally has to escape a
//! pixel that would otherwise look like Radiance's RLE marker.

use super::*;

pub(super) fn encode_exr_bounded<W: Write + Seek>(
    writer: &mut W,
    img: &DynamicImage,
) -> exr::error::UnitResult {
    use exr::prelude::{Encoding, Image, SpecificChannels, Vec2, WritableImage};

    let channels = SpecificChannels::rgba(|position: Vec2<usize>| {
        let [r, g, b, a] = rgba_f32_at(img, position.x() as u32, position.y() as u32);
        (r, g, b, a)
    });
    let image = Image::from_encoded_channels(
        (img.width() as usize, img.height() as usize),
        Encoding::SMALL_FAST_LOSSLESS,
        channels,
    );

    // SMALL_FAST_LOSSLESS is PIZ over 256x256 tiles. Coupled with the sequential
    // writer, memory is bounded to one f32 compression tile instead of a second
    // width*height RGBA32F frame (or one tile per Rayon worker). Keeping f32
    // channels also preserves the source's full precision and values > f16::MAX.
    image.write().non_parallel().to_buffered(writer)
}

#[inline]
pub(super) fn float_rgb_to_rgbe([r, g, b]: [f32; 3]) -> [u8; 4] {
    // Largest finite value representable by normalized RGBE: the greatest f32
    // below 2^127. Larger finite values and +Inf saturate here; NaN, negatives,
    // and -Inf carry no radiance and become zero.
    const RGBE_MAX: f32 = f32::from_bits(0x7E_FF_FF_FF);
    let sanitize = |value: f32| {
        if value.is_nan() || value <= 0.0 {
            0.0
        } else if !value.is_finite() || value > RGBE_MAX {
            RGBE_MAX
        } else {
            value
        }
    };
    let [r, g, b] = [sanitize(r), sanitize(g), sanitize(b)];
    let maximum = r.max(g).max(b);
    if maximum <= 0.0 {
        return [0; 4];
    }

    // This intentionally matches image's Radiance encoder conversion.
    let exponent = maximum.log2().floor() as i32 + 1;
    // Exponent byte 0 denotes black. Values below the smallest normalized
    // Radiance value therefore underflow cleanly instead of wrapping a negative
    // exponent through `as u8`.
    if exponent < -127 {
        return [0; 4];
    }
    let exponent = exponent.clamp(-127, 127);
    let scale = 2.0_f32.powi(exponent);
    [
        (r / scale * 256.0).trunc() as u8,
        (g / scale * 256.0).trunc() as u8,
        (b / scale * 256.0).trunc() as u8,
        (exponent + 128) as u8,
    ]
}

#[inline]
pub(super) fn rgbe_at(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
    let [r, g, b, _] = rgba_f32_at(img, x, y);
    float_rgb_to_rgbe([r, g, b])
}

#[inline]
pub(super) fn escape_raw_rgbe_marker(mut pixel: [u8; 4], first_in_scanline: bool) -> [u8; 4] {
    // The legacy/raw decoder does not have an escape byte. Any literal
    // [1,1,1,E] is interpreted as "repeat the previous pixel E times" (and is
    // illegal as the first pixel), so perturb one 8-bit mantissa by one quantum.
    if pixel[..3] == [1, 1, 1] {
        pixel[2] = 2;
    }
    // The first pixel also selects the codec. [2,2,B<128,E] means new
    // per-component RLE, not a literal pixel.
    if first_in_scanline && pixel[0] == 2 && pixel[1] == 2 && pixel[2] < 128 {
        pixel[1] = 3;
    }
    pixel
}

pub(super) fn write_hdr_component_rle<W: Write>(
    writer: &mut W,
    scanline: &[[u8; 4]],
    component: usize,
) -> std::io::Result<()> {
    const MAX_RUN: usize = 127;
    const MAX_LITERAL: usize = 128;

    let mut index = 0;
    let mut literal = [0u8; MAX_LITERAL];
    while index < scanline.len() {
        let value = scanline[index][component];
        let run = scanline[index..]
            .iter()
            .take(MAX_RUN)
            .take_while(|pixel| pixel[component] == value)
            .count();
        if run >= 3 {
            writer.write_all(&[128 + run as u8, value])?;
            index += run;
            continue;
        }

        let mut literal_len = 0;
        while index < scanline.len() && literal_len < MAX_LITERAL {
            let value = scanline[index][component];
            let run = scanline[index..]
                .iter()
                .take(MAX_RUN)
                .take_while(|pixel| pixel[component] == value)
                .count();
            if run >= 3 {
                break;
            }

            let take = run.min(MAX_LITERAL - literal_len);
            for pixel in &scanline[index..index + take] {
                literal[literal_len] = pixel[component];
                literal_len += 1;
            }
            index += take;
        }

        debug_assert!(literal_len > 0);
        writer.write_all(&[literal_len as u8])?;
        writer.write_all(&literal[..literal_len])?;
    }
    Ok(())
}

pub(super) fn encode_hdr_bounded<W: Write>(
    writer: &mut W,
    img: &DynamicImage,
) -> std::io::Result<()> {
    let width = img.width() as usize;
    let height = img.height() as usize;
    writer.write_all(b"#?RADIANCE\n")?;
    writer.write_all(b"# Rust HDR encoder\n")?;
    writer.write_all(b"FORMAT=32-bit_rle_rgbe\n\n")?;
    writeln!(writer, "-Y {height} +X {width}")?;

    if !(8..=32_767).contains(&width) {
        // Radiance's new component-RLE marker cannot represent these widths.
        // Old readers accept a raw row-major RGBE stream after the same header.
        for y in 0..img.height() {
            for x in 0..img.width() {
                let pixel = escape_raw_rgbe_marker(rgbe_at(img, x, y), x == 0);
                writer.write_all(&pixel)?;
            }
        }
        return Ok(());
    }

    // The new RLE format stores one scanline, then compresses its R/G/B/E
    // components separately. Its format-level width ceiling makes this at most
    // 128 KiB regardless of the image height.
    let mut scanline = vec![[0u8; 4]; width];
    let marker = [2, 2, (width / 256) as u8, (width % 256) as u8];
    for y in 0..img.height() {
        for (x, pixel) in scanline.iter_mut().enumerate() {
            *pixel = rgbe_at(img, x as u32, y);
        }
        writer.write_all(&marker)?;
        for component in 0..4 {
            write_hdr_component_rle(writer, &scanline, component)?;
        }
    }
    Ok(())
}

pub(super) fn encode_farbfeld_streaming<W: Write>(
    writer: &mut W,
    img: &DynamicImage,
) -> std::io::Result<()> {
    writer.write_all(b"farbfeld")?;
    writer.write_all(&img.width().to_be_bytes())?;
    writer.write_all(&img.height().to_be_bytes())?;
    for y in 0..img.height() {
        for x in 0..img.width() {
            let channels = rgba_u16_at(img, x, y);
            let mut bytes = [0u8; 8];
            for (slot, channel) in bytes.chunks_exact_mut(2).zip(channels) {
                slot.copy_from_slice(&channel.to_be_bytes());
            }
            writer.write_all(&bytes)?;
        }
    }
    Ok(())
}

pub(super) fn pam_layout(img: &DynamicImage) -> (usize, &'static str, bool) {
    match img {
        DynamicImage::ImageLuma8(_) => (1, "GRAYSCALE", false),
        DynamicImage::ImageLumaA8(_) => (2, "GRAYSCALE_ALPHA", false),
        DynamicImage::ImageRgb8(_) => (3, "RGB", false),
        DynamicImage::ImageRgba8(_) => (4, "RGB_ALPHA", false),
        DynamicImage::ImageLuma16(_) => (1, "GRAYSCALE", true),
        DynamicImage::ImageLumaA16(_) => (2, "GRAYSCALE_ALPHA", true),
        DynamicImage::ImageRgb16(_) => (3, "RGB", true),
        DynamicImage::ImageRgba16(_) => (4, "RGB_ALPHA", true),
        // PAM has no floating-point sample representation. Preserve the channel
        // model and map floats to clamped unsigned 16-bit samples.
        DynamicImage::ImageRgb32F(_) => (3, "RGB", true),
        DynamicImage::ImageRgba32F(_) => (4, "RGB_ALPHA", true),
        #[allow(unreachable_patterns)]
        _ => (4, "RGB_ALPHA", false),
    }
}

#[inline]
pub(super) fn pam_u16_at(img: &DynamicImage, x: u32, y: u32) -> [u16; 4] {
    #[allow(unreachable_patterns)]
    match img {
        DynamicImage::ImageLuma8(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            [u8_to_u16(l), 0, 0, 0]
        }
        DynamicImage::ImageLumaA8(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            [u8_to_u16(l), u8_to_u16(a), 0, 0]
        }
        DynamicImage::ImageRgb8(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), 0]
        }
        DynamicImage::ImageRgba8(pixels) => {
            let [r, g, b, a] = pixels.get_pixel(x, y).0;
            [u8_to_u16(r), u8_to_u16(g), u8_to_u16(b), u8_to_u16(a)]
        }
        DynamicImage::ImageLuma16(pixels) => {
            let [l] = pixels.get_pixel(x, y).0;
            [l, 0, 0, 0]
        }
        DynamicImage::ImageLumaA16(pixels) => {
            let [l, a] = pixels.get_pixel(x, y).0;
            [l, a, 0, 0]
        }
        DynamicImage::ImageRgb16(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [r, g, b, 0]
        }
        DynamicImage::ImageRgba16(pixels) => pixels.get_pixel(x, y).0,
        DynamicImage::ImageRgb32F(pixels) => {
            let [r, g, b] = pixels.get_pixel(x, y).0;
            [f32_to_u16(r), f32_to_u16(g), f32_to_u16(b), 0]
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

pub(super) fn encode_pam_streaming<W: Write>(
    writer: &mut W,
    img: &DynamicImage,
) -> std::io::Result<()> {
    let (depth, tuple_type, wide) = pam_layout(img);
    write_pam_header(writer, img, depth, tuple_type, wide)?;
    write_pam_pixels(writer, img, depth, wide)
}

fn write_pam_header<W: Write>(
    writer: &mut W,
    img: &DynamicImage,
    depth: usize,
    tuple_type: &str,
    wide: bool,
) -> std::io::Result<()> {
    writeln!(writer, "P7")?;
    writeln!(writer, "WIDTH {}", img.width())?;
    writeln!(writer, "HEIGHT {}", img.height())?;
    writeln!(writer, "DEPTH {depth}")?;
    writeln!(writer, "MAXVAL {}", if wide { 65_535 } else { 255 })?;
    writeln!(writer, "TUPLTYPE {tuple_type}")?;
    writeln!(writer, "ENDHDR")
}

fn write_pam_pixels<W: Write>(
    writer: &mut W,
    img: &DynamicImage,
    depth: usize,
    wide: bool,
) -> std::io::Result<()> {
    for y in 0..img.height() {
        for x in 0..img.width() {
            let samples = pam_u16_at(img, x, y);
            for sample in &samples[..depth] {
                if wide {
                    writer.write_all(&sample.to_be_bytes())?;
                } else {
                    writer.write_all(&[u16_to_u8(*sample)])?;
                }
            }
        }
    }
    Ok(())
}

pub(super) fn encode_ppm_streaming<W: Write>(
    writer: &mut W,
    img: &DynamicImage,
) -> std::io::Result<()> {
    // PPM is always RGB and therefore drops alpha, but it can retain 16-bit
    // integer precision. Float inputs are clamped into that same 0..65535 range.
    let wide = matches!(
        img,
        DynamicImage::ImageLuma16(_)
            | DynamicImage::ImageLumaA16(_)
            | DynamicImage::ImageRgb16(_)
            | DynamicImage::ImageRgba16(_)
            | DynamicImage::ImageRgb32F(_)
            | DynamicImage::ImageRgba32F(_)
    );
    writeln!(writer, "P6")?;
    writeln!(writer, "{} {}", img.width(), img.height())?;
    writeln!(writer, "{}", if wide { 65_535 } else { 255 })?;
    for y in 0..img.height() {
        for x in 0..img.width() {
            if wide {
                let [r, g, b, _] = rgba_u16_at(img, x, y);
                writer.write_all(&r.to_be_bytes())?;
                writer.write_all(&g.to_be_bytes())?;
                writer.write_all(&b.to_be_bytes())?;
            } else {
                let [r, g, b, _] = rgba_u8_at(img, x, y);
                writer.write_all(&[r, g, b])?;
            }
        }
    }
    Ok(())
}
