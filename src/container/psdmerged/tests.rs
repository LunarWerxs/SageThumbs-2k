use super::*;
use std::io::{Cursor, Write};

/// A Photoshop document whose composite is `px(x, y)` (one value a channel), for the tests and
/// the fuzz seed. `packed` picks PackBits over raw; `real` is the version-info flag. It has no
/// layers.
pub(crate) fn synth(
    (w, h): (u32, u32),
    (mode, channels, depth): (u16, u16, u16),
    psb: bool,
    packed: bool,
    real: bool,
    px: impl Fn(u32, u32, u16) -> u16,
) -> Vec<u8> {
    let mut f = head_and_resources((w, h), (mode, channels, depth), psb, real);
    // An empty Layer and Mask section.
    f.extend_from_slice(&vec![0u8; if psb { 8 } else { 4 }]);
    f.extend_from_slice(&u16::from(packed).to_be_bytes());
    let rows: Vec<Vec<u8>> = (0..channels)
        .flat_map(|c| (0..h).map(move |y| (c, y)))
        .map(|(c, y)| sample_row(w, depth, |x| px(x, y, c)))
        .collect();
    if !packed {
        rows.iter().for_each(|r| f.extend_from_slice(r));
        return f;
    }
    let packed_rows: Vec<Vec<u8>> = rows.iter().map(|r| pack(r)).collect();
    for r in &packed_rows {
        push_count(&mut f, r.len(), psb);
    }
    packed_rows.iter().for_each(|r| f.extend_from_slice(r));
    f
}

/// The header, an empty Color Mode Data section, and one version-info resource carrying the
/// composite flag.
fn head_and_resources(
    (w, h): (u32, u32),
    (mode, channels, depth): (u16, u16, u16),
    psb: bool,
    real: bool,
) -> Vec<u8> {
    let mut f = Vec::new();
    f.extend_from_slice(b"8BPS");
    f.extend_from_slice(&(1 + u16::from(psb)).to_be_bytes());
    f.extend_from_slice(&[0u8; 6]);
    f.extend_from_slice(&channels.to_be_bytes());
    f.extend_from_slice(&h.to_be_bytes());
    f.extend_from_slice(&w.to_be_bytes());
    f.extend_from_slice(&depth.to_be_bytes());
    f.extend_from_slice(&mode.to_be_bytes());
    f.extend_from_slice(&0u32.to_be_bytes()); // colour mode data
    let info = [0, 0, 0, 1, u8::from(real), 0];
    let mut res = b"8BIM".to_vec();
    res.extend_from_slice(&VERSION_INFO.to_be_bytes());
    res.extend_from_slice(&[0, 0]);
    res.extend_from_slice(&(info.len() as u32).to_be_bytes());
    res.extend_from_slice(&info);
    f.extend_from_slice(&(res.len() as u32).to_be_bytes());
    f.extend_from_slice(&res);
    f
}

/// One row of `w` samples at `depth` bits, big-endian.
fn sample_row(w: u32, depth: u16, v: impl Fn(u32) -> u16) -> Vec<u8> {
    (0..w)
        .flat_map(|x| match depth {
            16 => v(x).to_be_bytes().to_vec(),
            32 => (f32::from(v(x)) / 255.0).to_be_bytes().to_vec(),
            _ => vec![v(x) as u8],
        })
        .collect()
}

fn push_count(f: &mut Vec<u8>, n: usize, psb: bool) {
    if psb {
        f.extend_from_slice(&(n as u32).to_be_bytes());
    } else {
        f.extend_from_slice(&(n as u16).to_be_bytes());
    }
}

/// PackBits, as Photoshop writes it: a run of one repeated byte as a repeat, anything else as
/// literals, 128 bytes at most per control.
fn pack(row: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in row.chunks(128) {
        if chunk.len() > 1 && chunk.iter().all(|&b| b == chunk[0]) {
            out.push((1 - chunk.len() as i16) as i8 as u8);
            out.push(chunk[0]);
        } else {
            out.push(chunk.len() as u8 - 1);
            out.extend_from_slice(chunk);
        }
    }
    out
}

fn decode(bytes: &[u8], edge: u32) -> Option<RgbaImage> {
    from_reader(Cursor::new(bytes), edge).map(|i| i.to_rgba8())
}

/// Red follows the column, green the row, blue is flat: a shrink that takes the wrong rows
/// or columns shows up as the wrong value at a known place.
fn ramp(x: u32, y: u32, c: u16) -> u16 {
    match c {
        0 => (x % 256) as u16,
        1 => (y % 256) as u16,
        _ => 200,
    }
}

fn mean_diff(a: &RgbaImage, b: &RgbaImage) -> f64 {
    let diff: u64 = a
        .as_raw()
        .iter()
        .zip(b.as_raw())
        .map(|(&x, &y)| u64::from(x.abs_diff(y)))
        .sum();
    diff as f64 / a.as_raw().len() as f64
}

#[test]
fn a_packbits_rgb_composite_reads_at_full_size() {
    for psb in [false, true] {
        let f = synth((40, 30), (3, 3, 8), psb, true, true, ramp);
        let img = decode(&f, 4096).expect("composite");
        assert_eq!(img.dimensions(), (40, 30));
        assert_eq!(img.get_pixel(7, 11).0, [7, 11, 200, 255], "psb={psb}");
    }
}

#[test]
fn a_big_document_is_shrunk_from_the_rows_it_samples() {
    let f = synth((300, 200), (3, 3, 8), false, true, true, ramp);
    let img = decode(&f, 100).expect("composite");
    assert_eq!(img.dimensions(), (100, 67));
    // Cell (10, 5) averages columns 30..33 and takes row 5 * 3 + 1.
    assert_eq!(img.get_pixel(10, 5).0, [31, 16, 200, 255]);
}

#[test]
fn raw_sixteen_bit_grey_keeps_the_high_byte() {
    let f = synth((20, 10), (1, 1, 16), true, false, true, |x, _, _| {
        (x as u16) << 8 | 0xAB
    });
    let img = decode(&f, 4096).expect("composite");
    assert_eq!(img.get_pixel(9, 3).0, [9, 9, 9, 255]);
}

#[test]
fn thirty_two_bit_linear_light_goes_through_the_srgb_curve() {
    // `sample_row` stores v / 255 as the float: 0.5 linear is 188 in sRGB, 1.0 stays 255.
    let f = synth((6, 4), (3, 3, 32), false, false, true, |_, _, c| {
        [255, 128, 0][usize::from(c)]
    });
    let px = decode(&f, 64).expect("32-bit composite").get_pixel(2, 2).0;
    assert_eq!(px, [255, 188, 0, 255]);
}

#[test]
fn cmyk_inks_come_out_as_rgb() {
    // Stored inverted: C at half, M and Y none, K none -> half-red cyan-ish (128, 255, 255).
    let f = synth((8, 8), (4, 4, 8), false, true, true, |_, _, c| {
        [128, 255, 255, 255][usize::from(c)]
    });
    assert_eq!(
        decode(&f, 64).unwrap().get_pixel(3, 3).0,
        [128, 255, 255, 255]
    );
    // Half black scales all three.
    let f = synth((8, 8), (4, 4, 8), false, true, true, |_, _, c| {
        [255, 255, 255, 128][usize::from(c)]
    });
    assert_eq!(
        decode(&f, 64).unwrap().get_pixel(3, 3).0,
        [128, 128, 128, 255]
    );
}

#[test]
fn lab_is_converted_from_d50() {
    // Photoshop's L=255, a=b=128 is white, and L=0 black, whatever the white point.
    for (l, want) in [(255, 255), (0, 0)] {
        let f = synth((4, 4), (9, 3, 8), false, true, true, |_, _, c| {
            [l, 128, 128][usize::from(c)]
        });
        let px = decode(&f, 64).expect("Lab composite").get_pixel(1, 1).0;
        assert!(
            px[..3].iter().all(|&v| v.abs_diff(want) <= 1),
            "L={l}: {px:?}"
        );
    }
}

#[test]
fn a_bitmap_document_is_black_where_its_bits_are_set() {
    // 1-bit rows, eight pixels a byte: 0xF0 is four black pixels then four white ones.
    let mut f = head_and_resources((8, 2), (0, 1, 1), false, true);
    f.extend_from_slice(&[0u8; 4]);
    f.extend_from_slice(&0u16.to_be_bytes());
    f.extend_from_slice(&[0xF0, 0xF0]);
    let img = decode(&f, 64).expect("bitmap composite");
    assert_eq!(img.get_pixel(0, 0).0, [0, 0, 0, 255]);
    assert_eq!(img.get_pixel(7, 1).0, [255, 255, 255, 255]);
}

#[test]
fn transparency_is_unblended_from_white() {
    // Red at half alpha, blended over white the way Photoshop stores it: 255, 128, 128.
    let f = synth((6, 6), (3, 4, 8), false, true, true, |_, _, c| {
        [255, 128, 128, 128][usize::from(c)]
    });
    let px = decode(&f, 64).unwrap().get_pixel(2, 2).0;
    assert_eq!(px[3], 128);
    assert!(px[0] == 255 && px[1] <= 1 && px[2] <= 1, "{px:?}");
}

#[test]
fn what_this_does_not_read_is_declined() {
    let ok = |mode, ch, depth, packed, real| {
        decode(
            &synth((8, 8), (mode, ch, depth), false, packed, real, ramp),
            64,
        )
        .is_some()
    };
    assert!(ok(3, 3, 8, true, true));
    assert!(
        ok(3, 3, 8, true, false),
        "no layers: the composite is the only picture, whatever the flag says"
    );
    assert!(!ok(7, 3, 8, true, true), "Multichannel");
    assert!(!ok(2, 1, 8, true, true), "Indexed with no colour table");
    assert!(!ok(9, 3, 32, false, true), "32-bit Lab does not exist");
    assert!(!ok(0, 1, 8, true, true), "an 8-bit Bitmap does not exist");
    assert!(
        !ok(3, 2, 8, true, true),
        "fewer channels than the mode needs"
    );
    let mut zip = synth((8, 8), (3, 3, 8), false, false, true, ramp);
    let at = zip.len() - 8 * 8 * 3 - 2;
    zip[at + 1] = 2;
    assert!(decode(&zip, 64).is_none(), "ZIP");
}

#[test]
fn a_truncated_or_lying_file_is_refused() {
    let f = synth((16, 16), (3, 3, 8), false, true, true, ramp);
    for cut in [10, 40, f.len() / 2, f.len() - 1] {
        assert!(decode(&f[..cut], 64).is_none(), "cut at {cut}");
    }
    // A row length no PackBits row of this width can have.
    let mut lie = f.clone();
    let first = find_table(&f);
    lie[first] = 0xFF;
    lie[first + 1] = 0xFF;
    assert!(decode(&lie, 64).is_none());
}

/// The first row-length entry of a PSD built by [`synth`] with no layers.
fn find_table(f: &[u8]) -> usize {
    let res = u32::from_be_bytes(f[30..34].try_into().unwrap()) as usize;
    30 + 4 + res + 4 + 2
}

/// Every Photoshop-written variant in the corpus, against ImageMagick's reading of the same
/// file, which is what the Quick preview showed before. Lab is the one allowed to differ by
/// more: ImageMagick reads Photoshop's D50 Lab as D65 (see `lab_pixel`). Left out: the 32-bit
/// pair (below) and `real.psd`, a 10x12 16-bit CMYK test file with no layers whose own flag
/// disowns its composite and whose baked preview is blank white, which ImageMagick draws with
/// its black channel the other way up from every 8-bit CMYK document here.
#[test]
fn real_documents_agree_with_imagemagick() {
    const READ: [(&str, f64); 19] = [
        ("real-flat.psd", 1.5),
        ("real-flat.psb", 1.5),
        ("real-grey.psd", 1.5),
        ("real-grey.psb", 1.5),
        ("real-cmyk.psd", 1.5),
        ("real-cmyk.psb", 1.5),
        ("real-16bit.psd", 1.5),
        ("real-16bit.psb", 1.5),
        ("real-rgb-layers.psd", 1.5),
        ("real-rgb-layers.psb", 1.5),
        ("real-nocomposite.psd", 1.5),
        ("real-nocomposite.psb", 1.5),
        ("real-indexed.psb", 1.5),
        ("real-lab.psd", 3.0),
        ("real-lab.psb", 3.0),
        ("real.psb", 1.5),
        ("real.pdd", 1.5),
        ("sample.psd", 1.5),
        ("sample.psb", 1.5),
    ];
    for (name, tolerance) in READ {
        let Some(bytes) = st2k_base::testcorpus::read(name) else {
            eprintln!("NOT MEASURED: {name} absent");
            continue;
        };
        let ours = decode(&bytes, 4096).unwrap_or_else(|| panic!("{name}"));
        let Ok(theirs) = crate::decode::psd_composite_via_magick(&bytes) else {
            eprintln!("NOT MEASURED: no ImageMagick for {name}");
            continue;
        };
        let theirs = theirs.to_rgba8();
        assert_eq!(ours.dimensions(), theirs.dimensions(), "{name}");
        let mean = mean_diff(&ours, &theirs);
        assert!(mean < tolerance, "{name}: mean difference {mean:.2}");
    }
}

/// A 32-bit document against Photoshop's own baked preview instead: ImageMagick writes its
/// linear light out without the sRGB curve (182, 5, 5 for the corpus's red, where Photoshop's
/// preview and the 8-bit variants show 220, 40, 40). The preview is a small JPEG, hence the
/// looser tolerance.
#[test]
fn thirty_two_bit_documents_agree_with_photoshops_preview() {
    for name in ["real-32bit.psd", "real-32bit.psb"] {
        let Some(bytes) = st2k_base::testcorpus::read(name) else {
            eprintln!("NOT MEASURED: {name} absent");
            continue;
        };
        let ours = decode(&bytes, 4096).unwrap_or_else(|| panic!("{name}"));
        let preview = crate::container::psd_baked_preview(&bytes)
            .and_then(|jpeg| image::load_from_memory(&jpeg).ok())
            .unwrap_or_else(|| panic!("{name}: no baked preview"));
        let (w, h) = ours.dimensions();
        let preview = image::imageops::resize(
            &preview.to_rgba8(),
            w,
            h,
            image::imageops::FilterType::Triangle,
        );
        let mean = mean_diff(&ours, &preview);
        assert!(mean < 6.0, "{name}: mean difference {mean:.2}");
    }
}

/// The layers of every corpus document that keeps them beside a real composite, flattened,
/// against that composite: Photoshop's own answer to what the layers look like together.
#[test]
fn flattened_layers_match_photoshops_own_composite() {
    const LAYERED: [&str; 10] = [
        "real-rgb-layers.psd",
        "real-rgb-layers.psb",
        "real-cmyk.psd",
        "real-cmyk.psb",
        "real-grey.psd",
        "real-grey.psb",
        "real-lab.psd",
        "real-lab.psb",
        "real-16bit.psd",
        "real-16bit.psb",
    ];
    for name in LAYERED {
        let Some(bytes) = st2k_base::testcorpus::read(name) else {
            eprintln!("NOT MEASURED: {name} absent");
            continue;
        };
        let flat = layers::flatten_any(Cursor::new(&bytes), 4096)
            .unwrap_or_else(|| panic!("{name}: no layers flattened"))
            .to_rgba8();
        let stored = decode(&bytes, 4096).unwrap_or_else(|| panic!("{name}"));
        assert_eq!(flat.dimensions(), stored.dimensions(), "{name}");
        let mean = mean_diff(&flat, &stored);
        assert!(mean < 1.5, "{name}: mean difference {mean:.2}");
    }
}

/// A test layer: a solid colour over `rect`, drawn at `opacity`, optionally transparent,
/// hidden, clipped, masked, or a group marker.
#[derive(Clone)]
struct TestLayer {
    rect: [i32; 4],
    colour: [u8; 3],
    alpha: Option<u8>,
    opacity: u8,
    hidden: bool,
    clipped: bool,
    /// `(rect, outside value, inside value)`.
    mask: Option<([i32; 4], u8, u8)>,
    section: u32,
}

fn solid(rect: [i32; 4], colour: [u8; 3]) -> TestLayer {
    TestLayer {
        rect,
        colour,
        alpha: None,
        opacity: 255,
        hidden: false,
        clipped: false,
        mask: None,
        section: 0,
    }
}

fn marker(section: u32, hidden: bool) -> TestLayer {
    TestLayer {
        section,
        hidden,
        ..solid([0, 0, 0, 0], [0; 3])
    }
}

/// One channel's data at `compression` (0 raw, 1 PackBits, 2 ZIP, 3 ZIP with prediction):
/// `rows` rows of `row` stored bytes each, all `value`.
fn channel_data(
    rows: usize,
    width: usize,
    value: u8,
    (depth, compression, psb): (u16, u16, bool),
) -> Vec<u8> {
    // A 16-bit sample carries the byte in both halves, as 255 * 257 is white.
    let v = if depth == 16 {
        u16::from(value) * 257
    } else {
        u16::from(value)
    };
    let row = sample_row(width as u32, depth, |_| v);
    let mut out = compression.to_be_bytes().to_vec();
    match compression {
        0 => (0..rows).for_each(|_| out.extend_from_slice(&row)),
        1 => {
            let packed = pack(&row);
            (0..rows).for_each(|_| push_count(&mut out, packed.len(), psb));
            (0..rows).for_each(|_| out.extend_from_slice(&packed));
        }
        _ => {
            let stored = if compression == 3 {
                predict(&row, depth)
            } else {
                row
            };
            let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
            (0..rows).for_each(|_| z.write_all(&stored).unwrap());
            out.extend_from_slice(&z.finish().unwrap());
        }
    }
    out
}

/// Photoshop's prediction, the inverse of `layers::unpredict`.
fn predict(row: &[u8], depth: u16) -> Vec<u8> {
    let mut row = row.to_vec();
    if depth == 32 {
        let w = row.len() / 4;
        let planes: Vec<u8> = (0..4)
            .flat_map(|k| (0..w).map(move |x| (k, x)))
            .map(|(k, x)| row[x * 4 + k])
            .collect();
        row = planes;
    }
    if depth == 16 {
        let words: Vec<u16> = row
            .as_chunks::<2>()
            .0
            .iter()
            .map(|s| u16::from_be_bytes(*s))
            .collect();
        let mut out = Vec::new();
        for (i, w) in words.iter().enumerate() {
            let prev = if i == 0 { 0 } else { words[i - 1] };
            out.extend_from_slice(&w.wrapping_sub(prev).to_be_bytes());
        }
        return out;
    }
    let mut out = row.clone();
    for i in (1..row.len()).rev() {
        out[i] = row[i].wrapping_sub(row[i - 1]);
    }
    out
}

/// A length field: eight bytes in a PSB, four in a PSD.
fn put_len(f: &mut Vec<u8>, n: usize, psb: bool) {
    if psb {
        f.extend_from_slice(&(n as u64).to_be_bytes());
    } else {
        f.extend_from_slice(&(n as u32).to_be_bytes());
    }
}

/// One layer's record, its channels' data appended to `data`.
fn layer_record(l: &TestLayer, fmt: (u16, u16, bool), records: &mut Vec<u8>, data: &mut Vec<u8>) {
    let psb = fmt.2;
    let [t, lf, b, r] = l.rect;
    let (rows, cols) = ((b - t).max(0) as usize, (r - lf).max(0) as usize);
    let mut chans: Vec<(i16, Vec<u8>)> = (0..3)
        .map(|c| (c as i16, channel_data(rows, cols, l.colour[c], fmt)))
        .collect();
    if let Some(a) = l.alpha {
        chans.push((-1, channel_data(rows, cols, a, fmt)));
    }
    if let Some(([mt, ml, mb, mr], _, inside)) = l.mask {
        let (mrows, mcols) = ((mb - mt) as usize, (mr - ml) as usize);
        chans.push((-2, channel_data(mrows, mcols, inside, fmt)));
    }
    r_i32(records, &l.rect);
    records.extend_from_slice(&(chans.len() as u16).to_be_bytes());
    for (id, body) in &chans {
        records.extend_from_slice(&id.to_be_bytes());
        put_len(records, body.len(), psb);
        data.extend_from_slice(body);
    }
    records.extend_from_slice(b"8BIMnorm");
    records.push(l.opacity);
    records.push(u8::from(l.clipped));
    records.push(if l.hidden { 0x0A } else { 0x08 });
    records.push(0);
    let extra = record_extra(l);
    records.extend_from_slice(&(extra.len() as u32).to_be_bytes());
    records.extend_from_slice(&extra);
}

/// An RGB document of `layers` (bottom first) saved without a composite, its channels at
/// `depth` bits in `compression`. The 16- and 32-bit ones keep their layers in an `Lr16` /
/// `Lr32` block, as Photoshop does.
fn layered(
    (w, h): (u32, u32),
    layers: &[TestLayer],
    (depth, compression, psb): (u16, u16, bool),
) -> Vec<u8> {
    let len = |f: &mut Vec<u8>, n: usize| put_len(f, n, psb);
    let mut records = Vec::new();
    let mut data = Vec::new();
    records.extend_from_slice(&(layers.len() as i16).to_be_bytes());
    for l in layers {
        layer_record(l, (depth, compression, psb), &mut records, &mut data);
    }
    let mut info = records;
    info.extend_from_slice(&data);
    let mut section = Vec::new();
    if depth == 8 {
        len(&mut section, info.len());
        section.extend_from_slice(&info);
        section.extend_from_slice(&0u32.to_be_bytes()); // global layer mask
    } else {
        len(&mut section, 0);
        section.extend_from_slice(&0u32.to_be_bytes()); // global layer mask
        section.extend_from_slice(b"8BIM");
        section.extend_from_slice(if depth == 16 { b"Lr16" } else { b"Lr32" });
        len(&mut section, info.len());
        section.extend_from_slice(&info);
    }
    let mut f = head_and_resources((w, h), (3, 3, depth), psb, false);
    len(&mut f, section.len());
    f.extend_from_slice(&section);
    // The white composite Photoshop leaves behind.
    f.extend_from_slice(&0u16.to_be_bytes());
    let white = sample_row(w, depth, |_| 255);
    (0..3 * h).for_each(|_| f.extend_from_slice(&white));
    f
}

fn r_i32(out: &mut Vec<u8>, v: &[i32; 4]) {
    v.iter()
        .for_each(|s| out.extend_from_slice(&s.to_be_bytes()));
}

/// A record's extra data: its mask, empty blending ranges, a name, and the group marker.
fn record_extra(l: &TestLayer) -> Vec<u8> {
    let mut e = Vec::new();
    match l.mask {
        Some((rect, outside, _)) => {
            e.extend_from_slice(&20u32.to_be_bytes());
            r_i32(&mut e, &rect);
            e.extend_from_slice(&[outside, 0, 0, 0]);
        }
        None => e.extend_from_slice(&0u32.to_be_bytes()),
    }
    e.extend_from_slice(&0u32.to_be_bytes());
    e.extend_from_slice(&[1, b'L', 0, 0]);
    if l.section != 0 {
        e.extend_from_slice(b"8BIMlsct");
        e.extend_from_slice(&4u32.to_be_bytes());
        e.extend_from_slice(&l.section.to_be_bytes());
    }
    e
}

/// The fuzz seed: every structure the flatten walks, in a small 16-bit document.
pub(super) fn fuzz_seed_layered() -> Vec<u8> {
    let masked = TestLayer {
        mask: Some(([2, 2, 8, 12], 0, 200)),
        alpha: Some(180),
        ..solid([1, 1, 9, 15], [30, 200, 90])
    };
    let clipped = TestLayer {
        clipped: true,
        opacity: 150,
        ..solid([0, 0, 10, 20], [250, 10, 10])
    };
    layered(
        (20, 10),
        &[
            solid([0, 0, 10, 20], [10, 20, 30]),
            marker(3, false),
            masked,
            clipped,
            marker(1, false),
        ],
        (16, 3, false),
    )
}

fn flat(layers: &[TestLayer]) -> RgbaImage {
    decode(&layered((20, 10), layers, (8, 1, false)), 64).expect("flattened")
}

#[test]
fn a_document_saved_without_its_composite_shows_its_layers() {
    for depth in [8u16, 16, 32] {
        for compression in [0u16, 1, 2, 3] {
            for psb in [false, true] {
                let f = layered(
                    (20, 10),
                    &[
                        solid([0, 0, 10, 20], [200, 40, 40]),
                        solid([2, 5, 8, 15], [250, 250, 250]),
                    ],
                    (depth, compression, psb),
                );
                let img = decode(&f, 64).unwrap_or_else(|| panic!("{depth} {compression}"));
                let at = |x, y| img.get_pixel(x, y).0;
                // A 32-bit sample is linear light, shown through the sRGB curve.
                let shown = |v: u8| {
                    if depth == 32 {
                        float_sample(f32::from(v) / 255.0, true)
                    } else {
                        v
                    }
                };
                let what = format!("depth {depth} compression {compression} psb {psb}");
                assert_eq!(at(1, 1), [shown(200), shown(40), shown(40), 255], "{what}");
                assert_eq!(
                    at(10, 5),
                    [shown(250), shown(250), shown(250), 255],
                    "{what}"
                );
            }
        }
    }
}

#[test]
fn hidden_layers_and_the_layers_of_hidden_groups_are_not_drawn() {
    let base = solid([0, 0, 10, 20], [10, 20, 30]);
    let hidden = TestLayer {
        hidden: true,
        ..solid([0, 0, 10, 20], [255, 0, 0])
    };
    assert_eq!(
        flat(&[base.clone(), hidden]).get_pixel(3, 3).0,
        [10, 20, 30, 255]
    );
    // Bottom to top: the group's closing marker, its member, then its (hidden) header.
    let grouped = [
        base.clone(),
        marker(3, false),
        solid([0, 0, 10, 20], [255, 0, 0]),
        marker(1, true),
    ];
    assert_eq!(flat(&grouped).get_pixel(3, 3).0, [10, 20, 30, 255]);
    let shown = [
        base,
        marker(3, false),
        solid([0, 0, 10, 20], [255, 0, 0]),
        marker(1, false),
    ];
    assert_eq!(flat(&shown).get_pixel(3, 3).0, [255, 0, 0, 255]);
}

#[test]
fn opacity_transparency_and_masks_let_the_layer_below_through() {
    let base = solid([0, 0, 10, 20], [0, 0, 0]);
    let half = TestLayer {
        opacity: 128,
        ..solid([0, 0, 10, 20], [255, 255, 255])
    };
    assert_eq!(
        flat(&[base.clone(), half]).get_pixel(3, 3).0,
        [128, 128, 128, 255]
    );
    let clear = TestLayer {
        alpha: Some(0),
        ..solid([0, 0, 10, 20], [255, 255, 255])
    };
    assert_eq!(
        flat(&[base.clone(), clear]).get_pixel(3, 3).0,
        [0, 0, 0, 255]
    );
    // A mask that hides everything outside columns 10..20.
    let masked = TestLayer {
        mask: Some(([0, 10, 10, 20], 0, 255)),
        ..solid([0, 0, 10, 20], [255, 255, 255])
    };
    let img = flat(&[base, masked]);
    assert_eq!(img.get_pixel(3, 3).0, [0, 0, 0, 255]);
    assert_eq!(img.get_pixel(15, 3).0, [255, 255, 255, 255]);
}

#[test]
fn a_clipped_layer_draws_only_where_its_base_does() {
    let back = solid([0, 0, 10, 20], [0, 0, 0]);
    let base = solid([0, 0, 10, 10], [0, 0, 255]);
    let clipped = TestLayer {
        clipped: true,
        ..solid([0, 0, 10, 20], [255, 0, 0])
    };
    let img = flat(&[back, base, clipped]);
    assert_eq!(img.get_pixel(3, 3).0, [255, 0, 0, 255], "inside the base");
    assert_eq!(img.get_pixel(15, 3).0, [0, 0, 0, 255], "outside it");
}

#[test]
fn nothing_but_uncovered_canvas_is_transparent() {
    let img = flat(&[solid([0, 0, 5, 20], [9, 9, 9])]);
    assert_eq!(img.get_pixel(3, 2).0, [9, 9, 9, 255]);
    assert_eq!(img.get_pixel(3, 7).0[3], 0);
}

#[test]
fn a_cut_short_layered_document_is_refused_not_misread() {
    let f = layered(
        (20, 10),
        &[
            solid([0, 0, 10, 20], [200, 40, 40]),
            solid([2, 5, 8, 15], [250, 250, 250]),
        ],
        (16, 3, false),
    );
    for cut in (40..f.len()).step_by(7) {
        let _ = decode(&f[..cut], 64);
    }
    // Cut inside the layer records: nothing is drawn from half a layer.
    let records = f.windows(4).position(|w| w == b"Lr16").expect("Lr16 block") + 30;
    assert!(decode(&f[..records], 64).is_none());
}

/// `real.psd` keeps one layer of no size in an `Lr16` block and its picture in the composite,
/// whatever its flag says. Flattening that layer drew nothing, and the empty canvas was taken
/// for the picture: every surface drew a blank tile where 3.2.0 drew the file (the big-file
/// gate's blind-spot check found it).
#[test]
fn a_layer_that_draws_nothing_leaves_the_composite() {
    let Some(bytes) = st2k_base::testcorpus::read("real.psd") else {
        eprintln!("NOT MEASURED: real.psd absent");
        return;
    };
    let img = super::from_reader(Cursor::new(&bytes), 256)
        .expect("reads")
        .to_rgba8();
    assert!(img.pixels().all(|p| p[3] == 255), "the composite is opaque");
    let first = img.get_pixel(0, 0);
    assert!(img.pixels().any(|p| p != first), "a flat picture");
}
