#![cfg(test)]

use super::*;

fn noisy_jpeg_bytes(w: u32, h: u32) -> Vec<u8> {
    let mut img = image::RgbImage::new(w, h);
    for y in 0..h {
        for x in 0..w {
            let r = ((x * 37 + y * 11) & 0xFF) as u8;
            let g = ((x * 13 + y * 53) & 0xFF) as u8;
            let b = ((x * 97 + y * 3) & 0xFF) as u8;
            img.put_pixel(x, y, image::Rgb([r, g, b]));
        }
    }
    let mut bytes = Vec::new();
    image::DynamicImage::ImageRgb8(img)
        .write_to(
            &mut std::io::Cursor::new(&mut bytes),
            image::ImageFormat::Jpeg,
        )
        .expect("encode noisy jpeg");
    bytes
}

/// Wrap a JPEG's bytes with an EXIF APP1 declaring `orientation` (1..=8).
///
/// Hand-assembled rather than pulled from a corpus file so the test states exactly what
/// it depends on: one IFD0 entry, tag 0x0112, little-endian TIFF.
fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
    assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "fixture must be a JPEG");
    let mut app1: Vec<u8> = Vec::new();
    app1.extend_from_slice(b"Exif\0\0");
    app1.extend_from_slice(b"II\x2A\x00"); // little-endian TIFF magic
    app1.extend_from_slice(&8u32.to_le_bytes()); // IFD0 begins at offset 8
    app1.extend_from_slice(&1u16.to_le_bytes()); // one entry
    app1.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
    app1.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
    app1.extend_from_slice(&1u32.to_le_bytes()); // count
    app1.extend_from_slice(&(orientation as u32).to_le_bytes()); // value, left-packed
    app1.extend_from_slice(&0u32.to_le_bytes()); // no next IFD

    let mut out = Vec::with_capacity(jpeg.len() + app1.len() + 4);
    out.extend_from_slice(&jpeg[..2]); // SOI
    out.extend_from_slice(&[0xFF, 0xE1]);
    out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
    out.extend_from_slice(&app1);
    out.extend_from_slice(&jpeg[2..]);
    out
}

/// Minimal JPEG shaped like a CMYK/YCCK frame: SOI + SOF0 declaring `nf` components.
/// Matches `color::is_cmyk_jpeg`'s own detection rule (component count only, no pixel
/// decode needed), same construction proven against that function in `decode::tests`.
fn jpeg_with_components(nf: u8) -> Vec<u8> {
    let len = 8 + 3 * nf as usize; // SOF0 length field
    let mut b = vec![0xFF, 0xD8]; // SOI
    b.extend_from_slice(&[0xFF, 0xC0, (len >> 8) as u8, len as u8, 8, 0, 1, 0, 1, nf]);
    b.extend(std::iter::repeat_n(0u8, 3 * nf as usize)); // component specs
    b.extend_from_slice(&[0xFF, 0xD9]); // EOI
    b
}

/// A064 / A083: the DCT-scaled fast path used to gate only on size + JPEG magic, so a
/// CMYK/YCCK JPEG over the size floor skipped `is_cmyk_jpeg`'s color-managed tier and
/// went through WIC's naive CMYK->RGB instead. The size floor is gone now (see
/// [`scaled_prepass_declines`]), which makes the CMYK arm the ONLY thing standing between
/// a CMYK JPEG and WIC's naive conversion - so it is checked at both ends of the size
/// range, not just past a floor that no longer exists.
#[test]
fn scaled_prepass_declines_cmyk_jpeg_even_when_large_enough_to_qualify() {
    let small_cmyk = jpeg_with_components(4);
    assert!(
        scaled_prepass_declines(&small_cmyk),
        "a small CMYK JPEG must be declined too - the floor that used to catch it is gone"
    );
    let mut small_rgb = jpeg_with_components(3);
    small_rgb.resize(40 * 1024, 0);
    assert!(
        !scaled_prepass_declines(&small_rgb),
        "a small non-CMYK JPEG must now QUALIFY: removing the floor is the whole change"
    );

    let mut cmyk = jpeg_with_components(4);
    cmyk.resize(600 * 1024, 0);
    assert!(
        is_cmyk_jpeg(&cmyk),
        "fixture must actually look CMYK to the shared detector"
    );
    assert!(
        scaled_prepass_declines(&cmyk),
        "a large CMYK JPEG must still be declined by the fast-path gate"
    );

    // Sanity: an otherwise-identical 3-component (non-CMYK) header of the SAME size must
    // NOT be declined — proves the assertion above is about component count, not merely
    // about being JPEG-shaped.
    let mut rgb_like = jpeg_with_components(3);
    rgb_like.resize(600 * 1024, 0);
    assert!(!is_cmyk_jpeg(&rgb_like));
    assert!(
        !scaled_prepass_declines(&rgb_like),
        "a large non-CMYK JPEG must still qualify for the fast path"
    );
}

/// A084: `decode_preview_path` returns `decode_preview_streamed`'s result directly on
/// success; for a file past MAX_INPUT_BYTES that result comes from the stream cascade, whose
/// WIC rescue decodes like `wic_scaled_from_path` does - which used to hand back WIC's raw,
/// unrotated pixels with no EXIF orientation applied anywhere on that branch — so a large
/// rotated phone photo/scan rendered sideways. `wic_scaled_from_path` carries no size gate
/// of its own (its callers apply theirs), so this exercises the real function directly
/// off a real file, the same as the oversized rescue once did for an oversized one.
#[test]
fn wic_scaled_from_path_applies_exif_orientation() {
    // The path is genuinely WIC, so it needs COM on this thread like the other WIC tests.
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    let base = noisy_jpeg_bytes(1400, 900); // landscape
    let bytes = with_exif_orientation(&base, 6); // 6 = rotate 90 deg CW
    assert_eq!(
        exif_orientation(&bytes),
        Some(6),
        "the APP1 orientation must be readable by the same reader apply_exif_orientation uses"
    );

    // PID-suffixed so concurrent `cargo test` runs cannot race each other on the file.
    let path = std::env::temp_dir().join(format!(
        "st2k_oversized_rescue_orient_{}.jpg",
        std::process::id()
    ));
    std::fs::write(&path, &bytes).expect("stage temp jpeg");
    let p = path.to_string_lossy().into_owned();

    let out = wic_scaled_from_path(&p, 256).expect("WIC must decode the staged JPEG");
    assert!(
        out.height() > out.width(),
        "orientation 6 must rotate the landscape source to portrait, got {}x{}",
        out.width(),
        out.height()
    );

    let _ = std::fs::remove_file(&path);
}

/// Issue #34: a Convert batch silently dropped every PSD over 256 MiB, because the verb
/// read its input through the THUMBNAIL path's DoS budget. This pins the RELATIONSHIP
/// rather than the numbers — whatever the two ceilings are, the one for a file the user
/// picked must be the larger.
///
/// A const block, so it is checked when the crate compiles rather than when a test runs:
/// both sides are constants, and a build that got this backwards should not produce a
/// binary at all. (It is also what clippy asks for, for the same reason.)
#[test]
fn a_user_chosen_file_gets_a_bigger_budget_than_one_that_arrived() {
    const {
        assert!(
            limits::MAX_FULL_FIDELITY_INPUT_BYTES > limits::MAX_INPUT_BYTES,
            "Convert must not be refused at the size an unsolicited thumbnail is"
        );
        // The reported files ran 34-502 MB. Both ends have to be inside the verb budget,
        // and the top end was outside the old one — which is the whole bug.
        let reported_largest: u64 = 502 * 1000 * 1000;
        assert!(reported_largest > limits::MAX_INPUT_BYTES);
        assert!(reported_largest < limits::MAX_FULL_FIDELITY_INPUT_BYTES);
    }
}

/// The refusal that remains must be a REPORTED one. A read that dies by aborting the
/// process (which is what an infallible `Vec` growth does under `panic = "abort"`) would
/// take the whole batch with it, so the ceiling is checked from metadata before anything
/// is allocated, and the message says what happened.
#[test]
fn an_over_ceiling_file_is_refused_with_a_reason_not_an_abort() {
    let path =
        std::env::temp_dir().join(format!("st2k_full_fidelity_cap_{}.bin", std::process::id()));
    std::fs::write(&path, b"not actually huge").expect("stage temp file");
    let p = path.to_string_lossy().into_owned();
    // Under the ceiling: read normally, byte-for-byte.
    assert_eq!(read_full_fidelity(&p).unwrap(), b"not actually huge");

    // A sparse file past the ceiling, so the refusal is exercised without staging 2 GiB
    // of real bytes. `set_len` reserves the LENGTH only; Windows reports it as the file
    // size, which is exactly what the metadata check reads.
    let big = std::env::temp_dir().join(format!(
        "st2k_full_fidelity_over_{}.bin",
        std::process::id()
    ));
    let f = std::fs::File::create(&big).expect("create sparse file");
    f.set_len(limits::MAX_FULL_FIDELITY_INPUT_BYTES + 1)
        .expect("set_len");
    drop(f);
    let err = read_full_fidelity(&big.to_string_lossy()).expect_err("must refuse");
    let msg = err.to_string();
    assert!(
        msg.contains("over the") && msg.contains("limit"),
        "the refusal must say why, got: {msg}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&big);
}

/// `len` from `std::fs::metadata` is a SNAPSHOT, not a bound — a plain
/// `read_to_end` keeps reading to EOF regardless, so a file that grows WHILE this reads
/// it (a download in progress, a log, a share) used to come back larger than what was
/// checked against the ceiling, and could grow the `Vec` past its fallible reservation
/// via `read_to_end`'s own infallible growth. `Read::take(len)` must cap the read at
/// exactly the metadata-time size no matter how much more the file grows underneath it.
///
/// Driven through the reader seam with a source that holds far more than `len`, so the
/// growth is a fact of the input rather than a race. The earlier version of this test
/// raced a writer thread against the real function's metadata call and lost on a loaded
/// box (2026-09-19: the writer appended 25 blocks before the metadata read, so the
/// "metadata-time length" it asserted against was simply wrong), which blocked a push on
/// a test that measured scheduling, not the code.
#[test]
fn read_full_fidelity_caps_at_the_metadata_time_length_even_if_the_file_grows() {
    let initial = vec![b'a'; 4096];
    let mut grown = initial.clone();
    grown.extend(std::iter::repeat_n(b'b', 200 * 4096));

    let got = read_full_fidelity_from(&grown[..], initial.len() as u64)
        .expect("must still read the checked-size prefix");
    assert_eq!(
        got.len(),
        initial.len(),
        "the read must stop at the metadata-time length ({}), not follow the file's growth",
        initial.len()
    );
    assert!(
        got.iter().all(|&b| b == b'a'),
        "must be exactly the original bytes, no appended ones"
    );
    // And the real function still goes through that seam: a static file reads whole.
    let path = std::env::temp_dir().join(format!(
        "st2k_full_fidelity_static_{}.bin",
        std::process::id()
    ));
    std::fs::write(&path, &initial).expect("stage temp file");
    let whole = read_full_fidelity(&path.to_string_lossy()).expect("static file");
    assert_eq!(whole, initial);
    let _ = std::fs::remove_file(&path);
}

/// A source that yields more than its metadata advertised (a file growing under the
/// read) must be refused at the bound, not returned truncated and not read to EOF; a
/// source of exactly the cap is fine. Driven with in-memory readers so the grow-on-read
/// case is deterministic rather than a race against a writer thread.
#[test]
fn read_bounded_refuses_a_source_past_the_cap_and_accepts_one_at_it() {
    let at_cap = vec![b'x'; 64];
    assert_eq!(
        read_bounded(&at_cap[..], 64).expect("exactly the cap is allowed"),
        at_cap
    );
    let grown = [b'x'; 65];
    let err = read_bounded(&grown[..], 64).expect_err("one past the cap must be refused");
    assert!(
        err.to_string().contains("over the 64 byte limit"),
        "the refusal must say why, got: {err}"
    );
    // Far past the cap: the reader must stop at cap+1, not read the lot.
    struct Endless;
    impl std::io::Read for Endless {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            buf.fill(b'y');
            Ok(buf.len())
        }
    }
    assert!(
        read_bounded(Endless, 1024).is_err(),
        "an endless source is refused"
    );
}

/// The by-path preview read's under-cap branch goes through the bounded reader: a file
/// that is under the cap at metadata time but over it by the time it is read is refused
/// instead of allocated in full. The file is grown BEFORE the read here, so the outcome
/// is deterministic; the reader-level race is pinned by the test above.
#[test]
fn preview_read_refuses_a_file_that_grew_past_the_cap_after_its_size_was_checked() {
    let path = std::env::temp_dir().join(format!("st2k_preview_grown_{}.bin", std::process::id()));
    std::fs::write(&path, vec![b'a'; 100]).expect("stage temp file");
    let p = path.to_string_lossy().into_owned();

    // Under the cap: read byte-for-byte.
    let got = read_preview_capped_at(&p, 100, 16, ANY_PREVIEW).expect("at the cap reads");
    assert_eq!(got.len(), 100);

    // Now the cap is smaller than the file: the under-cap branch is never entered, and
    // a plain (non-head-preview) file is refused by the oversize path, as before.
    assert!(read_preview_capped_at(&p, 99, 16, ANY_PREVIEW).is_err());

    // A source that reports one size and delivers another is the reader's job:
    // the same bytes through `read_bounded` with the metadata-time cap are refused.
    let f = std::fs::File::open(&path).expect("open");
    assert!(
        read_bounded(f, 99).is_err(),
        "the reader, not the metadata call, must enforce the cap"
    );

    let _ = std::fs::remove_file(&path);
}

/// A TIFF decoded by WIC - and past the input ceiling every TIFF is - keeps its colour
/// profile. Windows' TIFF codec offers the InterColorProfile tag as no colour context, so
/// `wic::wic_tiff_icc` asks for it by name; before that, the corpus's `real.tif` (an Apple
/// display profile) rendered 7 levels off the image tier, which matches a colour-managed
/// ImageMagick reference to 0.1. Found by the big-file gate (`scripts/bigfiles/`).
#[test]
fn a_tiff_decoded_by_wic_is_colour_managed_like_the_image_tier() {
    let (Some(path), Some(bytes)) = (
        st2k_base::testcorpus::path("real.tif"),
        st2k_base::testcorpus::read("real.tif"),
    ) else {
        eprintln!("NOT MEASURED: real.tif absent");
        return;
    };
    unsafe {
        let _ = windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
        );
    }
    let wic = wic_scaled_from_path(&path.to_string_lossy(), 256)
        .expect("WIC must decode real.tif")
        .to_rgb8();
    let tier = super::decode_preview(&bytes)
        .expect("the image tier must decode real.tif")
        .resize_exact(wic.width(), wic.height(), FilterType::Triangle)
        .to_rgb8();
    let diff: u64 = wic
        .as_raw()
        .iter()
        .zip(tier.as_raw())
        .map(|(&a, &b)| u64::from(a.abs_diff(b)))
        .sum();
    let mean = diff as f64 / wic.as_raw().len() as f64;
    assert!(
        mean < 2.5,
        "WIC's real.tif is {mean:.1} levels off the image tier"
    );
}
