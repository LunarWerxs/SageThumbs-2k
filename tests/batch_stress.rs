//! Large-batch stress: every output must be the size that was asked for.
//!
//! PowerToys shipped exactly this bug in Image Resizer 0.96.0 (their #42116): a
//! batch resize would drop the chosen preset for roughly one file in a hundred
//! and shrink it to 100px instead. It survived release because their tests
//! checked that outputs *existed*, not that they were *right*, and a 1-in-100
//! failure never shows up in a three-file test.
//!
//! Our batch path is a hand-rolled scoped thread pool (`src/parallel.rs`) with
//! output names reserved serially before the parallel pass, so the same class of
//! race is plausible here. This test therefore runs a few hundred files through
//! it and asserts the DIMENSIONS of every single output, plus that no two files
//! collided on a name.
//!
//! Sizes deliberately vary per file so a mixed-up result cannot coincidentally
//! look correct, and one file is a deliberately unreadable dud: a batch must
//! report that one failure without disturbing its neighbours.

use std::collections::HashSet;
use std::path::PathBuf;

use sagethumbs2k_core::{parallel, resize_file, Resize};

/// Enough files to expose a 1-in-100 race, small enough to stay a fast test.
const COUNT: usize = 300;
/// The resize cap every output must respect.
const FIT: u32 = 64;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("st2k_batch_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn every_output_of_a_large_batch_has_the_requested_size() {
    let dir = scratch("resize");

    // Varying source sizes: 100..=399 wide, half as tall. All are larger than FIT
    // on the long edge, so every single one must actually be scaled down.
    let mut inputs = Vec::with_capacity(COUNT);
    for i in 0..COUNT {
        let w = 100 + i as u32;
        let h = w / 2;
        let p = dir.join(format!("img{i:04}.png"));
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            w,
            h,
            image::Rgb([(i % 256) as u8, 90, 160]),
        ))
        .save(&p)
        .unwrap();
        inputs.push(p.to_string_lossy().into_owned());
    }

    // Same fan-out the context menu and `st2k batch` use.
    let results = parallel::map(&inputs, |_, p| resize_file(p, Resize::Fit(FIT, FIT)));

    let mut seen: HashSet<PathBuf> = HashSet::new();
    for (i, r) in results.iter().enumerate() {
        let out = r
            .as_ref()
            .unwrap_or_else(|e| panic!("file {i} failed to resize: {e:?}"));
        assert!(
            seen.insert(out.clone()),
            "two inputs wrote to the same output path: {}",
            out.display()
        );

        let src_w = 100 + i as u32;
        let src_h = src_w / 2;
        let scale = f64::from(FIT) / f64::from(src_w.max(src_h));
        let expect_w = ((f64::from(src_w) * scale).round() as u32).max(1);

        let (w, h) =
            image::image_dimensions(out).unwrap_or_else(|e| panic!("output {i} unreadable: {e}"));
        // The long edge is the contract; allow a pixel of rounding on both.
        assert!(
            w.abs_diff(expect_w) <= 1,
            "file {i}: width {w}, expected about {expect_w} (source {src_w}x{src_h})"
        );
        assert!(
            w.max(h) <= FIT,
            "file {i}: {w}x{h} exceeds the {FIT}px cap — the preset was dropped"
        );
        assert!(w > 1 && h > 1, "file {i} collapsed to {w}x{h}");
    }
    assert_eq!(seen.len(), COUNT, "some outputs were overwritten");

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn one_bad_file_does_not_disturb_the_rest_of_the_batch() {
    let dir = scratch("dud");
    let mut inputs = Vec::new();
    for i in 0..40 {
        let p = dir.join(format!("img{i:03}.png"));
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            120,
            80,
            image::Rgb([10, 20, 30]),
        ))
        .save(&p)
        .unwrap();
        inputs.push(p.to_string_lossy().into_owned());
    }
    // A file that claims to be a PNG and is not.
    let dud = dir.join("img017-broken.png");
    std::fs::write(&dud, b"this is not an image at all").unwrap();
    inputs.insert(17, dud.to_string_lossy().into_owned());

    let results = parallel::map(&inputs, |_, p| resize_file(p, Resize::Fit(FIT, FIT)));

    assert!(results[17].is_err(), "the broken file should have failed");
    for (i, r) in results.iter().enumerate() {
        if i == 17 {
            continue;
        }
        let out = r
            .as_ref()
            .unwrap_or_else(|e| panic!("neighbour {i} failed because of the dud: {e:?}"));
        let (w, h) = image::image_dimensions(out).unwrap();
        assert!(w.max(h) <= FIT, "neighbour {i} came out {w}x{h}");
    }

    let _ = std::fs::remove_dir_all(&dir);
}
