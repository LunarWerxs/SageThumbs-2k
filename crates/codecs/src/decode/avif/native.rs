//! The AVIF decode itself: every item's AV1 picture through `rav1d` (`av1`), converted by our
//! own code (`yuv`), assembled (grid tiles, alpha, clean aperture, rotation, mirror) and handed
//! to the shared colour code. Compiled only where a decoder panic cannot reach Explorer: the
//! EXEs (feature `av1`; `st2k avif-frame` answers the shell extension from one) and this
//! crate's tests. See the parent module's doc.

use super::super::*;
use super::container::{Avif, Grid, Nclx};
use super::{fail, is_hdr};

mod av1;
mod yuv;

type Rgba16 = image::ImageBuffer<image::Rgba<u16>, Vec<u16>>;

/// The largest picture (or grid canvas) decoded: 16-bit RGBA must fit the allocation ceiling.
const MAX_AVIF_PIXELS: u64 = MAX_ALLOC / 8;
/// Decode an AVIF's primary picture on at most `threads` threads. With `max_edge`, the picture
/// comes back reduced by a power of two to no less than that edge (block-averaged, see
/// `yuv::convert`): AV1 has no reduced-resolution decode, but the conversion and everything
/// after it need not be full size, and for a thumbnail that is most of the time and memory.
pub(in crate::decode) fn decode_avif(
    bytes: &[u8],
    threads: u32,
    max_edge: Option<u32>,
) -> Result<DynamicImage> {
    let file = Avif::parse(bytes).ok_or_else(|| fail("no readable primary item"))?;
    let id = file.primary;
    let picture = match file.kind(id).as_ref() {
        Some(b"av01") => single(&file, id, threads, max_edge)?,
        Some(b"grid") => grid(&file, id, threads, max_edge)?,
        _ => return Err(fail("the primary item is not an AV1 picture")),
    };
    let colour = picture.colour;
    let img = transform(&file, id, picture);
    Ok(finish_colour(&file, id, bytes, img, colour))
}

/// A decoded picture before its transforms and colour management: `image` is the `full` size
/// reduced by `step`.
struct Picture {
    image: DynamicImage,
    colour: Nclx,
    step: u32,
    full: (u32, u32),
}

/// The power-of-two reduction for a `width` x `height` picture wanted at `max_edge`: the largest
/// that still leaves the edge. The reduction is a block average and the caller's final fit an
/// area average, so the two together are the one area average from full size.
fn reduction(width: u32, height: u32, max_edge: Option<u32>) -> u32 {
    let Some(edge) = max_edge else { return 1 };
    let room = width.max(height) / edge.max(1);
    if room < 2 {
        1
    } else {
        1 << (31 - room.leading_zeros())
    }
}

/// An RGBA canvas at the depth the picture needs: 8-bit, or 16-bit for more than 8 bits or HDR.
enum Canvas {
    Eight(image::RgbaImage),
    Sixteen(Rgba16),
}

impl Canvas {
    fn new(width: u32, height: u32, wide: bool) -> Result<Self> {
        if u64::from(width) * u64::from(height) > MAX_AVIF_PIXELS
            || width > MAX_DIM
            || height > MAX_DIM
        {
            return Err(fail("the picture is larger than the decode limit"));
        }
        Ok(if wide {
            Canvas::Sixteen(Rgba16::new(width, height))
        } else {
            Canvas::Eight(image::RgbaImage::new(width, height))
        })
    }

    fn size(&self) -> (u32, u32) {
        match self {
            Canvas::Eight(c) => c.dimensions(),
            Canvas::Sixteen(c) => c.dimensions(),
        }
    }

    /// Write one picture's pixels, reduced by `step`, at (`left`, `top`), clipped to the canvas,
    /// alpha opaque.
    fn paint(&mut self, frame: &av1::Frame, colour: Nclx, step: u32, left: u32, top: u32) {
        let (w, h) = self.size();
        let matrix = yuv::matrix_for(colour.matrix);
        let full = colour.full_range;
        match self {
            Canvas::Eight(c) => yuv::convert(frame, matrix, full, step, |x, y, rgb| {
                let (x, y) = (x + left, y + top);
                if x < w && y < h {
                    let [r, g, b] = rgb.map(|v| (v * 255.0 + 0.5) as u8);
                    c.put_pixel(x, y, image::Rgba([r, g, b, 255]));
                }
            }),
            Canvas::Sixteen(c) => yuv::convert(frame, matrix, full, step, |x, y, rgb| {
                let (x, y) = (x + left, y + top);
                if x < w && y < h {
                    let [r, g, b] = rgb.map(|v| (v * 65535.0 + 0.5) as u16);
                    c.put_pixel(x, y, image::Rgba([r, g, b, 65535]));
                }
            }),
        }
    }

    /// Set the alpha channel from `alpha` (one value per pixel, row-major, 0..=1), undoing a
    /// premultiplication when the file declares one.
    fn apply_alpha(&mut self, alpha: &[f32], premultiplied: bool) {
        match self {
            Canvas::Eight(c) => c.pixels_mut().zip(alpha).for_each(|(px, &a)| {
                let unit = with_alpha(px.0.map(|v| f32::from(v) / 255.0), a, premultiplied);
                px.0 = unit.map(|v| (v * 255.0 + 0.5) as u8);
            }),
            Canvas::Sixteen(c) => c.pixels_mut().zip(alpha).for_each(|(px, &a)| {
                let unit = with_alpha(px.0.map(|v| f32::from(v) / 65535.0), a, premultiplied);
                px.0 = unit.map(|v| (v * 65535.0 + 0.5) as u16);
            }),
        }
    }

    fn into_image(self) -> DynamicImage {
        match self {
            Canvas::Eight(c) => DynamicImage::ImageRgba8(c),
            Canvas::Sixteen(c) => DynamicImage::ImageRgba16(c),
        }
    }
}

/// An RGBA pixel in 0..=1 given alpha `a`, its colour unpremultiplied first when `premultiplied`.
fn with_alpha([r, g, b, _]: [f32; 4], a: f32, premultiplied: bool) -> [f32; 4] {
    let un = |v: f32| {
        if premultiplied && a > 0.0 {
            (v / a).min(1.0)
        } else {
            v
        }
    };
    [un(r), un(g), un(b), a]
}

/// Decode one AV1 item.
fn decode_item(file: &Avif<'_>, id: u32, threads: u32) -> Result<av1::Frame> {
    let data = file
        .data(id)
        .ok_or_else(|| fail("an item's bytes are missing"))?;
    av1::decode(
        &data,
        threads,
        MAX_AVIF_PIXELS.min(u64::from(u32::MAX)) as u32,
    )
    .ok_or_else(|| fail("the AV1 picture did not decode"))
}

/// The colour description of `id`: its `nclx`, with any field that says "unspecified" (2) taken
/// from the decoded picture's own sequence header instead, and the header alone when there is
/// no `nclx`. ffmpeg writes an `nclx` of 2/2/9 over a PQ / BT.2020 stream, and read literally
/// that is an SDR picture shown as its raw signal.
fn colour_of(file: &Avif<'_>, id: u32, frame: &av1::Frame) -> Nclx {
    let stream = frame.sequence_colour();
    let Some(boxed) = file.nclx(id) else {
        return stream.unwrap_or(Nclx {
            primaries: 1,
            transfer: 13,
            matrix: 6,
            full_range: false,
        });
    };
    let or_stream = |own: u16, from: fn(&Nclx) -> u16| match (own, &stream) {
        (2, Some(s)) => from(s),
        _ => own,
    };
    Nclx {
        primaries: or_stream(boxed.primaries, |s| s.primaries),
        transfer: or_stream(boxed.transfer, |s| s.transfer),
        matrix: or_stream(boxed.matrix, |s| s.matrix),
        full_range: boxed.full_range,
    }
}

fn single(file: &Avif<'_>, id: u32, threads: u32, max_edge: Option<u32>) -> Result<Picture> {
    let frame = decode_item(file, id, threads)?;
    let colour = colour_of(file, id, &frame);
    let full = (frame.width, frame.height);
    let step = reduction(full.0, full.1, max_edge);
    let wide = frame.bits > 8 || is_hdr(colour);
    let mut canvas = Canvas::new(full.0.div_ceil(step), full.1.div_ceil(step), wide)?;
    canvas.paint(&frame, colour, step, 0, 0);
    drop(frame);
    add_alpha(file, id, threads, step, &mut canvas);
    Ok(Picture {
        image: canvas.into_image(),
        colour,
        step,
        full,
    })
}

fn grid(file: &Avif<'_>, id: u32, threads: u32, max_edge: Option<u32>) -> Result<Picture> {
    let g = file
        .grid(id)
        .ok_or_else(|| fail("the grid descriptor is malformed"))?;
    let first = decode_item(file, g.tiles[0], threads)?;
    let (tw, th) = (first.width, first.height);
    // The tiles must cover the canvas, and the canvas must not be larger than they are.
    if u64::from(tw) * u64::from(g.columns) < u64::from(g.width)
        || u64::from(th) * u64::from(g.rows) < u64::from(g.height)
    {
        return Err(fail("the grid's tiles do not cover its canvas"));
    }
    let step = grid_step(&g, (tw, th), max_edge);
    let colour = file
        .nclx(id)
        .unwrap_or_else(|| colour_of(file, g.tiles[0], &first));
    let wide = first.bits > 8 || is_hdr(colour);
    let mut canvas = Canvas::new(g.width.div_ceil(step), g.height.div_ceil(step), wide)?;
    canvas.paint(&first, colour, step, 0, 0);
    drop(first);
    let run = TileRun {
        file,
        grid: &g,
        threads,
        colour,
        step,
        tile_size: (tw, th),
    };
    for (i, &tile) in g.tiles.iter().enumerate().skip(1) {
        run.paint(i as u32, tile, &mut canvas)?;
    }
    add_alpha(file, id, threads, step, &mut canvas);
    Ok(Picture {
        image: canvas.into_image(),
        colour,
        step,
        full: (g.width, g.height),
    })
}

/// The reduction for a grid of `tile`-sized tiles: one that divides the tile size, so the
/// reduced tiles still meet at whole pixels.
fn grid_step(g: &Grid, (tw, th): (u32, u32), max_edge: Option<u32>) -> u32 {
    let mut step = reduction(g.width, g.height, max_edge);
    while step > 1 && (tw % step != 0 || th % step != 0) {
        step /= 2;
    }
    step
}

/// What every tile of one grid is decoded and painted with.
struct TileRun<'r, 'a> {
    file: &'r Avif<'a>,
    grid: &'r Grid,
    threads: u32,
    colour: Nclx,
    step: u32,
    tile_size: (u32, u32),
}

impl TileRun<'_, '_> {
    /// Decode tile number `i` (item `tile`) and paint it, reduced by `step`, in its place.
    fn paint(&self, i: u32, tile: u32, canvas: &mut Canvas) -> Result<()> {
        let (tw, th) = self.tile_size;
        let frame = decode_item(self.file, tile, self.threads)?;
        if (frame.width, frame.height) != (tw, th) {
            return Err(fail("the grid's tiles differ in size"));
        }
        let (row, col) = (i / self.grid.columns, i % self.grid.columns);
        let step = self.step;
        canvas.paint(&frame, self.colour, step, col * tw / step, row * th / step);
        Ok(())
    }
}

/// Give `canvas` the alpha plane of `id`, reduced by `step` like the colour, if it has one that
/// decodes to the same size (a single AV1 item or a grid of them). A missing or broken alpha
/// leaves the picture opaque rather than losing it.
fn add_alpha(file: &Avif<'_>, id: u32, threads: u32, step: u32, canvas: &mut Canvas) {
    let Some(alpha_id) = file.alpha_for(id) else {
        return;
    };
    let (w, h) = canvas.size();
    // Made once the first alpha picture is in hand, when its decoder is gone, not before:
    // beside the canvas and the decoder's own buffers it made the decode's peak (a 512x384
    // picture as a 256 px thumbnail peaked at 774 KB, and peaks at 595 KB this way).
    let mut plane = Vec::new();
    let mut paint = |frame: &av1::Frame, left: u32, top: u32| {
        if plane.is_empty() {
            plane = vec![1.0f32; w as usize * h as usize];
        }
        let full = frame.sequence_colour().is_none_or(|c| c.full_range);
        yuv::convert_alpha(frame, full, step, |x, y, a| {
            let (x, y) = (x + left, y + top);
            if x < w && y < h {
                plane[(y * w + x) as usize] = a;
            }
        });
    };
    let painted = match file.kind(alpha_id).as_ref() {
        Some(b"av01") => decode_item(file, alpha_id, threads)
            .ok()
            .filter(|f| (f.width.div_ceil(step), f.height.div_ceil(step)) == (w, h))
            .map(|f| paint(&f, 0, 0))
            .is_some(),
        Some(b"grid") => file.grid(alpha_id).is_some_and(|g| {
            g.tiles.iter().enumerate().all(|(i, &tile)| {
                decode_item(file, tile, threads).is_ok_and(|f| {
                    let (row, col) = (i as u32 / g.columns, i as u32 % g.columns);
                    paint(&f, col * f.width / step, row * f.height / step);
                    true
                })
            })
        }),
        _ => false,
    };
    if painted {
        canvas.apply_alpha(&plane, file.premultiplied(id, alpha_id));
    }
}

/// The transformative properties, in the order HEIF applies them: clean aperture (a crop, in
/// full-size pixels, scaled to the picture's reduction), rotation, mirror.
fn transform(file: &Avif<'_>, id: u32, picture: Picture) -> DynamicImage {
    let Picture {
        image: img,
        step,
        full: (fw, fh),
        ..
    } = picture;
    let crop = file
        .property(id, b"clap")
        .and_then(|c| clean_aperture(c, fw, fh))
        .map(|(x, y, w, h)| (x / step, y / step, (w / step).max(1), (h / step).max(1)))
        .filter(|&(x, y, w, h)| x + w <= img.width() && y + h <= img.height());
    let img = match crop {
        Some((x, y, w, h)) => img.crop_imm(x, y, w, h),
        None => img,
    };
    // `irot`: anticlockwise, in quarter turns.
    let img = match file
        .property(id, b"irot")
        .and_then(|b| b.first())
        .map(|a| a & 3)
    {
        Some(1) => img.rotate270(),
        Some(2) => img.rotate180(),
        Some(3) => img.rotate90(),
        _ => img,
    };
    // `imir`: axis 0 exchanges top and bottom, 1 left and right (ISO/IEC 23008-12:2022 6.5.12;
    // Exif orientations 4 and 2 respectively).
    match file
        .property(id, b"imir")
        .and_then(|b| b.first())
        .map(|a| a & 1)
    {
        Some(0) => img.flipv(),
        Some(1) => img.fliph(),
        _ => img,
    }
}

/// The crop `clap` describes on a `width` x `height` picture, as (x, y, w, h); `None` for one
/// that is not a whole-pixel rectangle inside the picture (it is then ignored, as libavif does
/// for a thumbnail's purposes).
fn clean_aperture(clap: &[u8], width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    let field = |i: usize| -> Option<f64> {
        let n = i32::from_be_bytes(clap.get(i * 8..i * 8 + 4)?.try_into().ok()?);
        let d = i32::from_be_bytes(clap.get(i * 8 + 4..i * 8 + 8)?.try_into().ok()?);
        (d > 0).then(|| f64::from(n) / f64::from(d))
    };
    let (cw, ch, dx, dy) = (field(0)?, field(1)?, field(2)?, field(3)?);
    let x = dx + (f64::from(width) - cw) / 2.0;
    let y = dy + (f64::from(height) - ch) / 2.0;
    let whole = |v: f64| v >= 0.0 && v.fract() == 0.0;
    (whole(cw) && whole(ch) && whole(x) && whole(y) && cw >= 1.0 && ch >= 1.0)
        .then_some((x as u32, y as u32, cw as u32, ch as u32))
        .filter(|&(x, y, w, h)| {
            u64::from(x) + u64::from(w) <= u64::from(width)
                && u64::from(y) + u64::from(h) <= u64::from(height)
        })
}

/// Turn the picture's own colour into sRGB for display: an HDR transfer (PQ or HLG) through the
/// shared cICP conversion and float tone map, anything else through its ICC profile (the item's
/// own, or the one its `nclx` primaries stand for).
fn finish_colour(
    file: &Avif<'_>,
    id: u32,
    bytes: &[u8],
    img: DynamicImage,
    colour: Nclx,
) -> DynamicImage {
    if is_hdr(colour) {
        let cicp = cicp::PngCicp {
            primaries: u8::try_from(colour.primaries).unwrap_or(9),
            transfer: colour.transfer as u8,
            // The conversion above already produced full-range R'G'B'.
            full_range: true,
        };
        if let Some(linear) = cicp_hdr_to_linear(&img, &cicp) {
            return tone_map_float(&linear);
        }
    }
    let icc = file
        .icc(id)
        .map(<[u8]>::to_vec)
        .or_else(|| color::isobmff_color_icc(bytes));
    apply_icc_to_srgb(img, icc)
}

#[cfg(test)]
mod tests;
