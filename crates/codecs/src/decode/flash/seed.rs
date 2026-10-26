//! A movie that reaches every part of the tier, for the fuzz seeds and the tests. The square
//! movie draws one solid fill, so mutating it never reached the bitmap readers; this one holds
//! every bitmap kind (15-bit, 8-bit palette and 32-bit in both versions, a DefineBits that
//! leans on the file's JPEGTables, a JPEG2 behind the erased EOI SOI lead, a JPEG3 with its
//! alpha mask, a PNG inside a JPEG tag), linear, radial and focal gradients, a stroked curve,
//! a morph, a sprite, a clip layer and a colour transform, behind one blank opening frame.
//! Built with the `swf` crate's writer, so the bytes are what a real tool emits.

use std::borrow::Cow;
use std::io::Write;

use swf::{
    BitmapFormat, CharacterId, Color, ColorTransform, DefineBitsJpeg3, DefineBitsLossless,
    DefineMorphShape, DefineMorphShapeFlag, Depth, FillStyle, Fixed16, Fixed8, Gradient,
    GradientInterpolation, GradientRecord, GradientSpread, Header, LineStyle, Matrix, MorphShape,
    PlaceObject, PlaceObjectAction, PointDelta, Shape, ShapeFlag, ShapeRecord, ShapeStyles, Sprite,
    StyleChangeData, Tag,
};

use super::{px, rect};

/// The bitmap characters, in the order their fill shapes stand on the stage.
pub(super) const RICH_BITMAPS: [CharacterId; 9] = [10, 11, 12, 13, 14, 15, 16, 17, 18];

/// The rich movie: a 240 x 120 px stage on white, nothing in frame 1, everything in frame 2.
pub(crate) fn synthetic_rich_movie() -> Vec<u8> {
    let jpeg = jpeg_8x8();
    let sof = jpeg
        .windows(2)
        .position(|w| w == [0xFF, 0xC0])
        .expect("a baseline JPEG has its SOF0");
    // An old-style DefineBits: the tables up to the frame header live in JPEGTables.
    let tables = [&jpeg[..sof], &[0xFF, 0xD9][..]].concat();
    let picture = [&[0xFF, 0xD8][..], &jpeg[sof..]].concat();
    let erased_lead = [&[0xFF, 0xD9, 0xFF, 0xD8][..], &jpeg].concat();
    let mask = zlib(&[200u8; 64]);
    let png = png_2x1();

    let mut tags = vec![
        Tag::SetBackgroundColor(rgba(255, 255, 255, 255)),
        Tag::JpegTables(&tables),
    ];
    tags.extend(lossless_bitmaps().into_iter().map(Tag::DefineBitsLossless));
    tags.extend([
        Tag::DefineBits {
            id: 15,
            jpeg_data: &picture,
        },
        Tag::DefineBitsJpeg2 {
            id: 16,
            jpeg_data: &erased_lead,
        },
        Tag::DefineBitsJpeg3(DefineBitsJpeg3 {
            id: 17,
            version: 3,
            deblocking: Fixed8::ZERO,
            data: &jpeg,
            alpha_data: &mask,
        }),
        Tag::DefineBitsJpeg2 {
            id: 18,
            jpeg_data: &png,
        },
    ]);
    // One 20 x 20 px rectangle per bitmap, each bitmap pixel 10 px wide.
    for (i, &bitmap) in RICH_BITMAPS.iter().enumerate() {
        let fill = FillStyle::Bitmap {
            id: bitmap,
            matrix: Matrix::scale(Fixed16::from_f64(200.0), Fixed16::from_f64(200.0)),
            is_smoothed: i % 2 == 0,
            is_repeating: i % 3 == 0,
        };
        tags.push(Tag::DefineShape(Box::new(rect_shape(
            20 + i as u16,
            20,
            20,
            fill,
            3,
        ))));
    }
    tags.extend(vector_characters());
    tags.push(Tag::ShowFrame);
    tags.extend(frame_two());
    tags.extend([Tag::ShowFrame, Tag::End]);

    let mut header = Header::default_with_swf_version(10);
    header.stage_size = rect(240, 120);
    header.frame_rate = Fixed8::ONE;
    header.num_frames = 2;
    let mut out = Vec::new();
    swf::write_swf(&header, &tags, &mut out).expect("writing a test movie cannot fail");
    out
}

fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
    Color { r, g, b, a }
}

fn zlib(raw: &[u8]) -> Vec<u8> {
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(raw).expect("deflate");
    z.finish().expect("deflate")
}

/// An 8 x 8 baseline JPEG of a colour ramp, from the `image` crate's encoder.
fn jpeg_8x8() -> Vec<u8> {
    let img = image::RgbImage::from_fn(8, 8, |x, y| image::Rgb([x as u8 * 30, y as u8 * 30, 90]));
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90)
        .encode(img.as_raw(), 8, 8, image::ExtendedColorType::Rgb8)
        .expect("encoding a test JPEG cannot fail");
    out
}

/// A 2 x 1 PNG with straight alpha: half-transparent red, then opaque blue.
fn png_2x1() -> Vec<u8> {
    let img =
        image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 128, 0, 0, 255, 255]).expect("2x1 RGBA");
    let mut out = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("encoding a test PNG cannot fail");
    out
}

/// The five lossless kinds. Rows are padded to 4 bytes; version 2 colours are premultiplied.
fn lossless_bitmaps() -> Vec<DefineBitsLossless<'static>> {
    let make = |id, version, format, (width, height), raw: &[u8]| DefineBitsLossless {
        version,
        id,
        format,
        width,
        height,
        data: Cow::Owned(zlib(raw)),
    };
    let palette8 = BitmapFormat::ColorMap8 { num_colors: 1 };
    vec![
        // 15-bit red, blue: one 4-byte row.
        make(
            10,
            1,
            BitmapFormat::Rgb15,
            (2, 1),
            &[0x7C, 0x00, 0x00, 0x1F],
        ),
        // A green and yellow palette, then two padded rows of indices.
        make(
            11,
            1,
            palette8,
            (2, 2),
            &[0, 255, 0, 255, 255, 0, 0, 1, 0, 0, 1, 0, 0, 0],
        ),
        // The same in version 2, its second entry half transparent; index 7 is past the
        // palette and draws transparent.
        make(
            12,
            2,
            palette8,
            (2, 2),
            &[0, 255, 0, 255, 128, 0, 0, 128, 0, 1, 0, 0, 7, 0, 0, 0],
        ),
        // xRGB: red, green.
        make(
            13,
            1,
            BitmapFormat::Rgb32,
            (2, 1),
            &[0, 255, 0, 0, 0, 0, 255, 0],
        ),
        // ARGB premultiplied: opaque blue, half-transparent red.
        make(
            14,
            2,
            BitmapFormat::Rgb32,
            (2, 1),
            &[255, 0, 0, 255, 128, 128, 0, 0],
        ),
    ]
}

fn edge(dx: i32, dy: i32) -> ShapeRecord {
    ShapeRecord::StraightEdge {
        delta: PointDelta::new(px(dx), px(dy)),
    }
}

/// The records of a `w` x `h` px rectangle from the origin, filled with fill style 1 and
/// stroked with line style `line` when there is one.
fn rect_records(w: i32, h: i32, line: Option<u32>) -> Vec<ShapeRecord> {
    vec![
        ShapeRecord::StyleChange(Box::new(StyleChangeData {
            move_to: Some(swf::Point::new(px(0), px(0))),
            fill_style_0: None,
            fill_style_1: Some(1),
            line_style: line,
            new_styles: None,
        })),
        edge(w, 0),
        edge(0, h),
        edge(-w, 0),
        edge(0, -h),
    ]
}

fn rect_shape(id: CharacterId, w: i32, h: i32, fill: FillStyle, version: u8) -> Shape {
    Shape {
        version,
        id,
        shape_bounds: rect(w, h),
        edge_bounds: rect(w, h),
        flags: ShapeFlag::empty(),
        styles: ShapeStyles {
            fill_styles: vec![fill],
            line_styles: vec![],
        },
        shape: rect_records(w, h, None),
    }
}

/// A gradient over a `w` x `h` px box: the 32768-twip gradient square scaled onto it.
fn gradient(w: i32, h: i32, colours: &[Color]) -> Gradient {
    let last = colours.len().saturating_sub(1).max(1);
    Gradient {
        matrix: Matrix {
            a: Fixed16::from_f64(w as f64 * 20.0 / 32768.0),
            d: Fixed16::from_f64(h as f64 * 20.0 / 32768.0),
            tx: px(w / 2),
            ty: px(h / 2),
            ..Matrix::IDENTITY
        },
        spread: GradientSpread::Reflect,
        interpolation: GradientInterpolation::Rgb,
        records: colours
            .iter()
            .enumerate()
            .map(|(i, &color)| GradientRecord {
                ratio: (i * 255 / last) as u8,
                color,
            })
            .collect(),
    }
}

/// The gradient shapes (30 stroked with a curve, 31 focal, 32 radial), the clip shape 34, the
/// morph 33 and the sprite 40.
fn vector_characters() -> Vec<Tag<'static>> {
    let (red, blue) = (rgba(255, 0, 0, 255), rgba(0, 0, 255, 160));
    let mut stroked = rect_shape(
        30,
        60,
        30,
        FillStyle::LinearGradient(gradient(60, 30, &[red, blue])),
        3,
    );
    stroked.styles.line_styles = vec![LineStyle::new()
        .with_width(px(2))
        .with_color(rgba(0, 0, 0, 255))];
    // The bottom edge bows down as a curve; the outline still closes.
    stroked.shape = rect_records(60, 30, Some(1));
    stroked.shape[3] = ShapeRecord::CurvedEdge {
        control_delta: PointDelta::new(px(-30), px(10)),
        anchor_delta: PointDelta::new(px(-30), px(-10)),
    };
    let focal = FillStyle::FocalGradient {
        gradient: gradient(40, 30, &[red, rgba(0, 255, 0, 255), blue]),
        focal_point: Fixed8::from_f64(0.5),
    };
    let radial = FillStyle::RadialGradient(gradient(30, 30, &[blue, red]));
    let morph = DefineMorphShape {
        version: 1,
        id: 33,
        flags: DefineMorphShapeFlag::empty(),
        start: MorphShape {
            shape_bounds: rect(20, 20),
            edge_bounds: rect(20, 20),
            fill_styles: vec![FillStyle::Color(red)],
            line_styles: vec![],
            shape: rect_records(20, 20, None),
        },
        // The end outline carries no styles of its own, only its edges: the start's say how
        // each is filled.
        end: MorphShape {
            shape_bounds: rect(30, 30),
            edge_bounds: rect(30, 30),
            fill_styles: vec![FillStyle::Color(blue)],
            line_styles: vec![],
            shape: [
                vec![ShapeRecord::StyleChange(Box::new(StyleChangeData {
                    move_to: Some(swf::Point::new(px(0), px(0))),
                    fill_style_0: None,
                    fill_style_1: None,
                    line_style: None,
                    new_styles: None,
                }))],
                rect_records(30, 30, None).split_off(1),
            ]
            .concat(),
        },
    };
    vec![
        Tag::DefineShape(Box::new(stroked)),
        Tag::DefineShape(Box::new(rect_shape(31, 40, 30, focal, 4))),
        Tag::DefineShape(Box::new(rect_shape(32, 30, 30, radial, 3))),
        Tag::DefineMorphShape(Box::new(morph)),
        Tag::DefineShape(Box::new(rect_shape(34, 40, 30, FillStyle::Color(red), 1))),
        Tag::DefineSprite(Sprite {
            id: 40,
            num_frames: 1,
            tags: vec![
                Tag::PlaceObject(Box::new(place(1, 32, (0, 0)))),
                Tag::ShowFrame,
                Tag::End,
            ],
        }),
    ]
}

fn place(depth: Depth, id: CharacterId, (x, y): (i32, i32)) -> PlaceObject<'static> {
    PlaceObject {
        version: 2,
        action: PlaceObjectAction::Place(id),
        depth,
        matrix: Some(Matrix::translate(px(x), px(y))),
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
    }
}

/// Frame 2's placements: the bitmap row, the vector row, the morph half way, the sprite under
/// a colour transform, and a gradient seen through a clip.
fn frame_two() -> Vec<Tag<'static>> {
    let mut placed: Vec<PlaceObject<'static>> = (0..RICH_BITMAPS.len())
        .map(|i| place(1 + i as Depth, 20 + i as CharacterId, (25 * i as i32, 0)))
        .collect();
    placed.push(place(10, 30, (0, 40)));
    placed.push(place(11, 31, (70, 40)));
    let mut morph = place(12, 33, (140, 40));
    morph.ratio = Some(32768);
    placed.push(morph);
    let mut sprite = place(13, 40, (0, 80));
    sprite.color_transform = Some(ColorTransform {
        r_multiply: Fixed8::from_f64(0.5),
        g_add: 64,
        ..ColorTransform::IDENTITY
    });
    placed.push(sprite);
    let mut clip = place(20, 34, (100, 80));
    clip.clip_depth = Some(22);
    placed.push(clip);
    placed.push(place(21, 30, (90, 75)));
    placed
        .into_iter()
        .map(|p| Tag::PlaceObject(Box::new(p)))
        .collect()
}
