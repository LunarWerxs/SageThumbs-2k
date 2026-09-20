use super::foldericon::merge_shell_class_info;
use super::helper::routed_edit_output_ext;
use super::reveal_is_noise;
use super::{run_action, VerbAction};

/// A280: "Compress to under N MB" had no menu leaf / `VerbAction` / `run_action`
/// arm at all — this drives `run_action` exactly the way a right-click on the new
/// leaf would, and checks a real "(compressed)" JPEG lands next to the source.
#[test]
fn compress_to_size_dispatches_and_writes_a_compressed_sibling() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_actions_compress_dispatch_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let src = dir.join("photo.png");
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        64,
        48,
        image::Rgb([120, 40, 200]),
    ))
    .save(&src)
    .unwrap();
    let path = src.to_str().unwrap().to_string();

    let report = run_action(
        VerbAction::CompressToSize(crate::verbs::menu::CompressSize::Mb1),
        &[path],
    );
    assert_eq!(report.attempted, 1, "the one image was attempted");
    assert_eq!(report.done, 1, "compress must succeed on a plain image");
    let out = report
        .output
        .expect("a compressed sibling must be reported");
    assert!(out.exists(), "the compressed file must actually be written");
    assert_eq!(
        out.extension().and_then(|e| e.to_str()),
        Some("jpg"),
        "compress always writes a JPEG"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// F32 (2026-09-05): the note-formatting helper is pure - exact text, exact units
/// (bytes), same shape whether one or several images share the note.
#[test]
fn compress_shortfall_note_matches_cli_wording_and_units() {
    let one = super::compress_shortfall_note(1_000_000, 734_521, 1);
    assert_eq!(
        one,
        "cannot fit 1 image in 1000000 bytes: the smallest reachable was 734521 bytes. \
         Ask for at least 734521 bytes."
    );
    let many = super::compress_shortfall_note(5_000_000, 900_000, 3);
    assert_eq!(
        many,
        "cannot fit 3 images in 5000000 bytes: the smallest reachable was 900000 bytes. \
         Ask for at least 900000 bytes."
    );
}

/// The parser reads the exact wording `compress_to_size` produces (see its doc comment)
/// and stays `None`, never panics, on anything else - a decode-failure message included.
#[test]
fn parse_smallest_achievable_reads_the_compress_error_and_ignores_others() {
    let msg = "cannot fit in 1 bytes: the smallest JPEG this can make is 734521 bytes \
                (quality 20 at 32x32 px); nothing was written. Ask for at least 734521 bytes.";
    assert_eq!(super::parse_smallest_achievable(msg), Some(734521));
    assert_eq!(
        super::parse_smallest_achievable("decode failed: bad header"),
        None
    );
    assert_eq!(super::parse_smallest_achievable(""), None);
}

/// F32: `compress_batch_report` (the testable seam behind `handle_compress_to_size` - no
/// `CompressSize` preset is small enough to hit an unmeetable target for real) must name
/// the smallest reachable size instead of the old generic "couldn't compress some
/// images", write nothing, and report the shortfall as a real failure.
#[test]
fn compress_batch_report_names_the_smallest_reachable_size_on_an_unmeetable_target() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_actions_compress_shortfall_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // Per-pixel noise so the JPEG has real size to search over (a flat image would
    // compress to almost nothing and might satisfy even a 1-byte target's neighbors).
    let img = image::RgbImage::from_fn(96, 96, |x, y| {
        let h = (x.wrapping_mul(0x9E37_79B9) ^ y.wrapping_mul(0x85EB_CA6B)).rotate_left(7);
        image::Rgb([h as u8, (h >> 8) as u8, (h >> 16) as u8])
    });
    let src = dir.join("noise.png");
    image::DynamicImage::ImageRgb8(img).save(&src).unwrap();
    let path = src.to_str().unwrap().to_string();

    // `None` - the in-process arm - keeps this test's error text deterministic and
    // independent of whether a built `st2k.exe` happens to be resolvable here.
    let report = super::compress_batch_report(None, &[path], 1);
    assert_eq!(report.attempted, 1);
    assert_eq!(report.done, 0, "an impossible target must write nothing");
    assert!(report.output.is_none());
    let note = report.note.expect("a shortfall must produce a note");
    assert!(
        note.contains("cannot fit 1 image in 1 bytes") && note.contains("smallest reachable"),
        "{note}"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The bug: `combine_to_pdf`'s `Ok(_)` arm used to report a flat `applied(1, 1)` ("1 of 1
/// succeeded") no matter how many of the selected images actually made it into the PDF.
/// Combine 2 genuine images with 1 garbage file (same extension, so `is_image` still
/// selects it) and check the report reflects the REAL 2-of-3 outcome, with a note — not a
/// silent, misleadingly-total "succeeded".
#[test]
fn combine_to_pdf_reports_a_partial_success_when_some_inputs_are_undecodable() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_actions_combine_drop_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let good: Vec<String> = (0..2)
        .map(|i| {
            let p = dir.join(format!("good{i}.png"));
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                12,
                8,
                image::Rgb([i as u8 * 50, 40, 40]),
            ))
            .save(&p)
            .unwrap();
            p.to_str().unwrap().to_string()
        })
        .collect();
    let garbage = dir.join("garbage.png");
    std::fs::write(&garbage, b"not a png").unwrap();

    let mut paths = good;
    paths.push(garbage.to_str().unwrap().to_string());

    let report = run_action(VerbAction::CombineToPdf, &paths);
    assert_eq!(report.attempted, 3, "all 3 selected images were attempted");
    assert_eq!(
        report.done, 2,
        "only the 2 decodable images made it into the PDF"
    );
    assert!(
        report.note.is_some(),
        "a partial combine must carry an explanatory note, not report silent full success"
    );
    assert!(
        report.output.is_some(),
        "the partial PDF is still a real output to reveal"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `combine_to_cbz` used to abort the WHOLE archive on the first unreadable
/// page (`read_full_fidelity_capped(p)?` propagating straight out of `write_atomic`'s closure), unlike
/// its `combine_to_pdf` sibling. Select 2 real images + 1 path that's been deleted out
/// from under the selection (the "deleted between selection and invocation" case P40's
/// own evidence calls out) and check a partial CBZ is still produced, with a note.
#[test]
fn combine_to_cbz_reports_a_partial_success_when_a_page_cannot_be_read() {
    let dir = std::env::temp_dir().join(format!(
        "st2k_actions_cbz_drop_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let good: Vec<String> = (0..2)
        .map(|i| {
            let p = dir.join(format!("page{i}.png"));
            image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
                12,
                8,
                image::Rgb([i as u8 * 50, 40, 40]),
            ))
            .save(&p)
            .unwrap();
            p.to_str().unwrap().to_string()
        })
        .collect();
    // Selected, but gone by the time the combine actually reads it.
    let vanished = dir.join("page2.png");
    std::fs::write(&vanished, b"placeholder").unwrap();
    let vanished_str = vanished.to_str().unwrap().to_string();
    std::fs::remove_file(&vanished).unwrap();

    let mut paths = good;
    paths.push(vanished_str);

    let report = run_action(VerbAction::CombineToCbz, &paths);
    assert_eq!(report.attempted, 3, "all 3 selected images were attempted");
    assert_eq!(
        report.done, 2,
        "only the 2 readable pages made it into the CBZ"
    );
    assert!(
        report.note.is_some(),
        "a partial combine must carry an explanatory note, not report silent full success"
    );
    let out = report
        .output
        .as_ref()
        .expect("the partial CBZ is still a real output to reveal");
    assert!(out.exists(), "the partial CBZ must actually be written");

    let _ = std::fs::remove_dir_all(&dir);
}

/// Setting a folder icon must not eat the rest of desktop.ini. Explorer keeps localized
/// folder names and tooltips in the same file, and the old code replaced the whole thing.
#[test]
fn desktop_ini_merge_preserves_everything_else() {
    // Empty / missing file → just our section.
    let fresh = merge_shell_class_info("", "SageThumbsFolder.ico");
    assert_eq!(
        fresh,
        "[.ShellClassInfo]\r\nIconResource=SageThumbsFolder.ico,0\r\n\
         IconFile=SageThumbsFolder.ico\r\nIconIndex=0\r\n"
    );

    // Existing unrelated section survives, and our keys get their own section appended.
    let loc = "[LocalizedFileNames]\r\nreport.docx=@shell32.dll,-1\r\n";
    let merged = merge_shell_class_info(loc, "SageThumbsFolder.ico");
    assert!(merged.contains("[LocalizedFileNames]"), "{merged}");
    assert!(merged.contains("report.docx=@shell32.dll,-1"), "{merged}");
    assert!(merged.contains("[.ShellClassInfo]"), "{merged}");

    // An existing [.ShellClassInfo] keeps its NON-icon keys; the icon keys are replaced,
    // not duplicated.
    let prior = "[.ShellClassInfo]\r\nInfoTip=My photos\r\nIconResource=old.ico,3\r\n\
                 IconFile=old.ico\r\nIconIndex=3\r\nConfirmFileOp=0\r\n";
    let merged = merge_shell_class_info(prior, "SageThumbsFolder.ico");
    assert!(merged.contains("InfoTip=My photos"), "{merged}");
    assert!(merged.contains("ConfirmFileOp=0"), "{merged}");
    assert!(!merged.contains("old.ico"), "{merged}");
    assert_eq!(merged.matches("IconResource=").count(), 1, "{merged}");
    assert_eq!(merged.matches("[.ShellClassInfo]").count(), 1, "{merged}");

    // Section names are case-insensitive in INI files.
    let odd = "[.shellclassinfo]\r\nIconFile=old.ico\r\n";
    let merged = merge_shell_class_info(odd, "new.ico");
    assert_eq!(merged.matches("[.").count(), 1, "{merged}");
    assert!(merged.contains("IconFile=new.ico"), "{merged}");
    assert!(!merged.contains("old.ico"), "{merged}");

    // Re-running is idempotent — no key or section pile-up.
    let once = merge_shell_class_info("", "a.ico");
    assert_eq!(merge_shell_class_info(&once, "a.ico"), once);
}

#[test]
fn reveal_skips_in_place_sibling_only() {
    let dir = std::env::temp_dir().join(format!("st2k_reveal_noise_test_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("photo.png");
    std::fs::write(&src, b"src").unwrap();
    let sources = vec![src.to_string_lossy().into_owned()];

    // Convert into ▸ WebP: a file sibling next to the source → noise (no popup).
    let webp = dir.join("photo.webp");
    std::fs::write(&webp, b"out").unwrap();
    assert!(
        reveal_is_noise(&webp, &sources),
        "in-place convert must not reveal"
    );

    // Files-to-folder: a NEW directory → not noise (reveal it).
    let newfolder = dir.join("My Folder");
    std::fs::create_dir_all(&newfolder).unwrap();
    assert!(
        !reveal_is_noise(&newfolder, &sources),
        "new folder should reveal"
    );

    // A file inside a new subfolder (different parent) → reveal it.
    let moved = newfolder.join("photo.png");
    std::fs::write(&moved, b"moved").unwrap();
    assert!(
        !reveal_is_noise(&moved, &sources),
        "output in a new folder should reveal"
    );

    // Convert that wrote to a totally different folder → reveal it.
    let other_dir = dir.join("elsewhere");
    std::fs::create_dir_all(&other_dir).unwrap();
    let other = other_dir.join("photo.webp");
    std::fs::write(&other, b"o").unwrap();
    assert!(
        !reveal_is_noise(&other, &sources),
        "output in a different dir should reveal"
    );

    // A nonexistent output path is not a file → not "noise" (reveal attempt is
    // harmless; the file-exists gate is the caller's success check).
    assert!(!reveal_is_noise(&dir.join("ghost.webp"), &sources));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn routed_edits_use_the_same_honest_extension_as_in_process_edits() {
    for source in ["drawing.svg", "photo.heic", "bitmap.pbm", "mystery.unknown"] {
        assert_eq!(
            routed_edit_output_ext(std::path::Path::new(source)),
            "png",
            "{source}"
        );
    }
    for source in ["layered.psd", "texture.dds", "picture.jp2"] {
        assert_eq!(
            routed_edit_output_ext(std::path::Path::new(source)),
            source.rsplit_once('.').unwrap().1,
            "{source}"
        );
    }
    assert_eq!(
        routed_edit_output_ext(std::path::Path::new("photo.JPEG")),
        "jpeg"
    );
}

/// Two rapid launches of the SAME kind (e.g. two Convert clicks from different
/// Explorer windows in one host process) used to both compute the same
/// `st2k_<kind>_{pid}.lst` path — the second write could clobber the first before
/// the spawned app read it. The counter `launch_with_list` adds must keep every
/// call's listfile name unique, for any number of back-to-back calls.
///
/// Both halves are asserted: three distinct files on disk, AND three launches each
/// carrying its own one. The second half is the real check — the files are only
/// still there to count because [`super::intercept_launch`] swallows the spawn
/// under `cfg(test)`. Without that seam this test starts three REAL
/// `SageThumbs2K.exe --convert` processes (cargo puts the companion EXE in the same
/// `deps\` directory the test binary runs from, so `sibling_of_dll` finds it), and
/// their `read_listfile` deletes the listfiles out from under the scan: reproduced
/// here at roughly one run in two, single-threaded and alone, counting 0 or 2 of
/// the 3. Never relax this to "at least one" — the whole point is that three rapid
/// launches get three distinct names.
#[test]
fn rapid_same_kind_launches_get_distinct_listfile_names() {
    let dir = std::env::temp_dir();
    let prefix = format!("st2k_distincttest_{}_", std::process::id());
    // The pid keeps this test's listfiles distinguishable from every other test's
    // (and every other concurrent `cargo test` process's) in the shared temp dir.
    let mine = |p: &std::path::Path| -> Option<String> {
        let name = p.file_name()?.to_str()?.to_owned();
        (name.starts_with(&prefix) && name.ends_with(".lst")).then_some(name)
    };
    let scan = || -> Vec<std::path::PathBuf> {
        std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| mine(p).is_some())
            .collect()
    };
    // Clean up any leftovers from a prior failed run before asserting on counts.
    for f in scan() {
        let _ = std::fs::remove_file(f);
    }

    for _ in 0..3 {
        super::launch_with_list(
            &["a.png".to_string()],
            |_| true,
            "distincttest",
            "--convert",
        );
    }

    let files = scan();
    let mut on_disk: Vec<String> = files.iter().filter_map(|p| mine(p.as_path())).collect();
    on_disk.sort();
    assert_eq!(
        on_disk.len(),
        3,
        "three same-kind launches must produce three distinct listfiles, got {on_disk:?}"
    );

    // …and every one of those files must have been handed to a launch of its own.
    // Filtered to our own pid: the probe log is process-wide and other tests may be
    // recording into it in parallel.
    let ours: Vec<(String, String)> = super::launch_probe::recorded()
        .into_iter()
        .filter_map(|argv| {
            let name = mine(std::path::Path::new(argv.get(1)?))?;
            Some((argv.first()?.clone(), name))
        })
        .collect();
    for (flag, name) in &ours {
        assert_eq!(
            flag, "--convert",
            "{name} must reach the app behind its flag"
        );
    }
    let mut launched: Vec<String> = ours.into_iter().map(|(_, name)| name).collect();
    launched.sort();
    assert_eq!(
        launched, on_disk,
        "each listfile written must be the one its own launch passed to the app"
    );

    for f in &files {
        let _ = std::fs::remove_file(f);
    }
}
