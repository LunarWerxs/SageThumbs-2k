//! Flash tier tests: a movie built in code (so the drawing has known answers), a sweep of
//! hostile bytes that must never panic, and the reporter's own file when the corpus has it.

use super::*;

fn rgba(img: &DynamicImage, x: u32, y: u32) -> [u8; 4] {
    img.to_rgba8().get_pixel(x, y).0
}

#[test]
fn a_filled_square_on_a_background_renders_both_colours() {
    let img = render_swf(&synthetic_square_movie(400, 200)).expect("a valid movie renders");
    assert_eq!(
        (img.width(), img.height()),
        (400, 200),
        "stage is 400x200 px, under the ceiling"
    );
    assert_eq!(
        rgba(&img, 50, 50),
        [255, 0, 0, 255],
        "inside the square is its fill"
    );
    assert_eq!(
        rgba(&img, 300, 150),
        [0, 0, 255, 255],
        "outside it is the background colour"
    );
}

#[test]
fn a_small_stage_is_scaled_up_to_the_floor() {
    let img = render_swf(&synthetic_square_movie(100, 50)).expect("a valid movie renders");
    assert_eq!(
        (img.width(), img.height()),
        (256, 128),
        "a 100x50 stage is scaled to 256 wide"
    );
    assert_eq!(
        rgba(&img, 100, 60),
        [255, 0, 0, 255],
        "the square scales with the stage"
    );
}

#[test]
fn a_blank_opening_frame_gives_way_to_the_first_frame_that_draws() {
    let img = render_swf(&synthetic_movie(400, 200, 2)).expect("a valid movie renders");
    assert_eq!(
        rgba(&img, 50, 50),
        [255, 0, 0, 255],
        "the square of frame 3 is the thumbnail"
    );
}

#[test]
fn a_frame_past_the_scan_is_not_drawn() {
    let img =
        render_swf(&synthetic_movie(400, 200, MAX_SCAN_FRAMES)).expect("a valid movie renders");
    assert_eq!(
        rgba(&img, 50, 50),
        [0, 0, 255, 255],
        "nothing in the scanned frames: the background"
    );
}

#[test]
fn a_declared_length_past_the_cap_is_refused_before_inflating() {
    let mut bytes = synthetic_square_movie(400, 200);
    bytes[0..3].copy_from_slice(b"CWS");
    bytes[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    assert!(render_swf(&bytes).is_err());
}

#[test]
fn a_truncated_or_corrupted_movie_never_panics() {
    let good = synthetic_square_movie(400, 200);
    for n in 0..good.len() {
        let _ = render_swf(&good[..n]);
    }
    for i in (0..good.len()).step_by(3) {
        let mut bad = good.clone();
        bad[i] ^= 0xFF;
        let _ = render_swf(&bad);
    }
}

#[test]
fn the_magic_is_read_from_the_first_three_bytes() {
    assert!(looks_like_swf(b"FWS\x0a"));
    assert!(looks_like_swf(b"CWS\x0a"));
    assert!(looks_like_swf(b"ZWS\x0d"));
    assert!(!looks_like_swf(b"FW"));
    assert!(!looks_like_swf(b"\x89PNG"));
}

/// A 2x1 picture, red then green, filled at 50 px a picture pixel into a 100x50 px rectangle:
/// its matrix scales by 20 x 50 (twips per picture pixel) and shape twips go to pixels at 1/20.
/// Scaled by 20 once too often, the left picture pixel covered the whole rectangle.
#[test]
fn a_bitmap_fill_maps_its_picture_through_its_own_matrix() {
    let mut pic = Pixmap::new(2, 1).expect("2x1");
    pic.pixels_mut()[0] = tiny_skia::ColorU8::from_rgba(255, 0, 0, 255).premultiply();
    pic.pixels_mut()[1] = tiny_skia::ColorU8::from_rgba(0, 255, 0, 255).premultiply();
    let m = Matrix {
        a: Fixed16::from_f64(1000.0),
        d: Fixed16::from_f64(1000.0),
        ..Matrix::IDENTITY
    };
    let shader = paint::bitmap_shader(&pic, &m, Aff::scale(1.0 / 20.0), false, false);
    let mut target = Pixmap::new(100, 50).expect("100x50");
    let rect = tiny_skia::Rect::from_xywh(0.0, 0.0, 100.0, 50.0).expect("rect");
    let fill = tiny_skia::Paint {
        shader,
        ..tiny_skia::Paint::default()
    };
    target.fill_rect(rect, &fill, tiny_skia::Transform::identity(), None);
    let px = |x, y| {
        target
            .pixel(x, y)
            .map(|p| [p.red(), p.green(), p.blue(), p.alpha()])
    };
    assert_eq!(
        px(25, 25),
        Some([255, 0, 0, 255]),
        "the left half is the red pixel"
    );
    assert_eq!(
        px(75, 25),
        Some([0, 255, 0, 255]),
        "the right half is the green pixel"
    );
}

/// A 15-bit pixel is SWF bit fields read most significant bit first: `7C 00` is pure red and
/// `00 1F` pure blue. Read little-endian they came out as green-blue and a dim red.
#[test]
fn fifteen_bit_pixels_read_big_endian() {
    use std::io::Write;
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    z.write_all(&[0x7C, 0x00, 0x00, 0x1F]).expect("deflate");
    let packed = z.finish().expect("deflate");
    assert_eq!(
        paint::rgb15(&packed, (2, 1)),
        Some(vec![255, 0, 0, 255, 0, 0, 255, 255])
    );
}

/// A JPEG made of separate streams comes out as one: a single SOI, the segments in order,
/// a single EOI, and no stray SOI or EOI between them.
#[test]
fn split_jpeg_streams_are_joined_into_one() {
    let dqt = [0xFF, 0xDB, 0x00, 0x03, 0x07];
    let sos_and_data = [
        0xFF, 0xDA, 0x00, 0x02, 0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56,
    ];
    let tables = [&[0xFF, 0xD8][..], &dqt, &[0xFF, 0xD9]].concat();
    let picture = [
        &[0xFF, 0xD9, 0xFF, 0xD8, 0xFF, 0xD8][..],
        &sos_and_data,
        &[0xFF, 0xD9],
    ]
    .concat();
    let one = paint::one_jpeg_stream(&[&tables, &picture]);
    let want = [&[0xFF, 0xD8][..], &dqt, &sos_and_data, &[0xFF, 0xD9]].concat();
    assert_eq!(one, want);
    // Tables and picture in one tag, as DefineBitsJPEG2 stores them.
    assert_eq!(paint::one_jpeg_stream(&[&[tables, picture].concat()]), want);
}

/// The rich seed's bitmaps, decoded the way the scene decodes them, by character id.
fn rich_bitmap(id: u16) -> Option<Pixmap> {
    let bytes = synthetic_rich_movie();
    let buf = swf::decompress_swf(&bytes[..]).expect("the rich seed inflates");
    let movie = swf::parse_swf(&buf).expect("the rich seed parses");
    let defs = scene::Defs::collect(&movie.tags);
    match defs.by_id.get(&id) {
        Some(scene::Def::Bits(b)) => decode_unbudgeted(*b, defs.jpeg_tables),
        _ => panic!("character {id} is not a bitmap"),
    }
}

/// One picture decoded on its own, with no movie-wide pixel budget behind it.
fn decode_unbudgeted(bits: paint::Bits<'_>, tables: Option<&[u8]>) -> Option<Pixmap> {
    let mut pixels_left = u64::MAX;
    paint::decode_bits(bits, tables, &mut pixels_left)
}

fn premul(p: &Pixmap, x: u32, y: u32) -> [u8; 4] {
    let c = p.pixel(x, y).expect("pixel in range");
    [c.red(), c.green(), c.blue(), c.alpha()]
}

/// Every bitmap kind in the rich seed decodes (so its mutations reach every reader), and the
/// movie's frame 2 is drawn, not its blank frame 1.
#[test]
fn the_rich_seed_reaches_every_bitmap_kind_and_draws_its_second_frame() {
    for id in seed::RICH_BITMAPS {
        assert!(rich_bitmap(id).is_some(), "bitmap {id} decodes");
    }
    let img = render_swf(&synthetic_rich_movie()).expect("the rich seed renders");
    assert_eq!((img.width(), img.height()), (256, 128));
    let drawn = img
        .to_rgba8()
        .pixels()
        .filter(|p| p.0 != [255, 255, 255, 255])
        .count();
    assert!(drawn > 256 * 128 / 10, "frame 2 is drawn ({drawn} px)");
}

/// DefineBitsLossless2 stores its colours already multiplied by alpha: a half-transparent red
/// is `80 00 00 80`, drawn as it is. Multiplied again it came out half as bright.
#[test]
fn a_lossless2_picture_is_not_premultiplied_twice() {
    let p = rich_bitmap(14).expect("ARGB picture");
    assert_eq!(premul(&p, 0, 0), [0, 0, 255, 255]);
    assert_eq!(premul(&p, 1, 0), [128, 0, 0, 128]);
}

/// Every picture draws on one pixel budget, charged before decoding: a 2 x 1 picture is
/// refused with 1 pixel left and takes 2 of 2. Without a total, a movie of many pictures each
/// under the per-picture cap held all of them at once.
#[test]
fn every_picture_draws_on_one_pixel_budget() {
    let bytes = synthetic_rich_movie();
    let buf = swf::decompress_swf(&bytes[..]).expect("the rich seed inflates");
    let movie = swf::parse_swf(&buf).expect("the rich seed parses");
    let defs = scene::Defs::collect(&movie.tags);
    let Some(scene::Def::Bits(b)) = defs.by_id.get(&14).copied() else {
        panic!("character 14 is the 2x1 ARGB picture");
    };
    let mut left = 1;
    assert!(paint::decode_bits(b, None, &mut left).is_none());
    assert_eq!(left, 1, "a refused picture takes nothing");
    let mut left = 2;
    assert!(paint::decode_bits(b, None, &mut left).is_some());
    assert_eq!(left, 0);
}

/// A version-2 palette entry is premultiplied too, and an index past the palette is a
/// transparent pixel, not a refusal of the whole picture.
#[test]
fn a_palette_index_past_the_table_draws_transparent() {
    let p = rich_bitmap(12).expect("RGBA palette picture");
    assert_eq!(premul(&p, 0, 0), [0, 255, 0, 255]);
    assert_eq!(premul(&p, 1, 0), [128, 0, 0, 128]);
    assert_eq!(premul(&p, 0, 1), [0, 0, 0, 0]);
}

/// A JPEG3's colours are stored already multiplied by its mask; Flash clamps each to the alpha.
/// The ramp's blue is 90 under an alpha of 200: kept near 90, where multiplying again made it 70.
#[test]
fn a_jpeg3_colour_is_clamped_to_its_alpha_not_multiplied() {
    let p = rich_bitmap(17).expect("JPEG with an alpha mask");
    let [r, g, b, a] = premul(&p, 0, 0);
    assert_eq!(a, 200);
    assert!((86..=94).contains(&b), "blue {b} kept, not multiplied");
    assert!(r <= a && g <= a);
}

/// Since SWF 8 a JPEG tag may hold a PNG, whose alpha is straight and is multiplied in here.
#[test]
fn a_png_inside_a_jpeg_tag_decodes() {
    let p = rich_bitmap(18).expect("PNG in a DefineBitsJPEG2");
    assert_eq!(premul(&p, 0, 0), [128, 0, 0, 128]);
    assert_eq!(premul(&p, 1, 0), [0, 0, 255, 255]);
}

/// Every picture in the reporter's movie decodes, its two photos (a tables stream and a
/// picture stream in one tag) as well as the small one that leans on the file's JPEGTables.
#[test]
fn the_reporters_pictures_all_decode() {
    let Some(bytes) = st2k_base::testcorpus::read("real.swf") else {
        return;
    };
    let buf = swf::decompress_swf(&bytes[..]).expect("the reporter's movie inflates");
    let movie = swf::parse_swf(&buf).expect("the reporter's movie parses");
    let defs = scene::Defs::collect(&movie.tags);
    let mut sizes: Vec<(u16, Option<(u32, u32)>)> = defs
        .by_id
        .iter()
        .filter_map(|(id, d)| match d {
            scene::Def::Bits(b) => Some((
                *id,
                decode_unbudgeted(*b, defs.jpeg_tables).map(|p| (p.width(), p.height())),
            )),
            _ => None,
        })
        .collect();
    sizes.sort();
    assert_eq!(sizes.len(), 3, "three pictures: {sizes:?}");
    assert!(
        sizes.iter().all(|(_, s)| s.is_some()),
        "every picture decodes: {sizes:?}"
    );
}

/// The reporter's own movie (issue #70), when the corpus has it. CWS v7, 413x404 px. Frame 1
/// is a preloader on #FFFFCC: two device-font text fields (not drawn) and a faint dotted strip
/// of about 265 px. Frame 2 shows the movie's pictures, and that is the thumbnail.
#[test]
fn the_reporters_movie_shows_its_pictures_not_the_preloader() {
    let Some(bytes) = st2k_base::testcorpus::read("real.swf") else {
        return;
    };
    let img = render_swf(&bytes).expect("the reporter's movie renders");
    assert_eq!((img.width(), img.height()), (413, 404));
    let rgba = img.to_rgba8();
    let drawn = rgba
        .pixels()
        .filter(|p| p.0 != [255, 255, 204, 255])
        .count();
    let share = drawn as f64 / (413.0 * 404.0);
    assert!(
        share >= CONTENT_COVER,
        "frame 2's pictures, not the preloader's strip ({drawn} px)"
    );
}
