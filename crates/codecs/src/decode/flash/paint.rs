//! SWF paints: colour transforms, the shader for each fill style, and the bitmaps those
//! fills sample (JPEG with its shared tables, or the zlib-packed lossless formats).

use std::collections::HashMap;
use std::io::Read;

use swf::{CharacterId, Color, DefineBitsLossless, FillStyle, Gradient, GradientSpread};
use tiny_skia::{
    FilterQuality, GradientStop, LinearGradient, Paint, Pixmap, RadialGradient, Shader, SpreadMode,
};

use super::geom::Aff;
use crate::decode::limits;

/// A colour transform in `0..=1` units: `out = in * mul + add`, per channel r, g, b, a.
/// SWF stores `add` in `-255..=255`, which is the same scale divided by 255.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Cx {
    mul: [f32; 4],
    add: [f32; 4],
}

impl Cx {
    pub const IDENTITY: Cx = Cx {
        mul: [1.0; 4],
        add: [0.0; 4],
    };

    pub fn from_swf(ct: Option<&swf::ColorTransform>) -> Cx {
        match ct {
            None => Cx::IDENTITY,
            Some(ct) => Cx {
                mul: [
                    ct.r_multiply.to_f32(),
                    ct.g_multiply.to_f32(),
                    ct.b_multiply.to_f32(),
                    ct.a_multiply.to_f32(),
                ],
                add: [
                    ct.r_add as f32 / 255.0,
                    ct.g_add as f32 / 255.0,
                    ct.b_add as f32 / 255.0,
                    ct.a_add as f32 / 255.0,
                ],
            },
        }
    }

    /// `self` applied first, then `parent`: a child inside a transformed parent.
    pub fn then(self, parent: Cx) -> Cx {
        let mut out = Cx::IDENTITY;
        for i in 0..4 {
            out.mul[i] = self.mul[i] * parent.mul[i];
            out.add[i] = self.add[i] * parent.mul[i] + parent.add[i];
        }
        out
    }

    pub fn color(self, c: Color) -> tiny_skia::Color {
        let v = [c.r, c.g, c.b, c.a].map(|x| x as f32 / 255.0);
        let mut o = [0.0f32; 4];
        for i in 0..4 {
            o[i] = (v[i] * self.mul[i] + self.add[i]).clamp(0.0, 1.0);
        }
        tiny_skia::Color::from_rgba(o[0], o[1], o[2], o[3]).unwrap_or(tiny_skia::Color::BLACK)
    }
}

/// The paint for one fill style at `aff` (shape units to pixels). `None` when the style
/// cannot be drawn: a bitmap that did not decode, or a gradient with no stops. Such a fill
/// is left out rather than drawn wrong.
pub(super) fn fill_paint<'a>(
    style: &'a FillStyle,
    aff: Aff,
    cx: Cx,
    bitmaps: &'a HashMap<CharacterId, Option<Pixmap>>,
) -> Option<Paint<'a>> {
    let shader: Shader<'a> = match style {
        FillStyle::Color(c) => Shader::SolidColor(cx.color(*c)),
        FillStyle::LinearGradient(g) => gradient(g, aff, cx, None)?,
        FillStyle::RadialGradient(g) => gradient(g, aff, cx, Some(0.0))?,
        FillStyle::FocalGradient {
            gradient: g,
            focal_point,
        } => gradient(g, aff, cx, Some(focal_point.to_f64()))?,
        FillStyle::Bitmap {
            id,
            matrix,
            is_smoothed,
            is_repeating,
        } => bitmap_shader(
            bitmaps.get(id)?.as_ref()?,
            matrix,
            aff,
            *is_smoothed,
            *is_repeating,
        ),
    };
    Some(Paint {
        shader,
        anti_alias: true,
        ..Paint::default()
    })
}

/// A bitmap fill. Its matrix maps the picture's pixels to shape twips itself: a picture shown
/// 1:1 carries a scale of 20. (An extra x20 here drew the reporter's photos 20 times too big,
/// a smear of their top-left corner.)
pub(super) fn bitmap_shader<'a>(
    pixmap: &'a Pixmap,
    matrix: &swf::Matrix,
    aff: Aff,
    smooth: bool,
    repeat: bool,
) -> Shader<'a> {
    let t = aff.then(Aff::from_swf(matrix)).to_transform();
    let spread = if repeat {
        SpreadMode::Repeat
    } else {
        SpreadMode::Pad
    };
    let quality = if smooth {
        FilterQuality::Bilinear
    } else {
        FilterQuality::Nearest
    };
    tiny_skia::Pattern::new(pixmap.as_ref(), spread, quality, 1.0, t)
}

/// A linear gradient runs across the unit square `-1..=1` in gradient space, which SWF
/// scales by 16384 twips; a radial one is a circle of radius 1 about the origin. `focal`
/// moves a radial gradient's focal point along x (the focal form of the same gradient).
fn gradient(g: &Gradient, aff: Aff, cx: Cx, focal: Option<f64>) -> Option<Shader<'static>> {
    let mut records: Vec<&swf::GradientRecord> = g.records.iter().collect();
    records.sort_by_key(|r| r.ratio);
    if records.is_empty() {
        return None;
    }
    let stops: Vec<GradientStop> = records
        .iter()
        .map(|r| GradientStop::new(r.ratio as f32 / 255.0, cx.color(r.color)))
        .collect();
    let mode = match g.spread {
        GradientSpread::Pad => SpreadMode::Pad,
        GradientSpread::Reflect => SpreadMode::Reflect,
        GradientSpread::Repeat => SpreadMode::Repeat,
    };
    let t = aff
        .then(Aff::from_swf(&g.matrix))
        .then(Aff::scale(16384.0))
        .to_transform();
    let origin = tiny_skia::Point::from_xy(0.0, 0.0);
    match focal {
        None => LinearGradient::new(
            tiny_skia::Point::from_xy(-1.0, 0.0),
            tiny_skia::Point::from_xy(1.0, 0.0),
            stops,
            mode,
            t,
        ),
        Some(f) => RadialGradient::new(
            tiny_skia::Point::from_xy(f as f32, 0.0),
            0.0,
            origin,
            1.0,
            stops,
            mode,
            t,
        ),
    }
}

/// A solid, opaque black paint: what a clip shape draws into its mask.
pub(super) fn black() -> Paint<'static> {
    Paint {
        shader: Shader::SolidColor(tiny_skia::Color::BLACK),
        anti_alias: true,
        ..Paint::default()
    }
}

/// Zlib-inflate `data`, refusing to produce more than `cap` bytes. The swf crate would
/// inflate a header-declared length without bound, so the packed bitmap goes through here.
pub(super) fn inflate_capped(data: &[u8], cap: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut limited = flate2::read::ZlibDecoder::new(data).take(cap as u64 + 1);
    limited.read_to_end(&mut out).ok()?;
    (out.len() <= cap).then_some(out)
}

/// The largest bitmap this tier will decode. Past it a fill is drawn without its picture.
pub(super) const MAX_BITMAP_PIXELS: u64 = 4096 * 4096;

/// A DefineBits* character: a JPEG (plus its alpha, for JPEG3) or a lossless pixel block.
/// `shared_tables` marks an old-style DefineBits, whose tables are the file's JPEGTables; a
/// JPEG2 or JPEG3 carries its own.
#[derive(Clone, Copy, Debug)]
pub(super) enum Bits<'a> {
    Jpeg {
        data: &'a [u8],
        alpha: Option<&'a [u8]>,
        shared_tables: bool,
    },
    Lossless(&'a DefineBitsLossless<'a>),
}

/// Decodes one bitmap character to premultiplied pixels. `jpeg_tables` is the file's shared
/// `JPEGTables`, which an old-style JPEG leaves out. Returns `None` on anything it cannot
/// decode or that is over budget.
pub(super) fn decode_bits(bits: Bits<'_>, jpeg_tables: Option<&[u8]>) -> Option<Pixmap> {
    match bits {
        Bits::Jpeg {
            data,
            alpha,
            shared_tables,
        } => decode_jpeg(data, alpha, jpeg_tables.filter(|_| shared_tables)),
        Bits::Lossless(l) => decode_lossless(l),
    }
}

/// A JPEG tag's picture. Since SWF 8 a DefineBitsJPEG2/3 may hold a PNG or a GIF instead
/// (SWF19 pp.138-139), whose alpha is straight. A JPEG3's colours are stored already multiplied
/// by its alpha mask, so they are kept as they are, each clamped to its alpha the way Flash
/// Player draws a file that left them unmultiplied (Ruffle's `decode_jpeg` does the same).
fn decode_jpeg(data: &[u8], alpha: Option<&[u8]>, tables: Option<&[u8]>) -> Option<Pixmap> {
    if data.starts_with(b"\x89PNG") || data.starts_with(b"GIF8") {
        let rgba = decode_bounded_image(data)?;
        let (w, h) = rgba.dimensions();
        let mut buf = rgba.into_raw();
        premultiply(&mut buf);
        return pixmap(w, h, buf);
    }
    let bytes = match tables {
        Some(t) if !t.is_empty() => one_jpeg_stream(&[t, data]),
        _ => one_jpeg_stream(&[data]),
    };
    let rgba = decode_bounded_image(&bytes)?;
    let (w, h) = rgba.dimensions();
    let mut buf = rgba.into_raw();
    let pixels = (w as usize) * (h as usize);
    if let Some(alpha) = alpha
        .and_then(|a| inflate_capped(a, pixels))
        .filter(|a| a.len() == pixels)
    {
        for (px, al) in buf.as_chunks_mut::<4>().0.iter_mut().zip(alpha) {
            px[3] = al;
        }
        clamp_to_alpha(&mut buf);
    }
    pixmap(w, h, buf)
}

/// A JPEG as the one SOI..EOI stream a decoder takes. An SWF splits it in ways decoders refuse:
/// a DefineBits leaves its tables in the file's JPEGTables, a DefineBitsJPEG2 often holds its
/// tables and its picture as two streams (SOI tables EOI SOI picture EOI: the reporter's photos,
/// issue #70), and encoders before SWF 8 lead with an erased EOI SOI pair. So every part's
/// SOI and EOI markers are dropped and its segments kept in order, between one SOI and one EOI.
pub(super) fn one_jpeg_stream(parts: &[&[u8]]) -> Vec<u8> {
    let mut out = vec![0xFF, 0xD8];
    for part in parts {
        copy_segments(part, &mut out);
    }
    out.extend_from_slice(&[0xFF, 0xD9]);
    out
}

/// Appends `data`'s JPEG segments to `out`, leaving out its SOI and EOI markers. Stops at the
/// first byte that is not a marker or a segment that runs off the end (trailing padding).
fn copy_segments(data: &[u8], out: &mut Vec<u8>) {
    let mut i = 0;
    while data.get(i) == Some(&0xFF) {
        let Some(end) = segment_end(data, i) else {
            return;
        };
        if !matches!(data.get(i + 1), Some(0xD8 | 0xD9)) {
            out.extend_from_slice(&data[i..end]);
        }
        i = end;
    }
}

/// Where the segment whose marker starts at `data[i]` ends. A scan (SOS) runs on through its
/// entropy-coded data, which ends at the next marker that is neither a stuffed `FF 00` nor
/// a restart.
fn segment_end(data: &[u8], i: usize) -> Option<usize> {
    let marker = *data.get(i + 1)?;
    if matches!(marker, 0xFF) {
        return Some(i + 1); // a fill byte before the marker
    }
    if matches!(marker, 0xD8 | 0xD9 | 0x01 | 0xD0..=0xD7) {
        return Some(i + 2); // markers with no length
    }
    let len = u16::from_be_bytes([*data.get(i + 2)?, *data.get(i + 3)?]) as usize;
    let end = i + 2 + len;
    if end > data.len() || len < 2 {
        return None;
    }
    if marker != 0xDA {
        return Some(end);
    }
    let coded = data[end..]
        .windows(2)
        .position(|w| w[0] == 0xFF && w[1] != 0 && !(0xD0..=0xD7).contains(&w[1]));
    Some(coded.map_or(data.len(), |k| end + k))
}

/// Decodes a picture the `image` crate can read, after checking its declared size against
/// the bitmap budget, so an oversized header is refused before any pixel is produced.
fn decode_bounded_image(bytes: &[u8]) -> Option<image::RgbaImage> {
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    let (w, h) = reader.into_dimensions().ok()?;
    if u64::from(w) * u64::from(h) > MAX_BITMAP_PIXELS.min(limits::MAX_PIXELS) {
        return None;
    }
    let reader = image::ImageReader::new(std::io::Cursor::new(bytes))
        .with_guessed_format()
        .ok()?;
    Some(reader.decode().ok()?.to_rgba8())
}

fn decode_lossless(l: &DefineBitsLossless<'_>) -> Option<Pixmap> {
    let (w, h) = (l.width as usize, l.height as usize);
    let pixels = w.checked_mul(h)?;
    if w == 0 || h == 0 || (pixels as u64) > MAX_BITMAP_PIXELS {
        return None;
    }
    // Each format to RGBA8. Version 1 has no alpha channel, so it is opaque; version 2's
    // colours are stored already multiplied by their alpha (SWF19's DefineBitsLossless2, and
    // Ruffle's `decode_define_bits_lossless` takes them as they are), so they are only clamped.
    let v2 = l.version == 2;
    let mut buf = match l.format {
        swf::BitmapFormat::ColorMap8 { num_colors } => {
            colormap8(&l.data, (w, h), num_colors as usize + 1, v2)?
        }
        swf::BitmapFormat::Rgb15 => rgb15(&l.data, (w, h))?,
        swf::BitmapFormat::Rgb32 => rgb32(&l.data, pixels, v2)?,
    };
    clamp_to_alpha(&mut buf);
    pixmap(w as u32, h as u32, buf)
}

/// A palette of `colors` entries (RGB, or RGBA in version 2), then one index per pixel with
/// each row padded to 4 bytes. An index past the palette is a transparent pixel (opaque black
/// in version 1), as Flash Player and Ruffle draw it.
fn colormap8(packed: &[u8], (w, h): (usize, usize), colors: usize, v2: bool) -> Option<Vec<u8>> {
    let entry = if v2 { 4 } else { 3 };
    let table = colors * entry;
    let row = (w + 3) & !3;
    let data = inflate_capped(packed, table + row * h)?;
    let (palette, indices) = data.split_at_checked(table)?;
    let missing = if v2 { [0, 0, 0, 0] } else { [0, 0, 0, 255] };
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for &i in indices.get(y * row..y * row + w)? {
            let c = palette_rgba(palette, i as usize * entry, v2).unwrap_or(missing);
            out.extend_from_slice(&c);
        }
    }
    Some(out)
}

/// The palette entry at byte `at` as RGBA: RGB entries (version 1) are opaque.
fn palette_rgba(palette: &[u8], at: usize, v2: bool) -> Option<[u8; 4]> {
    if v2 {
        palette.get(at..at + 4)?.try_into().ok()
    } else {
        let c = palette.get(at..at + 3)?;
        Some([c[0], c[1], c[2], 255])
    }
}

/// Two bytes a pixel, each row padded to 4 bytes. The pixel is SWF bit fields (1 reserved,
/// then 5 each of red, green and blue), read most significant bit first: a big-endian u16.
pub(super) fn rgb15(packed: &[u8], (w, h): (usize, usize)) -> Option<Vec<u8>> {
    let row = (w * 2 + 3) & !3;
    let data = inflate_capped(packed, row * h)?;
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for p in data.get(y * row..y * row + w * 2)?.as_chunks::<2>().0 {
            out.extend_from_slice(&rgb15_pixel(u16::from_be_bytes(*p)));
        }
    }
    Some(out)
}

fn rgb15_pixel(v: u16) -> [u8; 4] {
    let ch = |shift: u16| {
        let c = ((v >> shift) & 0x1F) as u8;
        (c << 3) | (c >> 2)
    };
    [ch(10), ch(5), ch(0), 255]
}

/// Four bytes a pixel: ARGB (version 2) or xRGB (version 1, where x is padding). Pixels the
/// data runs out before stay transparent.
fn rgb32(packed: &[u8], pixels: usize, v2: bool) -> Option<Vec<u8>> {
    let data = inflate_capped(packed, pixels * 4)?;
    let mut out = vec![0u8; pixels * 4];
    for (dst, src) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(data.as_chunks::<4>().0)
    {
        dst.copy_from_slice(&[src[1], src[2], src[3], if v2 { src[0] } else { 255 }]);
    }
    Some(out)
}

/// Colours stored premultiplied, held to the rule a `Pixmap` relies on: no channel above its
/// alpha. A no-op on an opaque picture.
fn clamp_to_alpha(buf: &mut [u8]) {
    for px in buf.as_chunks_mut::<4>().0.iter_mut() {
        let a = px[3];
        for c in &mut px[..3] {
            *c = (*c).min(a);
        }
    }
}

/// Straight RGBA to premultiplied, the layout a `Pixmap` holds.
fn premultiply(buf: &mut [u8]) {
    for px in buf.as_chunks_mut::<4>().0.iter_mut() {
        let a = px[3] as u32;
        for c in &mut px[..3] {
            *c = ((*c as u32 * a + 127) / 255) as u8;
        }
    }
}

fn pixmap(w: u32, h: u32, buf: Vec<u8>) -> Option<Pixmap> {
    let size = tiny_skia::IntSize::from_wh(w, h)?;
    Pixmap::from_vec(buf, size)
}
