use super::*;
use crate::isobmff::testutil::{bx, infe};

const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/avif/");

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!("{FIX}{name}")).unwrap_or_else(|e| panic!("fixture {name}: {e}"))
}

/// Mean of the middle half of quarter `q` (0 top-left .. 3 bottom-right), per channel.
fn patch(img: &image::RgbaImage, q: u32) -> [f32; 3] {
    let (qw, qh) = (img.width() / 2, img.height() / 2);
    let (x0, y0) = ((q % 2) * qw + qw / 4, (q / 2) * qh + qh / 4);
    let mut sum = [0f32; 3];
    let mut n = 0f32;
    for y in y0..y0 + qh / 2 {
        for x in x0..x0 + qw / 2 {
            let p = img.get_pixel(x, y).0;
            for c in 0..3 {
                sum[c] += f32::from(p[c]);
            }
            n += 1.0;
        }
    }
    sum.map(|s| s / n)
}

/// The reference patches: 32x32 lossless 4:4:4 pictures of four flat 16x16 patches, one per
/// colour signalling (depth, matrix, range, a missing `colr`, monochrome, PQ HDR), generated
/// and checked against libdav1d by `scripts/make-avif-patches.py`. What each must look like:
/// red, green, mid grey and a skin tone (the skin tone moves first under a wrong matrix, the
/// grey under a wrong range or transfer); the monochrome one four greys; the PQ one the same
/// scene with grey at half of reference white.
const COLOUR: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [128, 128, 128], [222, 178, 145]];
const MONO: [[u8; 3]; 4] = [[32, 32, 32], [96, 96, 96], [160, 160, 160], [224, 224, 224]];
const PQ: [[u8; 3]; 4] = [[255, 0, 0], [0, 255, 0], [188, 188, 188], [222, 178, 145]];

#[test]
fn every_colour_signalling_decodes_to_its_reference_patches() {
    let cases: [(&str, &[[u8; 3]; 4]); 8] = [
        ("patches-8bit-bt709.avif", &COLOUR),
        ("patches-8bit-bt601.avif", &COLOUR),
        ("patches-8bit-nocolr.avif", &COLOUR),
        ("patches-10bit-bt709.avif", &COLOUR),
        ("patches-10bit-bt601.avif", &COLOUR),
        ("patches-10bit-nocolr.avif", &COLOUR),
        ("patches-10bit-mono.avif", &MONO),
        ("patches-10bit-pq2020.avif", &PQ),
    ];
    for (name, want) in cases {
        let img = decode_avif(&fixture(name), 1, None).unwrap_or_else(|e| panic!("{name}: {e}"));
        let rgba = img.to_rgba8();
        assert_eq!(rgba.dimensions(), (32, 32), "{name}");
        for (q, want) in want.iter().enumerate() {
            let got = patch(&rgba, q as u32);
            let worst = got
                .iter()
                .zip(want)
                .map(|(g, w)| (g - f32::from(*w)).abs())
                .fold(0.0, f32::max);
            assert!(worst <= 4.0, "{name} patch {q}: {got:?}, want {want:?}");
        }
    }
}

#[test]
fn an_alpha_item_becomes_the_alpha_channel() {
    let img = decode_avif(&fixture("alpha-half.avif"), 1, None)
        .unwrap()
        .to_rgba8();
    let opaque = img.get_pixel(4, 16).0;
    let clear = img.get_pixel(28, 16).0;
    assert!(
        opaque[3] >= 250 && opaque[0] >= 240,
        "left half: {opaque:?}"
    );
    assert!(clear[3] <= 5, "right half: {clear:?}");
}

/// One item of a synthetic AVIF: its type, bytes (in the file, or in `idat` when `inline`) and
/// its property boxes.
struct Item {
    id: u16,
    kind: [u8; 4],
    data: Vec<u8>,
    inline: bool,
    props: Vec<Vec<u8>>,
}

/// An AVIF around `items`: ftyp, meta{hdlr, pitm, iinf, iloc, iprp, iref, idat}, then the file
/// data the file-located items point into.
fn build(items: &[Item], primary: u16, refs: &[(&[u8; 4], u16, &[u16])]) -> Vec<u8> {
    let ftyp = bx(b"ftyp", b"avif\0\0\0\0avifmif1miaf");
    let mut hdlr = vec![0u8; 8];
    hdlr.extend_from_slice(b"pict");
    hdlr.extend_from_slice(&[0u8; 13]);
    let mut pitm = vec![0u8; 4];
    pitm.extend_from_slice(&primary.to_be_bytes());
    let mut iinf = vec![0u8; 4];
    iinf.extend_from_slice(&(items.len() as u16).to_be_bytes());
    for it in items {
        iinf.extend_from_slice(&infe(it.id, &it.kind, None));
    }
    let (mut ipco, mut ipma) = (Vec::new(), vec![0u8; 4]);
    ipma.extend_from_slice(&(items.len() as u32).to_be_bytes());
    let mut index = 0u8;
    for it in items {
        ipma.extend_from_slice(&it.id.to_be_bytes());
        ipma.push(it.props.len() as u8);
        for p in &it.props {
            ipco.extend_from_slice(p);
            index += 1;
            ipma.push(index);
        }
    }
    let iprp = [bx(b"ipco", &ipco), bx(b"ipma", &ipma)].concat();
    let mut iref = vec![0u8; 4];
    for (kind, from, to) in refs {
        let mut r = from.to_be_bytes().to_vec();
        r.extend_from_slice(&(to.len() as u16).to_be_bytes());
        for t in *to {
            r.extend_from_slice(&t.to_be_bytes());
        }
        iref.extend_from_slice(&bx(kind, &r));
    }
    let idat: Vec<u8> = items
        .iter()
        .filter(|i| i.inline)
        .flat_map(|i| i.data.clone())
        .collect();
    // iloc version 1: 4-byte offsets and lengths, no base offset, no index.
    let iloc = |file_base: usize| {
        let mut b = vec![1u8, 0, 0, 0, 0x44, 0x00];
        b.extend_from_slice(&(items.len() as u16).to_be_bytes());
        let (mut in_file, mut in_idat) = (file_base, 0usize);
        for it in items {
            b.extend_from_slice(&it.id.to_be_bytes());
            b.extend_from_slice(&u16::from(it.inline).to_be_bytes());
            b.extend_from_slice(&0u16.to_be_bytes());
            b.extend_from_slice(&1u16.to_be_bytes());
            let cursor = if it.inline {
                &mut in_idat
            } else {
                &mut in_file
            };
            b.extend_from_slice(&(*cursor as u32).to_be_bytes());
            b.extend_from_slice(&(it.data.len() as u32).to_be_bytes());
            *cursor += it.data.len();
        }
        bx(b"iloc", &b)
    };
    let meta = |file_base: usize| {
        let mut m = vec![0u8; 4];
        for part in [
            bx(b"hdlr", &hdlr),
            bx(b"pitm", &pitm),
            bx(b"iinf", &iinf),
            iloc(file_base),
            bx(b"iprp", &iprp),
            bx(b"iref", &iref),
            bx(b"idat", &idat),
        ] {
            m.extend_from_slice(&part);
        }
        bx(b"meta", &m)
    };
    let mdat: Vec<u8> = items
        .iter()
        .filter(|i| !i.inline)
        .flat_map(|i| i.data.clone())
        .collect();
    // The file data sits in `mdat`, after its 8-byte header.
    let base = ftyp.len() + meta(0).len() + 8;
    [ftyp, meta(base), bx(b"mdat", &mdat)].concat()
}

/// A fixture's primary AV1 bytes and the property boxes of type `keep` it has.
fn item_of(name: &str, keep: &[&[u8; 4]]) -> (Vec<u8>, Vec<Vec<u8>>) {
    let src = fixture(name);
    let f = Avif::parse(&src).unwrap();
    let id = f.primary;
    let props = keep
        .iter()
        .filter_map(|t| f.property(id, t).map(|body| bx(t, body)))
        .collect();
    (f.data(id).unwrap().to_vec(), props)
}

/// The 8-bit BT.709 patch picture's AV1 bytes and its `av1C`/`ispe`/`colr` properties.
fn tile() -> (Vec<u8>, Vec<Vec<u8>>) {
    item_of("patches-8bit-bt709.avif", &[b"av1C", b"ispe", b"colr"])
}

fn single_with(extra: Vec<Vec<u8>>) -> Vec<u8> {
    let (data, mut props) = tile();
    props.extend(extra);
    build(
        &[Item {
            id: 1,
            kind: *b"av01",
            data,
            inline: false,
            props,
        }],
        1,
        &[],
    )
}

#[test]
fn a_grid_places_its_tiles_left_to_right_top_to_bottom_and_crops_to_its_canvas() {
    let (data, props) = tile();
    let tile_img = decode_avif(&single_with(Vec::new()), 1, None)
        .unwrap()
        .to_rgba8();
    for (w, h) in [(64u16, 64u16), (60, 50)] {
        let mut items: Vec<Item> = (2..=5)
            .map(|id| Item {
                id,
                kind: *b"av01",
                data: data.clone(),
                inline: false,
                props: props.clone(),
            })
            .collect();
        let mut desc = vec![0u8, 0, 1, 1];
        desc.extend_from_slice(&w.to_be_bytes());
        desc.extend_from_slice(&h.to_be_bytes());
        let mut ispe = vec![0u8; 4];
        ispe.extend_from_slice(&u32::from(w).to_be_bytes());
        ispe.extend_from_slice(&u32::from(h).to_be_bytes());
        items.insert(
            0,
            Item {
                id: 1,
                kind: *b"grid",
                data: desc,
                inline: true,
                props: vec![bx(b"ispe", &ispe)],
            },
        );
        let file = build(&items, 1, &[(b"dimg", 1, &[2, 3, 4, 5])]);
        let img = decode_avif(&file, 1, None).unwrap().to_rgba8();
        assert_eq!(img.dimensions(), (u32::from(w), u32::from(h)));
        for (x, y, p) in img.enumerate_pixels() {
            assert_eq!(
                p,
                tile_img.get_pixel(x % 32, y % 32),
                "grid {w}x{h} pixel {x},{y}"
            );
        }
    }
}

/// A thumbnail-sized decode (`max_edge`) is the full picture sampled at the reduction: the grid's
/// tiles land at their reduced offsets, the crop scales with them and the alpha still applies.
/// The patches are flat 16x16 squares, so every 4x4 block is one colour and must match exactly.
#[test]
fn a_reduced_decode_is_the_full_picture_at_the_reduction_tiles_crop_and_alpha_included() {
    let (data, props) = tile();
    let mut items: Vec<Item> = (2..=5)
        .map(|id| Item {
            id,
            kind: *b"av01",
            data: data.clone(),
            inline: false,
            props: props.clone(),
        })
        .collect();
    let desc = [0u8, 0, 1, 1, 0, 64, 0, 64].to_vec();
    let ispe = [0u8, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 64];
    items.insert(
        0,
        Item {
            id: 1,
            kind: *b"grid",
            data: desc,
            inline: true,
            props: vec![bx(b"ispe", &ispe)],
        },
    );
    let file = build(&items, 1, &[(b"dimg", 1, &[2, 3, 4, 5])]);
    let full = decode_avif(&file, 1, None).unwrap().to_rgba8();
    // 64 px wanted at 20: reduced by 2 would leave 32, by 4 leaves 16, under the edge.
    assert_eq!(decode_avif(&file, 1, Some(20)).unwrap().width(), 32);
    let small = decode_avif(&file, 1, Some(16)).unwrap().to_rgba8();
    assert_eq!(small.dimensions(), (16, 16));
    for (x, y, p) in small.enumerate_pixels() {
        assert_eq!(p, full.get_pixel(x * 4, y * 4), "grid pixel {x},{y}");
    }
    let clap: Vec<u8> = [16i32, 1, 16, 1, 0, 1, 0, 1]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect();
    let plain = decode_avif(&single_with(Vec::new()), 1, Some(8)).unwrap();
    let cropped = decode_avif(&single_with(vec![bx(b"clap", &clap)]), 1, Some(8)).unwrap();
    assert_eq!(
        cropped.to_rgba8(),
        plain.crop_imm(2, 2, 4, 4).to_rgba8(),
        "the crop, reduced by 4"
    );
    let alpha = decode_avif(&fixture("alpha-half.avif"), 1, Some(8))
        .unwrap()
        .to_rgba8();
    assert_eq!(alpha.dimensions(), (8, 8));
    assert!(
        alpha.get_pixel(1, 4).0[3] >= 250 && alpha.get_pixel(7, 4).0[3] <= 5,
        "alpha, reduced"
    );
}

#[test]
fn rotation_mirror_and_crop_are_applied_in_heif_order() {
    let plain = decode_avif(&single_with(Vec::new()), 1, None).unwrap();
    let with = |p: Vec<u8>| {
        decode_avif(&single_with(vec![p]), 1, None)
            .unwrap()
            .to_rgba8()
    };
    assert_eq!(
        with(bx(b"irot", &[1])),
        plain.rotate270().to_rgba8(),
        "irot 1 = a quarter turn anticlockwise"
    );
    assert_eq!(
        with(bx(b"imir", &[0])),
        plain.flipv().to_rgba8(),
        "imir 0 exchanges top and bottom"
    );
    assert_eq!(
        with(bx(b"imir", &[1])),
        plain.fliph().to_rgba8(),
        "imir 1 exchanges left and right"
    );
    // A centred 16x16 clean aperture: widthN/D, heightN/D, horizOffN/D, vertOffN/D.
    let clap: Vec<u8> = [16i32, 1, 16, 1, 0, 1, 0, 1]
        .iter()
        .flat_map(|v| v.to_be_bytes())
        .collect();
    assert_eq!(
        with(bx(b"clap", &clap)),
        plain.crop_imm(8, 8, 16, 16).to_rgba8()
    );
}

#[test]
fn a_file_that_is_not_an_avif_picture_is_an_error_not_a_panic() {
    assert!(decode_avif(b"", 1, None).is_err());
    let mut file = single_with(Vec::new());
    let len = file.len();
    file.truncate(len - 20);
    assert!(decode_avif(&file, 1, None).is_err(), "a truncated picture");
}

/// With no `colr` at all, the AV1 stream's own colour description is the picture's: the PQ
/// patches, stripped of their `nclx`, still come out through the HDR path. The fallback this
/// takes is also what fills an `nclx` that says "unspecified" (the PQ fixture's own 2/2/9).
#[test]
fn a_picture_without_a_colour_box_takes_its_colour_from_the_av1_stream() {
    let (data, props) = item_of("patches-10bit-pq2020.avif", &[b"av1C", b"ispe"]);
    let file = build(
        &[Item {
            id: 1,
            kind: *b"av01",
            data,
            inline: false,
            props,
        }],
        1,
        &[],
    );
    let img = decode_avif(&file, 1, None).unwrap().to_rgba8();
    let grey = patch(&img, 2);
    assert!(
        grey.iter().all(|&c| (c - 188.0).abs() <= 4.0),
        "half of reference white through the HDR path is 188, the raw signal is not: {grey:?}"
    );
}
