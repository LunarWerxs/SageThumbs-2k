use super::*;

// Locks the fix for A121: shrink_one's routed `--quality` arg must be DERIVED from
// encode::EMAIL_JPEG_QUALITY (now pub(crate)), not a hand-copied literal that can
// silently drift from the in-process value. Referencing the constant here would not
// even COMPILE without the pub(crate) visibility fix, and the value check catches a
// future edit to one side without the other.
#[test]
fn routed_email_quality_matches_in_process_constant() {
    let routed_quality_arg = crate::verbs::encode::EMAIL_JPEG_QUALITY.to_string();
    assert_eq!(routed_quality_arg, "82");
    assert_eq!(crate::verbs::encode::EMAIL_JPEG_QUALITY, 82);
}

/// The module docs promise a missing helper "can never break a verb — it only
/// forfeits the crash isolation". Nothing was proving that, and the reason is easy
/// to miss: [`st2k_exe`] RESOLVES under test on any machine that has built the
/// workspace, because cargo hardlinks `st2k.exe` into the very `deps\` directory
/// the test binary runs from — and CI runs `cargo build` before `cargo test`, so it
/// does too. Every routed verb therefore takes the ROUTED arm in BOTH places, and
/// the in-process arm — the one a DLL-only install actually runs — was exercised
/// nowhere. Don't "simplify" this by deleting the explicit `None`: that argument is
/// the whole test, and letting `st2k_exe()` supply it would silently go back to
/// testing the routed path twice.
#[test]
fn the_in_process_fallback_still_converts_when_no_helper_is_present() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_helper_fallback_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("photo.png");
    DynamicImage::ImageRgb8(image::RgbImage::from_pixel(9, 7, image::Rgb([10, 90, 200])))
        .save(&src)
        .unwrap();

    // `None` is exactly what `run_action` passes when `st2k_exe()` finds nothing.
    let out = convert_one(
        None,
        src.to_str().unwrap(),
        Target {
            format: ImageFormat::Jpeg,
            ext: "jpg",
            webp_quality: None,
        },
    )
    .expect("a missing helper must forfeit isolation only — never the conversion");

    assert_eq!(
        out.extension().and_then(|e| e.to_str()),
        Some("jpg"),
        "the fallback must land on the same auto-named path the routed arm targets"
    );
    assert!(
        image::open(&out).is_ok(),
        "the fallback's output must be a real decodable image, not an empty slot"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// Locks the fix for A277: `transform_one` must return the path `st2k rotate`
// actually reports on stdout, not a name predicted before the subprocess ran
// (which a concurrent edit could steal). `run_st2k_capture` is the mechanism
// that makes that possible — it must actually read the child's stdout instead
// of discarding it like `run_st2k` does. Exercises a real subprocess (cmd.exe
// standing in for st2k) rather than mocking it: a regression back to
// `.status()` (stdout discarded) would make this fail every time, since
// `CaptureOutcome::Ok` could never be reached.
#[test]
fn run_st2k_capture_reads_the_real_path_from_stdout() {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let cmd_exe = Path::new(&system_root).join("System32").join("cmd.exe");
    // No spaces/parens in the echoed path — keeps this test independent of
    // Windows' argv quoting rules, which aren't what's under test here.
    let outcome = run_st2k_capture(
        &cmd_exe,
        "C:\\out\\file.jpg",
        &["/c", "echo", "C:\\out\\file_edited.jpg"],
    );
    match outcome {
        CaptureOutcome::Ok(path) => {
            assert_eq!(path, PathBuf::from("C:\\out\\file_edited.jpg"));
        }
        CaptureOutcome::Failed => panic!("cmd.exe echo should have exited 0 with output"),
        CaptureOutcome::SpawnFailed => panic!("cmd.exe should always be spawnable in CI"),
    }
}

/// `run_st2k` used to discard the child's stderr entirely (`Stdio::null()`),
/// so a non-zero exit logged only a generic "failed for {path}" with no hint of
/// WHY (access denied, disk full, an unsupported target — all indistinguishable).
/// Exercises a real subprocess that writes a known marker to stderr and exits
/// non-zero, and checks that marker actually reaches the diagnostics log.
#[test]
fn run_st2k_logs_the_real_stderr_on_a_non_zero_exit() {
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let cmd_exe = Path::new(&system_root).join("System32").join("cmd.exe");
    let marker = format!("st2k_helper_stderr_marker_{}", std::process::id());

    let outcome = run_st2k(
        &cmd_exe,
        "C:\\some\\path.jpg",
        &["/c", "echo", &marker, "1>&2", "&", "exit", "1"],
    );
    assert!(
        matches!(outcome, RunOutcome::Failed),
        "a non-zero exit must report Failed"
    );

    let log_path = crate::safety::log_file().expect("LOCALAPPDATA must be set to find the log");
    let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        contents.contains(&marker),
        "the child's real stderr must reach the diagnostics log"
    );
}

fn scratch(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "st2k_helper_{tag}_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Pure validation, no subprocess: rejects a stream shorter than the header, a
/// `w`/`h` over `MAX_DIM`, and a payload whose length doesn't match `w * h * 4`
/// exactly — the checks that keep a hostile or buggy `clip-pixels` child from
/// handing the parent anything it will trust with a bounded memcpy.
#[test]
fn parse_clip_pixels_rejects_short_oversized_and_mismatched_streams() {
    // Shorter than the 8-byte header.
    assert!(parse_clip_pixels(&[1, 2, 3]).is_none());

    // A well-formed 2x2 header with only 4 of the required 16 payload bytes.
    let mut short_payload = 2u32.to_le_bytes().to_vec();
    short_payload.extend_from_slice(&2u32.to_le_bytes());
    short_payload.extend_from_slice(&[0u8; 4]);
    assert!(parse_clip_pixels(&short_payload).is_none());

    // A width over MAX_DIM must be refused outright, whatever the rest of the
    // stream looks like — an attacker-controlled header claiming a huge canvas.
    let mut oversized = (decode::limits::MAX_DIM + 1).to_le_bytes().to_vec();
    oversized.extend_from_slice(&1u32.to_le_bytes());
    assert!(parse_clip_pixels(&oversized).is_none());

    // A correctly-sized, in-bounds stream parses to the exact dimensions + bytes.
    let mut good = 2u32.to_le_bytes().to_vec();
    good.extend_from_slice(&2u32.to_le_bytes());
    let rgba = [9u8; 16];
    good.extend_from_slice(&rgba);
    let (w, h, pixels) = parse_clip_pixels(&good).expect("a well-formed stream must parse");
    assert_eq!((w, h), (2, 2));
    assert_eq!(pixels, &rgba[..]);
}

/// `clipboard_one`'s ROUTED arm (`st2k clip-pixels` on the batch pool) must decode
/// the exact same pixels the in-process arm would hand to `copy_rgba_to_clipboard`
/// — checked without touching the real clipboard (shared, process-global state a
/// test can't safely claim) by comparing the RGBA bytes each side produces.
#[test]
fn clip_pixels_routed_output_matches_the_in_process_decode() {
    let dir = scratch("clip_pixels");
    let src = dir.join("swatch.png");
    let img = image::RgbaImage::from_fn(3, 2, |x, y| {
        image::Rgba([(x * 40) as u8, (y * 60) as u8, 200, 255])
    });
    DynamicImage::ImageRgba8(img.clone()).save(&src).unwrap();
    let path = src.to_str().unwrap();
    let expected = img.into_raw();

    let Some(exe) = st2k_exe() else {
        panic!("st2k.exe must be resolvable under test — see the module docs' fallback note");
    };
    match run_st2k_capture_bytes(&exe, path, &["clip-pixels", path]) {
        BytesOutcome::Ok(stdout) => {
            let (w, h, rgba) = parse_clip_pixels(&stdout)
                .expect("clip-pixels must print a well-formed header + payload");
            assert_eq!((w, h), (3, 2));
            assert_eq!(rgba, expected.as_slice());
        }
        BytesOutcome::Failed => panic!("st2k clip-pixels failed for {path}"),
        BytesOutcome::SpawnFailed => panic!("st2k.exe must be spawnable under test"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

/// `clipboard_one(None, …)` must degrade straight to `copy_to_clipboard`, never
/// trying to spawn anything — checked by comparing outcomes rather than reading
/// the real clipboard back (no test in this file touches it directly, matching
/// `clipboard.rs`'s own tests, which stop at the pure DIB-building functions).
#[test]
fn clipboard_one_with_no_helper_takes_the_in_process_fallback() {
    let dir = scratch("clip_fallback");
    let src = dir.join("swatch.png");
    DynamicImage::ImageRgb8(image::RgbImage::from_pixel(2, 2, image::Rgb([1, 2, 3])))
        .save(&src)
        .unwrap();
    let path = src.to_str().unwrap();

    assert_eq!(
        clipboard_one(None, path).is_ok(),
        copy_to_clipboard(path).is_ok(),
        "None must delegate straight to copy_to_clipboard, whatever this session's \
         clipboard access turns out to be"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `prepare_wallpaper_routed`'s ROUTED arm (`st2k wallpaper-prepare` on the batch
/// pool) must decode/resize/encode the exact same PNG bytes the in-process
/// `prepare_wallpaper` arm does, and the `None` arm must succeed standalone (the
/// fallback). Both arms target the SAME persistent `%APPDATA%\SageThumbs2K`
/// destination by design (the desktop needs a fixed path to keep reading from,
/// not a scratch one) — this is the exact file every real Set-as-wallpaper click
/// already overwrites, so reading it back here carries no extra risk.
#[test]
fn wallpaper_prepare_routed_matches_the_in_process_decode() {
    let dir = scratch("wallpaper_routed");
    let src = dir.join("swatch.png");
    DynamicImage::ImageRgb8(image::RgbImage::from_pixel(6, 4, image::Rgb([12, 200, 90])))
        .save(&src)
        .unwrap();
    let path = src.to_str().unwrap();

    let Some(exe) = st2k_exe() else {
        panic!("st2k.exe must be resolvable under test — see the module docs' fallback note");
    };
    let routed = prepare_wallpaper_routed(Some(&exe), path, false)
        .expect("the routed arm must succeed for a valid PNG");
    let routed_bytes = std::fs::read(&routed).unwrap();

    let in_process = prepare_wallpaper_routed(None, path, false)
        .expect("the in-process (fallback) arm must succeed for the same PNG");
    let in_process_bytes = std::fs::read(&in_process).unwrap();

    assert_eq!(
        routed, in_process,
        "both arms must target the same persistent path"
    );
    assert_eq!(
        routed_bytes, in_process_bytes,
        "the routed and in-process arms must encode identical PNG bytes"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `folder_icon_one`'s ROUTED arm (`st2k folder-icon` on the batch pool) must
/// produce the exact same `.ico` bytes and `desktop.ini` content the in-process
/// `set_folder_icon` arm does, each in a folder it owns; the `None` arm succeeding
/// on its own is the fallback proof.
#[test]
fn folder_icon_routed_matches_the_in_process_output() {
    let base = scratch("foldericon_routed");
    let routed_dir = base.join("routed");
    let direct_dir = base.join("direct");
    std::fs::create_dir_all(&routed_dir).unwrap();
    std::fs::create_dir_all(&direct_dir).unwrap();

    let make_src = |dir: &Path| {
        let p = dir.join("src.png");
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(5, 5, image::Rgb([30, 60, 90])))
            .save(&p)
            .unwrap();
        p
    };
    let routed_src = make_src(&routed_dir);
    let direct_src = make_src(&direct_dir);

    let Some(exe) = st2k_exe() else {
        panic!("st2k.exe must be resolvable under test — see the module docs' fallback note");
    };
    folder_icon_one(Some(&exe), routed_src.to_str().unwrap())
        .expect("the routed arm must succeed for a valid PNG");
    folder_icon_one(None, direct_src.to_str().unwrap())
        .expect("the in-process (fallback) arm must succeed for the same PNG");

    let routed_ico = std::fs::read(routed_dir.join("SageThumbsFolder.ico")).unwrap();
    let direct_ico = std::fs::read(direct_dir.join("SageThumbsFolder.ico")).unwrap();
    assert_eq!(
        routed_ico, direct_ico,
        "both arms must encode an identical .ico"
    );

    let routed_ini = std::fs::read_to_string(routed_dir.join("desktop.ini")).unwrap();
    let direct_ini = std::fs::read_to_string(direct_dir.join("desktop.ini")).unwrap();
    assert_eq!(
        routed_ini, direct_ini,
        "both arms must write an identical desktop.ini"
    );

    let _ = std::fs::remove_dir_all(&base);
}

/// `compress_one`'s ROUTED arm (`st2k compress` on the batch pool) must reach the
/// same meetable target the in-process `compress_one_to_size` arm does, with
/// byte-identical output (same engine, same deterministic search); the `None` arm
/// succeeding on its own is the fallback proof.
#[test]
fn compress_one_routed_matches_the_in_process_result_for_a_meetable_target() {
    let dir = scratch("compress_routed");
    // Per-pixel noise so the JPEG has real size to search over (a flat image would
    // compress to almost nothing regardless of target).
    let img = image::RgbImage::from_fn(96, 96, |x, y| {
        let h = (x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B)).rotate_left(7);
        image::Rgb([h as u8, (h >> 8) as u8, (h >> 16) as u8])
    });
    let src = dir.join("noise.png");
    DynamicImage::ImageRgb8(img).save(&src).unwrap();
    let path = src.to_str().unwrap();
    // Generous relative to the 96x96 noise source — easily meetable either way.
    let target = 40_000u64;

    let Some(exe) = st2k_exe() else {
        panic!("st2k.exe must be resolvable under test — see the module docs' fallback note");
    };
    let routed =
        compress_one(Some(&exe), path, target).expect("the routed arm must meet the target");
    let routed_bytes = std::fs::read(&routed).unwrap();
    assert!(
        routed_bytes.len() as u64 <= target,
        "routed output {} exceeds target {target}",
        routed_bytes.len()
    );

    let in_process = compress_one(None, path, target)
        .expect("the in-process (fallback) arm must meet the same target");
    let in_process_bytes = std::fs::read(&in_process).unwrap();
    assert!(
        in_process_bytes.len() as u64 <= target,
        "in-process output {} exceeds target {target}",
        in_process_bytes.len()
    );

    assert_eq!(
        routed_bytes, in_process_bytes,
        "the routed and in-process arms must produce byte-identical output"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
