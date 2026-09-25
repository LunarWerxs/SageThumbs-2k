#![cfg(test)]

//! Fuzzing the code BEHIND the decoders: fuzz bytes read as an image that is always valid, run
//! through the thumbnail and preview fitting pipeline, and checked against what that pipeline
//! promises rather than only for panics.
//!
//! Why a second kind of target: every other target in this harness feeds raw bytes to a
//! parser, and almost every mutant dies at a magic check or a length test. That is the right
//! pressure for the parsers, but it means the stage every decoded picture goes through next
//! (the integer box pre-reduction, the Lanczos fit, the tiny-icon Nearest upscale, the display
//! rotation, the archive contact sheet) only ever sees the handful of images a real seed
//! decodes to. Here the bytes cannot be rejected: whatever they are, [`read_case`] turns them
//! into an image of some sample layout, size and content plus the sizes to fit it to, so every
//! input spends its time in the fitting code. The idea is tesseract's
//! `unittest/fuzzers/fuzzer-api.cpp`, which reads its fuzz input one bit per pixel into a fixed
//! bitmap for the same reason; the code here is written fresh for this harness.
//!
//! Every image is at most [`MAX_PIXELS`] pixels, so a case costs milliseconds and the always-on
//! gate stays cheap, while the extreme SHAPES (2048x1, 1x2048) that the pre-reduction's edge
//! blocks and rounding care about are still reachable.

use super::*;

use crate::container::collage::{compose_prepared, prepare_for_sheet};
use crate::decode::{reduce_to_fit, thumbnail_from_image, thumbnail_from_own_picture};
use crate::video::apply_display_rotation;
use image::{DynamicImage, ImageBuffer, Pixel};

/// Sample layouts [`build_image`] can produce: 8-bit, 16-bit and float, with and without alpha.
const LAYOUTS: u8 = 10;
/// Longest side of a fuzz image.
const MAX_EDGE: u32 = 2048;
/// Pixel ceiling per fuzz image; the height is bounded by what the width leaves.
const MAX_PIXELS: u32 = 1 << 15;
/// Largest requested thumbnail edge (Explorer asks for 16..=256; the rest is headroom).
const MAX_CX: u32 = 320;
/// Largest contact-sheet edge asked for.
const MAX_SHEET_EDGE: u32 = 300;
/// Display rotations tried; 45 is not a supported angle and must be a no-op.
const ROTATIONS: [u32; 5] = [0, 90, 180, 270, 45];
/// How far a flat colour may drift through a resize: float rounding in a filter whose weights
/// sum to one, then the 16-bit or float to 8-bit step.
const FLAT_TOLERANCE: u8 = 2;
/// Random inputs the always-on gate adds to its named cases.
const RANDOM_CASES: usize = 64;
/// Ceiling on one input's length, so a report can print the whole input and reproduce it.
const MAX_INPUT: usize = 2048;
/// Inputs the deep session keeps to mutate from.
const POOL_CAP: usize = 256;
/// The deep session stops after this many failures: past it, the report is noise.
const MAX_REPORTED: usize = 8;

/// Fuzz bytes read as a stream that never runs dry: past the end it wraps to the start, and an
/// empty input reads as zeros, so ANY byte string describes a complete case.
struct ByteStream<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteStream<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u8(&mut self) -> u8 {
        let Some(&b) = self.bytes.get(self.pos % self.bytes.len().max(1)) else {
            return 0;
        };
        self.pos += 1;
        b
    }

    fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }
}

fn read_u8(s: &mut ByteStream<'_>) -> u8 {
    s.u8()
}

fn read_u16(s: &mut ByteStream<'_>) -> u16 {
    s.u16()
}

/// A float in 0..=1: the range a flat image's colour must stay in for its expected 8-bit value
/// to be exact.
fn unit_f32(s: &mut ByteStream<'_>) -> f32 {
    f32::from(s.u8()) / 255.0
}

/// Any 32 bits as a float: NaN, the infinities, denormals and huge values all reach the HDR
/// path, as a hostile EXR's samples would.
fn raw_f32(s: &mut ByteStream<'_>) -> f32 {
    f32::from_bits(u32::from_be_bytes([s.u8(), s.u8(), s.u8(), s.u8()]))
}

/// The sample layout, size and content style of one fuzz image.
struct Shape {
    layout: u8,
    width: u32,
    height: u32,
    flat: bool,
}

/// Read a shape no larger than `max_pixels`. The width is read first and the height is bounded
/// by what is left, so both extremes of aspect ratio stay reachable.
fn read_shape(s: &mut ByteStream<'_>, max_pixels: u32) -> Shape {
    let layout = s.u8() % LAYOUTS;
    let width = 1 + u32::from(s.u16()) % MAX_EDGE;
    let tallest = (max_pixels / width).clamp(1, MAX_EDGE);
    let height = 1 + u32::from(s.u16()) % tallest;
    // One image in four is a single flat colour, which is what lets a check assert the OUTPUT
    // and not just survival: no resize of a flat image may change its colour.
    let flat = s.u8() % 4 == 0;
    Shape {
        layout,
        width,
        height,
        flat,
    }
}

/// How to fill one image's sample buffer.
struct Fill {
    pixels: usize,
    flat: bool,
}

impl Fill {
    /// `channels` samples per pixel. A flat image repeats one pixel, its alpha (the last
    /// channel, when `alpha`) forced opaque so no alpha handling can move its colour; any other
    /// image reads every sample from the stream.
    fn samples<T: Copy>(
        &self,
        s: &mut ByteStream<'_>,
        channels: usize,
        alpha: bool,
        read: fn(&mut ByteStream<'_>) -> T,
        opaque: T,
    ) -> Vec<T> {
        if !self.flat {
            return (0..self.pixels * channels).map(|_| read(s)).collect();
        }
        let mut px: Vec<T> = (0..channels).map(|_| read(s)).collect();
        if let (true, Some(a)) = (alpha, px.last_mut()) {
            *a = opaque;
        }
        px.repeat(self.pixels)
    }
}

/// Wrap a sample buffer that [`Fill`] sized from these same dimensions.
fn sized<P: Pixel>(w: u32, h: u32, data: Vec<P::Subpixel>) -> ImageBuffer<P, Vec<P::Subpixel>> {
    ImageBuffer::from_raw(w, h, data).expect("the buffer is sized from these dimensions")
}

/// Build the image `shape` describes, reading its samples from the stream.
fn build_image(s: &mut ByteStream<'_>, shape: &Shape) -> DynamicImage {
    let (w, h) = (shape.width, shape.height);
    let fill = Fill {
        pixels: w as usize * h as usize,
        flat: shape.flat,
    };
    let float: fn(&mut ByteStream<'_>) -> f32 = if shape.flat { unit_f32 } else { raw_f32 };
    let (byte, word) = (u8::MAX, u16::MAX);
    match shape.layout {
        0 => DynamicImage::ImageLuma8(sized(w, h, fill.samples(s, 1, false, read_u8, byte))),
        1 => DynamicImage::ImageLumaA8(sized(w, h, fill.samples(s, 2, true, read_u8, byte))),
        2 => DynamicImage::ImageRgb8(sized(w, h, fill.samples(s, 3, false, read_u8, byte))),
        3 => DynamicImage::ImageRgba8(sized(w, h, fill.samples(s, 4, true, read_u8, byte))),
        4 => DynamicImage::ImageLuma16(sized(w, h, fill.samples(s, 1, false, read_u16, word))),
        5 => DynamicImage::ImageLumaA16(sized(w, h, fill.samples(s, 2, true, read_u16, word))),
        6 => DynamicImage::ImageRgb16(sized(w, h, fill.samples(s, 3, false, read_u16, word))),
        7 => DynamicImage::ImageRgba16(sized(w, h, fill.samples(s, 4, true, read_u16, word))),
        8 => DynamicImage::ImageRgb32F(sized(w, h, fill.samples(s, 3, false, float, 1.0))),
        _ => DynamicImage::ImageRgba32F(sized(w, h, fill.samples(s, 4, true, float, 1.0))),
    }
}

/// One fuzz input as the pipeline sees it: an image and the sizes it is asked to fit.
struct Case {
    image: DynamicImage,
    flat: bool,
    /// The requested thumbnail edge. Zero is legal at the API and must be survived.
    cx: u32,
    /// The box [`reduce_to_fit`] fits to, each side 0..=255 (the right-click tile is 220x88).
    fit_box: (u32, u32),
    rotation: u32,
}

/// Turn any bytes into a [`Case`]. The layout is fixed so [`encode_case`] can build a named
/// case: `cx` (u16), box width, box height, rotation, then the [`Shape`] and its samples.
fn read_case(bytes: &[u8]) -> Case {
    let mut s = ByteStream::new(bytes);
    let cx = u32::from(s.u16()) % (MAX_CX + 1);
    let fit_box = (u32::from(s.u8()), u32::from(s.u8()));
    let rotation = ROTATIONS[usize::from(s.u8()) % ROTATIONS.len()];
    let shape = read_shape(&mut s, MAX_PIXELS);
    let image = build_image(&mut s, &shape);
    Case {
        image,
        flat: shape.flat,
        cx,
        fit_box,
        rotation,
    }
}

fn describe(c: &Case) -> String {
    format!(
        "{}x{} {:?}{} cx {} box {:?} rotation {}",
        c.image.width(),
        c.image.height(),
        c.image.color(),
        if c.flat { " flat" } else { "" },
        c.cx,
        c.fit_box,
        c.rotation
    )
}

/// The first contract a thumbnail keeps: a non-empty RGBA buffer whose length matches its
/// dimensions, inside the `edge`-square box it was fitted to.
fn check_tile(what: &str, t: &crate::decode::Decoded, edge: u32) -> Result<(), String> {
    let (w, h) = (t.width, t.height);
    if t.rgba.len() != w as usize * h as usize * 4 {
        return Err(format!("{what}: a {w}x{h} tile carries {} bytes", t.rgba.len()));
    }
    if w == 0 || h == 0 || w > edge || h > edge {
        return Err(format!("{what}: a {w}x{h} tile for a {edge}-pixel box"));
    }
    Ok(())
}

/// Whether any channel of `px` is further than [`FLAT_TOLERANCE`] from `want`.
fn drifted(px: &[u8], want: [u8; 4]) -> bool {
    px.iter()
        .zip(want)
        .any(|(&got, w)| got.abs_diff(w) > FLAT_TOLERANCE)
}

/// A flat image comes out the colour it went in, whatever filter or scale ran.
fn check_flat(what: &str, t: &crate::decode::Decoded, want: [u8; 4]) -> Result<(), String> {
    match t.rgba.chunks_exact(4).position(|px| drifted(px, want)) {
        Some(i) => Err(format!(
            "{what}: flat {want:?} came out as {:?} at pixel {i}",
            &t.rgba[i * 4..i * 4 + 4]
        )),
        None => Ok(()),
    }
}

/// The colour a flat image should thumbnail to: its first pixel, through the same 8-bit
/// conversion the pipeline's last step uses.
fn flat_colour(img: &DynamicImage) -> [u8; 4] {
    img.crop_imm(0, 0, 1, 1).to_rgba8().get_pixel(0, 0).0
}

/// The shell's two fits, `thumbnail_from_image` and `thumbnail_from_own_picture`. The second
/// never draws the file's own picture larger than it is.
fn check_thumbnails(c: &Case) -> Result<(), String> {
    let long = c.image.width().max(c.image.height());
    let fitted = thumbnail_from_image(c.image.clone(), c.cx);
    check_tile("thumbnail_from_image", &fitted, c.cx.max(1))?;
    let own = thumbnail_from_own_picture(c.image.clone(), c.cx);
    check_tile("thumbnail_from_own_picture", &own, c.cx.min(long).max(1))?;
    if c.flat {
        let want = flat_colour(&c.image);
        check_flat("thumbnail_from_image", &fitted, want)?;
        check_flat("thumbnail_from_own_picture", &own, want)?;
    }
    Ok(())
}

/// `reduce_to_fit`, the one reduction the CLI, the menu tile and Quick preview share: inside
/// the box, never empty, and an image that already fits comes back at its own size.
fn check_reduce(c: &Case) -> Result<(), String> {
    let (w, h) = (c.image.width(), c.image.height());
    let out = reduce_to_fit(c.image.clone(), c.fit_box.0, c.fit_box.1);
    let (ow, oh) = (out.width(), out.height());
    let (bw, bh) = (c.fit_box.0.max(1), c.fit_box.1.max(1));
    if ow == 0 || oh == 0 || ow > bw || oh > bh {
        return Err(format!("reduce_to_fit: {w}x{h} into {bw}x{bh} gave {ow}x{oh}"));
    }
    if w <= bw && h <= bh && (ow, oh) != (w, h) {
        return Err(format!("reduce_to_fit: {w}x{h} fits {bw}x{bh} yet gave {ow}x{oh}"));
    }
    Ok(())
}

/// `apply_display_rotation`: a quarter turn swaps the sides, and four of any turn (or of an
/// unsupported angle, which is a no-op) give back the same samples bit for bit. Compared as
/// bytes, since a NaN sample is never equal to itself.
fn check_rotation(c: &Case) -> Result<(), String> {
    let (w, h) = (c.image.width(), c.image.height());
    let once = apply_display_rotation(c.image.clone(), c.rotation);
    let quarter = c.rotation % 180 == 90;
    let want = if quarter { (h, w) } else { (w, h) };
    if (once.width(), once.height()) != want {
        return Err(format!(
            "apply_display_rotation: one turn gave {}x{}, expected {want:?}",
            once.width(),
            once.height()
        ));
    }
    let mut back = once;
    for _ in 0..3 {
        back = apply_display_rotation(back, c.rotation);
    }
    if (back.width(), back.height()) != (w, h) || back.as_bytes() != c.image.as_bytes() {
        return Err("apply_display_rotation: four turns did not give back the image".into());
    }
    Ok(())
}

/// The archive contact sheet, by the path `thumbnail_from_covers` takes (`prepare_for_sheet`
/// then `compose_prepared`): two to four images of any shape make one square sheet. It reads
/// the same bytes its own way, so one input also describes a set of covers.
fn check_sheet(bytes: &[u8]) -> Result<(), String> {
    let mut s = ByteStream::new(bytes);
    let count = 2 + usize::from(s.u8() % 3);
    let edge = 1 + u32::from(s.u16()) % MAX_SHEET_EDGE;
    let prepared: Vec<_> = (0..count)
        .map(|_| {
            let shape = read_shape(&mut s, MAX_PIXELS / 4);
            prepare_for_sheet(&build_image(&mut s, &shape), edge)
        })
        .collect();
    let Some(sheet) = compose_prepared(&prepared, edge) else {
        return Ok(());
    };
    if sheet.width() == sheet.height() {
        return Ok(());
    }
    Err(format!(
        "compose_prepared: {count} covers at edge {edge} made a {}x{} sheet",
        sheet.width(),
        sheet.height()
    ))
}

/// Every check, in pipeline order.
fn check_all(case: &Case, bytes: &[u8]) -> Result<(), String> {
    check_thumbnails(case)?;
    check_reduce(case)?;
    check_rotation(case)?;
    check_sheet(bytes)
}

/// Every check, on the case these bytes describe, with the case named in the failure.
fn check_case(bytes: &[u8]) -> Result<(), String> {
    let case = read_case(bytes);
    check_all(&case, bytes).map_err(|why| format!("{}: {why}", describe(&case)))
}

/// A caught panic, named with its message and the `file:line` it came from.
fn panic_report(payload: &(dyn std::any::Any + Send)) -> String {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".into());
    format!("PANIC {msg} at {}", last_panic_site())
}

/// Run one input through every check, catching panics. `None` when it held, otherwise a report
/// carrying the whole input, which reproduces the case exactly (the reading is deterministic).
fn run_case(bytes: &[u8]) -> Option<String> {
    let why = match catch_unwind(AssertUnwindSafe(|| check_case(bytes))) {
        Ok(Ok(())) => return None,
        Ok(Err(why)) => why,
        Err(payload) => panic_report(payload.as_ref()),
    };
    let hex = bytes
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .concat();
    Some(format!("{why}\n  input[{}]: {hex}", bytes.len()))
}

/// The bytes [`read_case`] reads back as exactly this case, so the gate names the shapes it
/// must cover instead of hoping random bytes land on them. `height` must be within what
/// [`read_shape`] allows beside `width`.
fn encode_case(
    cx: u16,
    rotation: u8,
    layout: u8,
    (width, height): (u16, u16),
    flat: bool,
) -> Vec<u8> {
    let mut b = cx.to_be_bytes().to_vec();
    b.extend([220, 88, rotation, layout]);
    b.extend((width - 1).to_be_bytes());
    b.extend((height - 1).to_be_bytes());
    // `read_shape` reads a byte divisible by four as flat.
    b.push(u8::from(!flat));
    // Sample material: a quiet NaN, negative infinity and 1.0 as big-endian floats, then
    // assorted bytes, so the float layouts meet non-finite samples on the named cases too.
    b.extend([0x7F, 0xC0, 0x00, 0x00, 0xFF, 0x80, 0x00, 0x00, 0x3F, 0x80, 0x00, 0x00]);
    b.extend([0x10, 0xC7, 0x5A]);
    b
}

/// The named cases: every layout both flat under an enlarging request and long-and-thin under
/// a small one, plus one case for each remaining branch of the fit.
fn named_cases() -> Vec<Vec<u8>> {
    let mut cases = Vec::new();
    for layout in 0..LAYOUTS {
        let rotation = layout % 5;
        // Mid-sized under an Explorer-sized request: the Lanczos enlargement.
        cases.push(encode_case(256, rotation, layout, (97, 61), true));
        // Long and thin under a small request: the integer pre-reduction's partial edge blocks.
        let wide = layout % 2 == 0;
        let thin = if wide { (2048, 1) } else { (1, 2048) };
        cases.push(encode_case(16, rotation, layout, thin, false));
    }
    cases.push(encode_case(0, 0, 3, (1, 1), true)); // a zero-sized request
    cases.push(encode_case(256, 1, 3, (3, 5), true)); // the Nearest pixel-art upscale
    cases.push(encode_case(32, 2, 8, (181, 181), false)); // square pre-reduction of floats
    cases.push(encode_case(96, 3, 7, (640, 48), true)); // wide, flat, 16-bit
    cases
}

/// The report for every input that did not hold.
fn failures_in(inputs: &[Vec<u8>]) -> Vec<String> {
    inputs.iter().filter_map(|b| run_case(b)).collect()
}

/// The always-on gate: the named cases and a fixed set of random inputs, every one a valid
/// image, through every fit the thumbnail and preview paths use.
#[test]
fn thumbnail_pipeline_keeps_its_contract_on_always_valid_images() {
    let mut inputs = named_cases();
    // The same rule `fuzzseed` holds its seeds to: a named case that does not become the
    // image it names covers nothing, so check the encoder still agrees with the reader.
    let layouts: std::collections::BTreeSet<String> = inputs
        .iter()
        .map(|b| format!("{:?}", read_case(b).image.color()))
        .collect();
    assert_eq!(
        layouts.len(),
        usize::from(LAYOUTS),
        "the named cases must reach every sample layout, reached {layouts:?}"
    );
    let mut rng = Rng::new(0x7E55_0FF5_A11D_0001);
    for _ in 0..RANDOM_CASES {
        let len = rng.below(MAX_INPUT);
        inputs.push((0..len).map(|_| rng.byte()).collect());
    }
    let failures = with_quiet_panics(|| failures_in(&inputs));
    assert!(
        failures.is_empty(),
        "{} thumbnail-pipeline failure(s) on always-valid images:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// A child for the deep session: usually a stacked mutation of a case that held, so a shape
/// that reached deep code is explored around, sometimes fresh random bytes. Capped, so a run
/// of duplicating mutations cannot grow a case past what one iteration can afford.
fn next_input(rng: &mut Rng, pool: &[Vec<u8>]) -> Vec<u8> {
    let mut input: Vec<u8> = if rng.below(4) == 0 {
        let len = rng.below(MAX_INPUT);
        (0..len).map(|_| rng.byte()).collect()
    } else {
        let parent = rng.below(pool.len());
        let stack = 1 + rng.below(4);
        mutate_stacked(rng, &pool[parent], &[], stack)
    };
    input.truncate(MAX_INPUT);
    input
}

/// Keep a case that held, replacing a random one once the pool is full. There is no coverage
/// signal here, so the pool is a rolling sample rather than a curated corpus.
fn keep(rng: &mut Rng, pool: &mut Vec<Vec<u8>>, input: Vec<u8>) {
    if pool.len() < POOL_CAP {
        pool.push(input);
    } else {
        let i = rng.below(pool.len());
        pool[i] = input;
    }
}

/// The deep half of the gate above, for the nightly fuzz workflow and for a run after touching
/// any resize, fit or sheet code. Every mutant is still a valid image, so the whole budget
/// goes to the pipeline.
///
/// ```text
/// ST2K_FUZZ_SECS=600 cargo test --release -p sagethumbs2k-codecs --lib fuzz::pipeline::deep -- --ignored --nocapture
/// ```
#[test]
#[ignore = "deep pipeline fuzz session (minutes); set ST2K_FUZZ_SECS and run with --ignored"]
fn deep_session_over_the_thumbnail_pipeline() {
    let secs: u64 = std::env::var("ST2K_FUZZ_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut rng = Rng::new(0x7E55_0FF5_A11D_0002);
    let mut pool = named_cases();
    let mut failures: Vec<String> = Vec::new();
    let mut runs = 0u64;
    with_quiet_panics(|| {
        while failures.len() < MAX_REPORTED && std::time::Instant::now() < deadline {
            let input = next_input(&mut rng, &pool);
            runs += 1;
            match run_case(&input) {
                Some(report) => failures.push(report),
                None => keep(&mut rng, &mut pool, input),
            }
        }
    });
    eprintln!("deep pipeline fuzz: {runs} cases in a {secs}s budget");
    assert!(
        failures.is_empty(),
        "{} thumbnail-pipeline failure(s) found by the deep session:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
