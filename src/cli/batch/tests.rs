#![cfg(test)]

use super::*;

/// Serialises the tests that touch the process-wide Ctrl+C `CANCEL` flag against each
/// other and against anything that reads it. One flag, many test threads, so any test
/// that WRITES it must hold this first.
static CANCEL_FLAG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The flag part of the Ctrl+C wiring: `install` must start a fresh run from
/// "not cancelled" even if a previous run's Ctrl+C left it set — the actual OS-level
/// `SetConsoleCtrlHandler` registration and CTRL_C_EVENT delivery can't be exercised
/// in-process (there is no safe way to raise a real console control event against the
/// test runner itself), so this pins the one behaviour that IS a pure state check.
///
/// SERIALISED, and it has to be: `CANCEL` is one process-wide flag that `cli::prebuild`
/// also reads, cargo runs this file's tests on many threads in one process, and this test
/// deliberately SETS the flag. Without the lock it can make a concurrent prebuild test see
/// a cancellation nobody asked for, which is a flake that would look like a real bug in
/// the cancel path. Same defect an audit found in the DPI-override tests, same fix.
#[test]
fn ctrlc_cancel_install_resets_the_flag() {
    use std::sync::atomic::Ordering;
    let _guard = CANCEL_FLAG_TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    ctrlc_cancel::flag().store(true, Ordering::SeqCst);
    ctrlc_cancel::install();
    assert!(
        !ctrlc_cancel::flag().load(Ordering::SeqCst),
        "install() must clear a flag left set by an earlier run"
    );
    // Leave the shared flag as we found it, so a test that runs after this one is not
    // handed a stale cancellation.
    ctrlc_cancel::flag().store(false, Ordering::SeqCst);
}

/// The exact A058 race: two callers reserving under the SAME (dir, stem, ext)
/// concurrently must never both walk away with the same path. A check-then-
/// create loop (`exists()` then open) can let two threads both pass the check
/// for the same candidate before either creates it; `create_new` cannot.
#[test]
fn reserve_batch_output_is_race_safe_under_concurrent_callers() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_cli_toctou_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::sync::Arc::new(dir);

    let handles: Vec<_> = (0..8)
        .map(|_| {
            let dir = dir.clone();
            std::thread::spawn(move || reserve_batch_output(&dir, "race", "webp"))
        })
        .collect();
    let mut got: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    got.sort();
    let before = got.len();
    got.dedup();
    assert_eq!(
        got.len(),
        before,
        "two concurrent callers claimed the SAME output path — the exact race this fix closes"
    );

    let _ = std::fs::remove_dir_all(&*dir);
}

/// A name a batch's OWN earlier iteration already claimed, and a name some
/// external writer created before `batch` ever ran, must both be skipped —
/// the reservation itself is what proves it, not the (now-removed) `used` set.
#[test]
fn reserve_batch_output_skips_names_already_on_disk() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_cli_reserve_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("img.webp"), b"external writer").unwrap();

    let first = reserve_batch_output(&dir, "img", "webp").unwrap();
    assert_eq!(first, dir.join("img (1).webp"));
    let second = reserve_batch_output(&dir, "img", "webp").unwrap();
    assert_eq!(second, dir.join("img (2).webp"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// `batch convert --to webp --quality N` must actually vary output size with
/// N — before this fix `webp_quality` was hard-coded to `None` (lossless) no
/// matter what quality was requested. Gated like `verbs.rs`'s own
/// `lossy_webp_is_smaller_and_keeps_alpha`: without `webp-lossy`, WebP is
/// ALWAYS encoded losslessly regardless of `webp_quality`, so this can only
/// prove anything when the feature (which every release build enables) is on.
#[cfg(feature = "webp-lossy")]
#[test]
fn batch_convert_to_webp_honors_quality() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_cli_webpq_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    // Genuine per-pixel noise (an integer hash, not a linear/periodic formula):
    // a plain modular formula like `x*53 + y*17` has constant row/column
    // differences, which a LOSSLESS predictive coder crushes to near-nothing —
    // the opposite of what this test needs. Real noise is what quality-10
    // LOSSY WebP shrinks a lot and lossless does not.
    let img = image::RgbImage::from_fn(200, 200, |x, y| {
        let i = y.wrapping_mul(200).wrapping_add(x);
        let mut h = i.wrapping_mul(0x9E37_79B9) ^ 0x85EB_CA6B;
        h ^= h >> 16;
        h = h.wrapping_mul(0x045D_9F3B);
        h ^= h >> 16;
        image::Rgb([
            (h & 0xFF) as u8,
            ((h >> 8) & 0xFF) as u8,
            ((h >> 16) & 0xFF) as u8,
        ])
    });
    let src = dir.join("noise.png");
    image::DynamicImage::ImageRgb8(img).save(&src).unwrap();

    batch(
        "convert",
        &[src.to_str().unwrap().to_string()],
        false,
        Some(dir.to_str().unwrap()),
        256,
        Some("webp"),
        10, // aggressively lossy
        verbs::Resize::None,
        false,
    )
    .unwrap();
    let lossy_len = std::fs::metadata(dir.join("noise.webp")).unwrap().len();

    // The single-file path with an explicit `None` webp_quality is the known-
    // lossless baseline to compare against.
    let lossless = dir.join("noise_lossless.webp");
    verbs::convert_to(
        src.to_str().unwrap(),
        &lossless,
        10,
        None,
        verbs::Resize::None,
    )
    .unwrap();
    let lossless_len = std::fs::metadata(&lossless).unwrap().len();

    assert!(
        lossy_len < lossless_len,
        "batch webp at quality 10 ({lossy_len} bytes) should be smaller than lossless \
         ({lossless_len} bytes) — quality is not reaching the encoder"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// A OneDrive/Dropbox "free up space" placeholder must be skipped, not
/// silently hydrated (downloaded) by opening it for a decode.
#[test]
fn expand_inputs_skips_cloud_placeholders() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_cli_offline_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let normal = dir.join("normal.png");
    std::fs::write(&normal, b"not a real png, just needs to exist").unwrap();
    let placeholder = dir.join("cloud.png");
    std::fs::write(&placeholder, b"placeholder").unwrap();

    // FILE_ATTRIBUTE_OFFLINE, set directly rather than needing a real cloud
    // provider to reproduce the flag.
    unsafe {
        use std::os::windows::ffi::OsStrExt;
        use windows::core::PCWSTR;
        use windows::Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_OFFLINE};
        let wide: Vec<u16> = placeholder
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        SetFileAttributesW(PCWSTR(wide.as_ptr()), FILE_ATTRIBUTE_OFFLINE).unwrap();
    }
    assert!(crate::prebuild::is_cloud_placeholder(&placeholder));
    assert!(!crate::prebuild::is_cloud_placeholder(&normal));

    let (files, skipped, _) = expand_inputs(&[dir.to_str().unwrap().to_string()], false);
    assert_eq!(skipped, 1);
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with("normal.png"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// `expand_inputs` must find only the top-level file when `recurse` is false (the
/// historical default — an agent pointed at a photo tree with subfolders used to get a
/// partial result and a clean "N/N succeeded" with no way to ask for more), and every
/// file at every depth when `recurse` is true.
#[test]
fn expand_inputs_recurse_flag_controls_subdirectory_depth() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_cli_recurse_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let nested = dir.join("sub").join("deeper");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(dir.join("top.png"), b"top").unwrap();
    std::fs::write(dir.join("sub").join("mid.png"), b"mid").unwrap();
    std::fs::write(nested.join("bottom.png"), b"bottom").unwrap();

    let (shallow, _, _) = expand_inputs(&[dir.to_str().unwrap().to_string()], false);
    assert_eq!(
        shallow.len(),
        1,
        "non-recursive scan must stay one level deep"
    );
    assert!(shallow[0].ends_with("top.png"));

    let (deep, _, _) = expand_inputs(&[dir.to_str().unwrap().to_string()], true);
    assert_eq!(deep.len(), 3, "recursive scan must find every depth");
    assert!(deep.iter().any(|p| p.ends_with("top.png")));
    assert!(deep.iter().any(|p| p.ends_with("mid.png")));
    assert!(deep.iter().any(|p| p.ends_with("bottom.png")));

    let _ = std::fs::remove_dir_all(&dir);
}

/// `batch`'s `"info"` op returns a JSON array, one element per input, in the same
/// shape `info(_, true)` returns for a single file — and a per-file decode failure must
/// not fail the whole batch, just carry an `"error"` field on that one element.
#[test]
fn batch_info_op_returns_a_json_array_with_per_file_results() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_cli_batchinfo_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let good = dir.join("ok.png");
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(64, 48))
        .save(&good)
        .unwrap();

    let out = batch(
        "info",
        &[good.to_str().unwrap().to_string()],
        false,
        None,
        256,
        None,
        90,
        verbs::Resize::None,
        false,
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["width"], serde_json::json!(64));
    assert_eq!(arr[0]["height"], serde_json::json!(48));
    assert!(arr[0]["input"].as_str().unwrap().ends_with("ok.png"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// 2026-09-05 audit, F11, the whole acceptance case in one run: one file that converts,
/// one whose bytes no decoder takes, and one whose output name cannot be claimed. The
/// counts have to add up, the two failures have to carry DIFFERENT causes (they need
/// different fixes: replace the file, versus write somewhere else), and every failure
/// has to name its input path exactly as it was passed, because that path IS the retry.
/// Before this, all three came back as `1/3 succeeded (2 failed)` and nothing else.
///
/// The unwritable destination is a FOLDER already sitting on the output name, not an
/// ACL: Windows answers `create_new` on a directory with access-denied, so this is a
/// real permission failure the test can set up in one line and clean up in one more.
#[test]
fn batch_reports_a_distinct_cause_per_failure_and_a_retryable_list() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_cli_causes_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let good = dir.join("good.png");
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
        .save(&good)
        .unwrap();
    let broken = dir.join("broken.png");
    std::fs::write(&broken, b"this is not a PNG, or anything else").unwrap();
    let blocked = dir.join("blocked.png");
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(8, 8))
        .save(&blocked)
        .unwrap();
    std::fs::create_dir(dir.join("blocked.webp")).unwrap();

    let inputs: Vec<String> = [&good, &broken, &blocked]
        .iter()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    let run = |json| {
        batch(
            "convert",
            &inputs,
            false,
            None,
            256,
            Some("webp"),
            90,
            verbs::Resize::None,
            json,
        )
    };

    let v: serde_json::Value = serde_json::from_str(&run(true).unwrap()).unwrap();
    assert_eq!(v["status"], "partial");
    assert_eq!(v["requested"], 3);
    assert_eq!(v["succeeded"], 1);
    assert_eq!(v["failed"], 2);
    let results = v["results"].as_array().unwrap();
    assert_eq!(results.len(), 3, "one entry per input, in input order");

    assert_eq!(results[0]["status"], "ok");
    assert_eq!(results[0]["cause"], serde_json::Value::Null);
    assert!(results[0]["output"]
        .as_str()
        .unwrap()
        .ends_with("good.webp"));

    assert_eq!(results[1]["status"], "failed");
    assert_eq!(
        results[1]["cause"], "undecodable",
        "bytes no decoder takes is a bad INPUT: {}",
        results[1]
    );
    assert_eq!(results[2]["status"], "failed");
    assert_eq!(
        results[2]["cause"], "unwritable",
        "a destination that cannot be claimed is a bad OUTPUT: {}",
        results[2]
    );
    assert_ne!(
        results[1]["cause"], results[2]["cause"],
        "two different problems must not report one cause"
    );
    for (i, expected) in [(1usize, &broken), (2, &blocked)] {
        assert_eq!(
            results[i]["input"].as_str().unwrap(),
            expected.to_string_lossy(),
            "a failed entry must be retryable by the path it was given"
        );
        assert!(
            !results[i]["detail"].as_str().unwrap().is_empty(),
            "a cause without a sentence explains nothing to a person"
        );
    }
    assert!(
        results[2]["output"].is_null(),
        "nothing was written for the blocked file"
    );

    // The human form keeps its one-line summary and adds one tab-separated line per
    // failure, the same shape `pdf`/`cbz` print for an omitted input.
    let text = run(false).unwrap();
    assert!(
        text.starts_with("1/3 succeeded (2 failed)"),
        "the summary line is unchanged: {text}"
    );
    assert!(
        text.contains(&format!("failed\t{}\tundecodable\t", broken.display()))
            && text.contains(&format!("failed\t{}\tunwritable\t", blocked.display())),
        "each failure is named with its cause: {text}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
