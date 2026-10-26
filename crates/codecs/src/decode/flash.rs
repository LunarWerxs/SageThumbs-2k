//! The Adobe Flash tier (`.swf`, issue #70): the thumbnail is a frame of the main timeline,
//! rasterised from its vectors and bitmaps. Nothing runs: no ActionScript, no device text.
//!
//! Which frame: the first of the opening [`MAX_SCAN_FRAMES`] whose drawing covers at least
//! [`CONTENT_COVER`] of the stage, else the one that covers the most. Frame 1 alone was not
//! enough: the reporter's movie opens on a preloader (a "LOADING" line in a device font and a
//! faint dotted bar on a plain background), and its pictures only arrive in frame 2. Drawn
//! from frame 1, its thumbnail was an empty yellow square.
//!
//! The tags are parsed by the `swf` crate (Ruffle's, MIT/Apache). Its decompressor would
//! inflate a header-declared length without bound, so the size is checked here first. The
//! render runs on a budgeted worker like the SVG tier, and the scene caps its own work.

mod geom;
mod paint;
mod scene;
#[cfg(test)]
mod tests;

use std::time::Duration;

use super::*;
use geom::Aff;
use paint::Cx;
use scene::{Defs, Placed, Scene};
use swf::{Color, Depth, Tag};
#[cfg(test)]
use swf::{
    FillStyle, Fixed16, Fixed8, Header, Matrix, PlaceObject, PlaceObjectAction, Rectangle, Shape,
    ShapeFlag, ShapeRecord, ShapeStyles, StyleChangeData, Twips,
};
use tiny_skia::Pixmap;

/// Wall-clock cap on one render. A frame-1 draw is milliseconds; this only bounds a movie
/// that is built to be slow, and the abandoned-worker budget counts it like the SVG cap.
const SWF_TIMEOUT: Duration = Duration::from_secs(10);

/// The thumbnail's longest side is held in this range. A stage smaller than the floor is
/// scaled up (vector art stays sharp); one larger than the ceiling is scaled down.
const SWF_MIN_PX: f64 = 256.0;
const SWF_MAX_PX: f64 = 2048.0;

/// The largest decompressed movie this tier will parse. Real frame-1 content is far below it.
const SWF_MAX_UNCOMPRESSED: u32 = 64 * 1024 * 1024;

/// Twips per pixel: SWF measures shapes in twentieths of a pixel.
const TWIPS_PER_PX: f64 = 20.0;

/// How many opening frames of the main timeline are looked at for the thumbnail. A preloader
/// is one to three frames; the scene's work caps are shared by every frame drawn.
const MAX_SCAN_FRAMES: usize = 12;

/// The share of the stage a frame must draw over (not left as background) to be the thumbnail.
const CONTENT_COVER: f64 = 0.05;

/// How far a pixel's channel must be from the background's to count as drawn.
const DRAWN_DELTA: u8 = 24;

#[cfg(test)]
fn px(v: i32) -> Twips {
    Twips::from_pixels_i32(v)
}

#[cfg(test)]
fn rect(w: i32, h: i32) -> Rectangle<Twips> {
    Rectangle {
        x_min: px(0),
        x_max: px(w),
        y_min: px(0),
        y_max: px(h),
    }
}

/// A small valid movie for the fuzz seeds and the tests: a `w` x `h` pixel stage whose frame 1
/// is a red 100 x 100 square at the origin on a blue background. Built with the crate's own
/// writer, so the bytes are what a real tool emits.
#[cfg(test)]
pub(crate) fn synthetic_square_movie(w: i32, h: i32) -> Vec<u8> {
    synthetic_movie(w, h, 0)
}

/// [`synthetic_square_movie`] with `blank` empty frames before the square's.
#[cfg(test)]
pub(crate) fn synthetic_movie(w: i32, h: i32, blank: usize) -> Vec<u8> {
    let red = Color {
        r: 255,
        g: 0,
        b: 0,
        a: 255,
    };
    let edge = |dx: i32, dy: i32| ShapeRecord::StraightEdge {
        delta: swf::PointDelta {
            dx: px(dx),
            dy: px(dy),
        },
    };
    let shape = Shape {
        version: 1,
        id: 1,
        shape_bounds: rect(100, 100),
        edge_bounds: rect(100, 100),
        flags: ShapeFlag::empty(),
        styles: ShapeStyles {
            fill_styles: vec![FillStyle::Color(red)],
            line_styles: vec![],
        },
        shape: vec![
            ShapeRecord::StyleChange(Box::new(StyleChangeData {
                move_to: Some(swf::Point { x: px(0), y: px(0) }),
                fill_style_0: None,
                fill_style_1: Some(1),
                line_style: None,
                new_styles: None,
            })),
            edge(100, 0),
            edge(0, 100),
            edge(-100, 0),
            edge(0, -100),
        ],
    };
    let place = PlaceObject {
        version: 2,
        action: PlaceObjectAction::Place(1),
        depth: 1,
        matrix: Some(Matrix {
            a: Fixed16::from_f64(1.0),
            b: Fixed16::from_f64(0.0),
            c: Fixed16::from_f64(0.0),
            d: Fixed16::from_f64(1.0),
            tx: Twips::new(0),
            ty: Twips::new(0),
        }),
        color_transform: None,
        ratio: None,
        name: None,
        clip_depth: None,
        class_name: None,
        filters: None,
        background_color: None,
        blend_mode: None,
        clip_actions: None,
        has_image: false,
        is_bitmap_cached: None,
        is_visible: None,
        amf_data: None,
    };
    let mut tags = vec![
        Tag::SetBackgroundColor(Color {
            r: 0,
            g: 0,
            b: 255,
            a: 255,
        }),
        Tag::DefineShape(Box::new(shape)),
    ];
    tags.extend((0..blank).map(|_| Tag::ShowFrame));
    tags.extend([Tag::PlaceObject(Box::new(place)), Tag::ShowFrame, Tag::End]);
    let mut header = Header::default_with_swf_version(10);
    header.stage_size = rect(w, h);
    header.frame_rate = Fixed8::ONE;
    header.num_frames = blank as u16 + 1;
    let mut out = Vec::new();
    swf::write_swf(&header, &tags, &mut out).expect("writing a test movie cannot fail");
    out
}

/// Magic of an SWF: `FWS` (uncompressed), `CWS` (zlib), `ZWS` (LZMA).
pub(crate) fn looks_like_swf(bytes: &[u8]) -> bool {
    matches!(bytes.get(..3), Some(b"FWS" | b"CWS" | b"ZWS"))
}

/// The cascade's entry: `Some` for an SWF, so a file that is not one falls through.
pub(crate) fn try_swf_tier(bytes: &[u8]) -> Option<Result<DynamicImage>> {
    looks_like_swf(bytes).then(|| decode_swf(bytes))
}

/// Parse and render on a budgeted worker, joined with a deadline.
fn decode_swf(bytes: &[u8]) -> Result<DynamicImage> {
    let owned = bytes.to_vec();
    match st2k_base::safety::spawn_budgeted("st2k-swf-render", SWF_TIMEOUT, move || {
        render_swf(&owned)
    }) {
        Some(r) => r,
        None => Err(Error::from(E_FAIL)),
    }
}

/// The render itself, run on the worker thread above.
pub(crate) fn render_swf(bytes: &[u8]) -> Result<DynamicImage> {
    let buf = inflate(bytes).ok_or_else(|| Error::from(E_FAIL))?;
    let movie = swf::parse_swf(&buf).map_err(|e| {
        st2k_base::safety::log_debugf!("SWF parse failed: {e:?}");
        Error::from(E_FAIL)
    })?;
    let (w, h, root) = stage_fit(movie.header.stage_size()).ok_or_else(|| Error::from(E_FAIL))?;

    // The first SetBackgroundColor before frame 1 wins, as in the player.
    let bg = background(&movie.tags)
        .or_else(|| movie.header.background_color())
        .unwrap_or(Color {
            r: 255,
            g: 255,
            b: 255,
            a: 255,
        });
    let mut scene = Scene::new(Defs::collect(&movie.tags), w, h);
    let pixmap =
        pick_frame(&mut scene, &movie.tags, root, bg, (w, h)).ok_or_else(|| Error::from(E_FAIL))?;

    let mut buf = pixmap.data().to_vec();
    super::svg::unpremultiply_rgba(&mut buf);
    let img = image::RgbaImage::from_raw(w, h, buf).ok_or_else(|| Error::from(E_FAIL))?;
    Ok(DynamicImage::ImageRgba8(img))
}

/// The movie's tag data, inflated. ZWS is LZMA, which this tier does not inflate: it declines.
/// The header's declared length is checked against [`SWF_MAX_UNCOMPRESSED`] first, because the
/// crate's decompressor would inflate it without bound.
fn inflate(bytes: &[u8]) -> Option<swf::SwfBuf> {
    if bytes.first() == Some(&b'Z') {
        return None;
    }
    let declared = u32::from_le_bytes(bytes.get(4..8)?.try_into().ok()?);
    if declared > SWF_MAX_UNCOMPRESSED {
        st2k_base::safety::log_debugf!("SWF declares {declared} bytes uncompressed; refused");
        return None;
    }
    swf::decompress_swf(bytes)
        .map_err(|e| st2k_base::safety::log_debugf!("SWF decompress failed: {e:?}"))
        .ok()
}

/// The thumbnail's size for a stage, its longest side held in [`SWF_MIN_PX`]..[`SWF_MAX_PX`],
/// and the map from shape units (twips) to its pixels: the stage's corner to the origin, scaled.
fn stage_fit(stage: &swf::Rectangle<swf::Twips>) -> Option<(u32, u32, Aff)> {
    let (sw, sh) = (stage.width().get() as f64, stage.height().get() as f64);
    if !(sw > 0.0 && sh > 0.0) {
        return None;
    }
    let (w_px, h_px) = (sw / TWIPS_PER_PX, sh / TWIPS_PER_PX);
    let longest = w_px.max(h_px);
    let k = (longest.clamp(SWF_MIN_PX, SWF_MAX_PX)) / longest;
    let w = (w_px * k).ceil().max(1.0) as u32;
    let h = (h_px * k).ceil().max(1.0) as u32;
    let s = k / TWIPS_PER_PX;
    let root = Aff {
        a: s,
        d: s,
        e: -(stage.x_min.get() as f64) * s,
        f: -(stage.y_min.get() as f64) * s,
        ..Aff::IDENTITY
    };
    Some((w, h, root))
}

/// Draws the opening frames on `bg` and keeps the thumbnail's: the first that covers
/// [`CONTENT_COVER`] of the stage, else the one that covers the most (the earliest on a tie).
fn pick_frame(
    scene: &mut Scene<'_>,
    tags: &[Tag<'_>],
    root: Aff,
    bg: Color,
    (w, h): (u32, u32),
) -> Option<Pixmap> {
    let mut best: Option<(f64, Pixmap)> = None;
    for list in scene::frames(tags, MAX_SCAN_FRAMES) {
        let mut pixmap = Pixmap::new(w, h)?;
        pixmap.fill(tiny_skia::Color::from_rgba8(bg.r, bg.g, bg.b, 255));
        let items: Vec<(Depth, Placed)> = list.into_iter().collect();
        scene.draw_list(&mut pixmap, &items, root, Cx::IDENTITY, 0);
        let cover = drawn_share(&pixmap, bg);
        if cover >= CONTENT_COVER {
            return Some(pixmap);
        }
        if best.as_ref().is_none_or(|(c, _)| cover > *c) {
            best = Some((cover, pixmap));
        }
    }
    best.map(|(_, p)| p)
}

/// The share of the pixmap's pixels that differ from the opaque background `bg` by more than
/// [`DRAWN_DELTA`] in some channel. The background is opaque, so the pixels are too and their
/// premultiplied values are the colours themselves.
fn drawn_share(pixmap: &Pixmap, bg: Color) -> f64 {
    let far = |a: u8, b: u8| a.abs_diff(b) > DRAWN_DELTA;
    let (px, _) = pixmap.data().as_chunks::<4>();
    let drawn = px
        .iter()
        .filter(|p| far(p[0], bg.r) || far(p[1], bg.g) || far(p[2], bg.b))
        .count();
    drawn as f64 / px.len().max(1) as f64
}

/// The colour of the first `SetBackgroundColor` before the first frame, if any.
fn background(tags: &[Tag<'_>]) -> Option<Color> {
    for tag in tags {
        match tag {
            Tag::ShowFrame => return None,
            Tag::SetBackgroundColor(bg) => return Some(*bg),
            _ => {}
        }
    }
    None
}
