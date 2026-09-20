use super::*;

/// A deterministic photographic source: smooth large-scale structure plus per-pixel noise.
/// Flat or purely smooth content would let ANY reduction look correct; the high-frequency
/// half is what separates a filter that antialiases from one that does not.
fn photographic(w: u32, h: u32) -> DynamicImage {
    let mut buf = image::RgbImage::new(w, h);
    let mut state = 0x9E37_79B9u32;
    for (x, y, p) in buf.enumerate_pixels_mut() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let noise = (state >> 24) as i32 - 128;
        use std::f32::consts::TAU;
        let sx = ((x as f32 / w as f32) * TAU).sin();
        let sy = ((y as f32 / h as f32) * TAU * 1.5).cos();
        let base = 128.0 + 90.0 * sx * sy;
        let v = |o: i32| (base as i32 + o + noise / 3).clamp(0, 255) as u8;
        *p = image::Rgb([v(0), v(20), v(-25)]);
    }
    DynamicImage::ImageRgb8(buf)
}

/// A single 8-bit box-reduce block large enough that `sum * 255` exceeds
/// `u32::MAX` (a tiny requested edge — `--size 1` — against a large source drives `k`
/// this high) must still average correctly. A flat fill must round-trip to its own
/// colour per this function's own contract; a wrapped `u32` accumulator would not.
#[test]
fn box_reduce_u8_does_not_overflow_on_a_huge_block() {
    // k=4200 over one output pixel: 4200*4200*255 ~= 4.498e9, already past u32::MAX
    // (4.295e9) for a SINGLE channel's accumulator, before summing anything else.
    let (w, h, k) = (4200usize, 4200usize, 4200usize);
    let src = vec![255u8; w * h];
    let out = box_reduce_u8(&src, w, h, 1, k);
    assert_eq!(
        out,
        vec![255u8],
        "a flat 255 fill must reduce to 255, not a wrapped value"
    );
}

/// The pre-reduction is a SPEED change, and a speed change that quietly altered every
/// thumbnail would be a bad trade. This pins how far it can move the picture: the two-pass
/// result is compared against the single-pass filter it replaces, on content chosen to be
/// hostile to a box filter.
#[test]
fn the_pre_reduction_barely_moves_the_picture() {
    // 2048 is the comfortable case (a 5x step, 1.6x left over) and 768 is the softest
    // admitted (a 2x step, 1.5x left over, so the box filter does the largest share of
    // the work it ever will).
    let (mut mean, mut worst) = (0.0f64, 0u32);
    for edge in [2048u32, 768] {
        let img = photographic(edge, edge * 3 / 4);
        let one_pass = img.resize(256, 256, FilterType::Lanczos3).to_rgba8();
        let two_pass = pre_reduce(img, 256)
            .resize(256, 256, FilterType::Lanczos3)
            .to_rgba8();
        assert_eq!(one_pass.dimensions(), two_pass.dimensions());

        let (mut sum, mut w) = (0u64, 0u32);
        for (a, b) in one_pass.as_raw().iter().zip(two_pass.as_raw()) {
            let d = u32::from(a.abs_diff(*b));
            sum += u64::from(d);
            w = w.max(d);
        }
        let m = sum as f64 / one_pass.as_raw().len() as f64;
        eprintln!("pre-reduction delta at {edge} px: mean {m:.4}, worst {w}");
        mean = mean.max(m);
        worst = worst.max(w);
    }
    assert!(
        mean <= MEAN_DELTA_CEILING,
        "the pre-reduction moved the average channel by {mean:.4}, over the {MEAN_DELTA_CEILING} this is allowed to"
    );
    assert!(
        worst <= WORST_DELTA_CEILING,
        "the pre-reduction moved one channel by {worst}, over the {WORST_DELTA_CEILING} this is allowed to"
    );
}

/// Where the fit's time actually goes, for a size the speed corpus says matters. Banked as
/// a measurement rather than a gate: the split between the box pass and the real filter is
/// what decides whether [`pre_reduce`] is worth its existence, and guessing it wrong is how
/// an "optimisation" ends up slower than what it replaced.
///
/// `text
/// cargo test --release --lib -p sagethumbs2k fit_cost_split -- --ignored --nocapture
/// `
#[test]
#[ignore = "measurement, not a gate; run --release --nocapture"]
fn fit_cost_split() {
    use std::time::Instant;
    for (w, h) in [(1279u32, 1280u32), (2048, 1536), (4000, 3000)] {
        let img = photographic(w, h);
        let best = |mut f: Box<dyn FnMut()>| {
            let mut best = f64::MAX;
            for _ in 0..5 {
                let t = Instant::now();
                f();
                best = best.min(t.elapsed().as_secs_f64() * 1000.0);
            }
            best
        };
        let a = img.clone();
        let one = best(Box::new(move || {
            let _ = a.resize(256, 256, FilterType::Lanczos3);
        }));
        let b = img.clone();
        let boxed = best(Box::new(move || {
            let _ = pre_reduce(b.clone(), 256);
        }));
        let c = img.clone();
        let two = best(Box::new(move || {
            let _ = pre_reduce(c.clone(), 256).resize(256, 256, FilterType::Lanczos3);
        }));
        println!("{w}x{h}: single-pass {one:.1} ms | box {boxed:.1} ms | box+filter {two:.1} ms");
    }
}

/// The float arms exist so the caller can reduce BEFORE tone-mapping, which is only correct
/// if the reduction stays in linear light and in full float range. A path that clamped to
/// [0,1], or that dropped to 8 bits on the way, would quietly destroy exactly the highlight
/// detail an HDR file is kept for - and it would do it invisibly, since the tone map that
/// runs afterwards compresses the range anyway.
#[test]
fn float_buffers_are_reduced_in_linear_light_and_full_range() {
    let mut buf = image::ImageBuffer::<image::Rgb<f32>, Vec<f32>>::new(1024, 1024);
    for (x, _y, p) in buf.enumerate_pixels_mut() {
        // Red ramps far ABOVE 1.0 (a real Radiance sun is thousands); green is a constant
        // well above white; blue stays sub-unit so a clamp in either direction shows up.
        *p = image::Rgb([x as f32 * 10.0, 4096.0, 0.25]);
    }
    let reduced = pre_reduce(DynamicImage::ImageRgb32F(buf), 256);
    assert_eq!((reduced.width(), reduced.height()), (512, 512));
    let DynamicImage::ImageRgb32F(out) = reduced else {
        panic!("a float image must stay float through the reduction");
    };
    // 1024 / (256 * 3 / 2) = 2, so output column 1 is the mean of source columns 2 and 3:
    // (20 + 30) / 2 = 25. Arithmetic mean, in linear light, not clamped.
    let px = out.get_pixel(1, 0).0;
    assert!(
        (px[0] - 25.0).abs() < 1e-3,
        "red must be the linear mean of 20 and 30, got {}",
        px[0]
    );
    assert!(
        (px[1] - 4096.0).abs() < 1e-3,
        "a constant far above 1.0 must survive unclamped, got {}",
        px[1]
    );
    assert!(
        (px[2] - 0.25).abs() < 1e-6,
        "a sub-unit constant must survive exactly, got {}",
        px[2]
    );
}

/// A reduction must not upscale, must not fire when there is nothing to win, and must
/// leave at least [`PRE_REDUCE_GAP`] times over for the real filter.
#[test]
fn the_pre_reduction_fires_only_where_it_pays() {
    let big = photographic(2048, 1536);
    let reduced = pre_reduce(big, 256);
    assert_eq!(
        (reduced.width(), reduced.height()),
        (410, 308),
        "2048 px at a 256 px ask reduces by 5, leaving a 1.6x gap"
    );
    assert!(reduced.width() * 2 >= 256 * PRE_REDUCE_GAP_HALVES);

    // The softest case admitted: 768 px steps by 2, leaving exactly the 1.5x gap and no
    // more. This is the boundary  also
    // measures, so the softest result this can produce is a pinned one.
    let worst_case = pre_reduce(photographic(768, 768), 256);
    assert_eq!((worst_case.width(), worst_case.height()), (384, 384));

    // Short of a full second step the image is untouched, which keeps the band where the
    // single-pass filter is still cheap on the single-pass filter.
    for edge in [256u32, 400, 511, 767] {
        let img = photographic(edge, edge);
        let same = pre_reduce(img, 256);
        assert_eq!(
            (same.width(), same.height()),
            (edge, edge),
            "{edge} px at a 256 px ask has no whole-number step worth taking"
        );
    }
}

/// 16-bit is the worst case for the single-pass filter and the one every scanner and most
/// PNG/TIFF writers produce, so its buffer must be reduced too, in its own precision.
#[test]
fn sixteen_bit_buffers_are_reduced_in_sixteen_bit() {
    let mut buf = image::ImageBuffer::<image::Rgb<u16>, Vec<u16>>::new(1024, 1024);
    for (x, _y, p) in buf.enumerate_pixels_mut() {
        // A ramp far above 8-bit resolution: neighbouring columns differ by 64, which is
        // a quarter of one 8-bit step, so a path that dropped to 8 bits would flatten it.
        *p = image::Rgb([(x * 64) as u16, 30_000, 65_535 - (x * 64) as u16]);
    }
    let reduced = pre_reduce(DynamicImage::ImageRgb16(buf), 256);
    assert_eq!((reduced.width(), reduced.height()), (512, 512));
    let DynamicImage::ImageRgb16(out) = reduced else {
        panic!("a 16-bit image must stay 16-bit through the reduction");
    };
    // Output column 1 covers source columns 2 and 3, whose red is 128 and 192.
    assert_eq!(out.get_pixel(1, 0).0[0], 160);
    assert_eq!(out.get_pixel(1, 0).0[1], 30_000);
}

/// **This repo reduces a thumbnail in TWO places, and only one of them is what Explorer
/// draws.** `cli::thumbnail` (and `view_png`, and the batch verb) finish with
/// `DynamicImage::thumbnail`; the shell extension finishes with [`fit_to_box`]. Every
/// visual gate there is — `check-render-parity.ps1`, `regression.ps1`, the contact sheets,
/// the MCP `view` tool — drives the CLI. So a green parity run is only evidence about the
/// shipped picture to the extent that these two agree.
///
/// **They are ONE reduction now** ([`reduce_to_fit`]), so this test asserts EQUALITY. What
/// follows is the measurement that justified the change, kept because it is the evidence
/// for a decision that cost ~355 re-accepted parity baselines, and because **it came out
/// the opposite way round from the guess** (mean / worst channel difference, before):
///
/// ```text
/// 4000x3000 -> 256   15.6x   mean 0.85   worst  5
/// 4000x3000 ->  96   41.7x   mean 1.68   worst  7
/// 1600x1200 -> 256    6.3x   mean 2.12   worst 12
///  800x600  -> 256    3.1x   mean 4.39   worst 23
///  512x384  -> 256    2.0x   mean 4.37   worst 21   <-- every sample the gate has
/// ```
///
/// The two reductions AGREE on a big camera photo and DISAGREE on a small one. That is
/// structural, not noise: past a 2x step [`pre_reduce`] does the bulk as a box average and
/// leaves Lanczos3 only the last 1.5x, so the provider's path converges on the CLI's box
/// filter exactly where the reduction is large. At 2x nothing pre-reduces, so it is a clean
/// 2x2 box average against Lanczos3's sharpening lobes at full strength.
///
/// **The corpus is 512x384, so the parity gate ran at the worst row in that table.** A
/// worst-pixel difference of 21 is over the +/-8 per-channel band `compare-renders.py`
/// calls a match — i.e. a green parity run was not evidence about the shipped picture at
/// any size, and least of all at the one it tests. That is what made unifying them worth
/// the baseline churn rather than a tidy-up to defer.
///
/// The CLI's picture got BETTER, not merely different: Lanczos3 plus the integer pre-pass
/// is the sharper reduction, and it is also faster on anything large, since the pre-pass is
/// what took the fit from 815 ms to 52 ms at 12 MP. The MCP `view` tool, the contact
/// sheets, the right-click preview tile and the Quick preview's display cap all moved with
/// it, so there is no cheap-filter surface left to drift.
#[test]
fn the_gates_reduce_a_thumbnail_the_way_the_shell_extension_does() {
    for (w, h, cx) in [
        (4000u32, 3000u32, 256u32), // a 12 MP camera photo at Explorer's largest tile
        (4000, 3000, 96),           // ...and at Medium icons, a 41x reduction
        (1600, 1200, 256),          // a phone photo
        (800, 600, 256),            // barely over the box, so nothing pre-reduces
        (512, 384, 256),            // the corpus's own size — all the parity gate ever sees
        (513, 385, 256),            // odd edges, where the two filters' rounding differed
    ] {
        let img = photographic(w, h);
        let shell = fit_to_box(img.clone(), cx);
        let cli = reduce_to_fit(img, cx, cx).to_rgba8();

        assert_eq!(
            (shell.width, shell.height),
            cli.dimensions(),
            "{w}x{h} into a {cx} box: the two paths disagree on the output SIZE"
        );
        // EQUALITY, not a tolerance. Both paths now run `reduce_to_fit`, so any difference
        // at all means one of them has grown a step the other does not have — which is the
        // exact state this test was written to end. A tolerance here would let that drift
        // back in one level at a time.
        assert!(
            shell.rgba == *cli.as_raw(),
            "{w}x{h} into a {cx} box: the CLI's picture and the shell extension's are no \
             longer identical, so the visual gates have stopped standing in for the \
             picture Explorer actually draws"
        );
    }
}

/// The other half of the contract, and the reason the two paths could not simply be the
/// same call: `fit_to_box` ENLARGES a small source to fill the shell's box (issue #25 —
/// Explorer centres an undersized tile rather than scaling it, so a small PSD preview drew
/// as a smaller tile than the file beside it), while `st2k thumbnail --size` is a CEILING
/// and must hand back the source untouched rather than invent pixels.
///
/// Sharing one reduction is only safe while that asymmetry is deliberate, so it is pinned
/// here rather than left to be rediscovered by whoever unifies the next call site.
#[test]
fn the_shared_reduction_never_enlarges_even_though_the_shell_tile_does() {
    for (w, h, cx) in [(64u32, 48u32, 256u32), (200, 150, 256), (256, 256, 256)] {
        let kept = reduce_to_fit(photographic(w, h), cx, cx);
        assert_eq!(
            (kept.width(), kept.height()),
            (w, h),
            "{w}x{h} already fits a {cx} box - the shared reduction must return it untouched"
        );
    }
    // ...while the shell's own fit still fills the box, from the same source.
    let filled = fit_to_box(photographic(200, 150), 256);
    assert_eq!((filled.width, filled.height), (256, 192));

    // Non-square, the case the right-click tile (220x88) and the Quick preview (2048x4096)
    // need: reduction is governed by whichever axis is tighter, and the other is not
    // stretched to meet its own limit.
    let wide = reduce_to_fit(photographic(2000, 1000), 220, 88);
    assert_eq!((wide.width(), wide.height()), (176, 88));
}

/// A minimal, real TIFF with a single IFD0 entry (Orientation, tag 0x0112, SHORT):
/// header + one 12-byte entry + the 4-byte "next IFD" terminator, in the endianness
/// requested. Mirrors `tiff_thumbnail`'s own layout expectations.
fn minimal_tiff_with_orientation(le: bool, orientation: u16) -> Vec<u8> {
    let mut t = Vec::new();
    t.extend_from_slice(if le { b"II" } else { b"MM" });
    let u16b = |v: u16| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    let u32b = |v: u32| if le { v.to_le_bytes() } else { v.to_be_bytes() };
    t.extend_from_slice(&u16b(42));
    t.extend_from_slice(&u32b(8)); // IFD0 starts right after the 8-byte header
    t.extend_from_slice(&u16b(1)); // one entry
    t.extend_from_slice(&u16b(0x0112)); // tag: Orientation
    t.extend_from_slice(&u16b(3)); // type: SHORT
    t.extend_from_slice(&u32b(1)); // count: 1
    t.extend_from_slice(&u16b(orientation)); // value, left-justified in the 4-byte field
    t.extend_from_slice(&u16b(0)); // padding to fill the value field
    t.extend_from_slice(&u32b(0)); // next-IFD offset: none
    t
}

/// The second half: a TIFF-magic buffer must read Orientation straight out of IFD0
/// via the bounded `r16`/`r32` walk, not through `exif::Reader` — both endiannesses,
/// and it must agree with whatever `exif::Reader` would have said.
#[test]
fn exif_orientation_reads_tiff_ifd0_directly() {
    for le in [true, false] {
        let tiff = minimal_tiff_with_orientation(le, 6);
        assert_eq!(
            tiff_ifd0_orientation(&tiff),
            Some(6),
            "le={le}: IFD0 walk must find the Orientation tag"
        );
        assert_eq!(
            exif_orientation(&tiff),
            Some(6),
            "le={le}: exif_orientation must route TIFF magic through the IFD0 walk"
        );
    }
    // No Orientation tag at all: a bare, entry-less IFD0 must yield None, not panic.
    let mut t = Vec::new();
    t.extend_from_slice(b"II");
    t.extend_from_slice(&42u16.to_le_bytes());
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&0u16.to_le_bytes()); // zero entries
    t.extend_from_slice(&0u32.to_le_bytes());
    assert_eq!(tiff_ifd0_orientation(&t), None);

    // Truncated/garbage TIFF-shaped input must decline cleanly, never panic.
    assert_eq!(tiff_ifd0_orientation(b"II*\0"), None);
    assert_eq!(tiff_ifd0_orientation(b""), None);
    assert_eq!(tiff_ifd0_orientation(b"not a tiff at all"), None);
}
