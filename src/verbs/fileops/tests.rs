use super::*;

/// A writable directory on a fixed volume OTHER than `dir`'s, when this machine has one
/// (the dev box: D: source, C: temp; GitHub's Windows runners: C: and D:). None when it
/// does not - the test then proves nothing and says so rather than failing.
fn other_volume_dir(dir: &Path, tag: &str) -> Option<PathBuf> {
    let here = dir
        .components()
        .next()
        .map(|c| c.as_os_str().to_string_lossy().to_ascii_uppercase())?;
    for letter in ('C'..='H').chain('R'..='Z') {
        let root = format!("{letter}:\\");
        if root.to_ascii_uppercase().starts_with(&here) || !Path::new(&root).is_dir() {
            continue;
        }
        let candidate =
            PathBuf::from(&root).join(format!("st2k_xvol_{tag}_{}", std::process::id()));
        if std::fs::create_dir_all(&candidate).is_ok()
            && std::fs::write(candidate.join("probe"), b"x").is_ok()
        {
            let _ = std::fs::remove_file(candidate.join("probe"));
            return Some(candidate);
        }
        let _ = std::fs::remove_dir_all(&candidate);
    }
    None
}

/// 2026-09-19 audit concern 7: Tags to folders with Move and a destination on another
/// drive reported `(0, 1)` - every file skipped - while Copy worked, because `rename`
/// cannot cross volumes. Move must land the bytes on the other volume and remove the
/// source, exactly as Explorer's own drag-to-another-drive does.
#[test]
fn move_into_crosses_volumes() {
    let src_dir = std::env::temp_dir().join(format!("st2k_xvol_src_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&src_dir);
    std::fs::create_dir_all(&src_dir).unwrap();
    let Some(dst_dir) = other_volume_dir(&src_dir, "dst") else {
        eprintln!("move_into_crosses_volumes: no second writable volume here; NOT MEASURED");
        let _ = std::fs::remove_dir_all(&src_dir);
        return;
    };
    let src = src_dir.join("track.bin");
    std::fs::write(&src, b"cross-volume payload").unwrap();

    let landed = move_into(&src, &dst_dir).expect("move across volumes");
    assert_eq!(landed, dst_dir.join("track.bin"));
    assert_eq!(std::fs::read(&landed).unwrap(), b"cross-volume payload");
    assert!(
        !src.exists(),
        "the source must be gone after a completed move"
    );

    // A second file of the same name still dodges the collision on the far volume.
    std::fs::write(&src, b"second").unwrap();
    let landed2 = move_into(&src, &dst_dir).expect("second move");
    assert_ne!(landed2, landed);
    assert_eq!(std::fs::read(&landed2).unwrap(), b"second");

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dst_dir);
}

/// The whole verb, across volumes, the way the audit measured it: Move now counts the
/// file as done and Copy keeps behaving as before.
#[test]
fn tags_to_folders_move_across_volumes_counts_the_file_as_done() {
    let src_dir = std::env::temp_dir().join(format!("st2k_xvol_ttf_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&src_dir);
    std::fs::create_dir_all(&src_dir).unwrap();
    let Some(dest) = other_volume_dir(&src_dir, "ttf") else {
        eprintln!(
            "tags_to_folders_move_across_volumes: no second writable volume here; NOT MEASURED"
        );
        let _ = std::fs::remove_dir_all(&src_dir);
        return;
    };
    let a = src_dir.join("a.mp3");
    std::fs::write(&a, b"not really audio").unwrap();
    let files = vec![a.to_string_lossy().into_owned()];
    // A constant template needs no tags at all.
    assert_eq!(
        tags_to_folders(&files, &dest, "sorted", "Unknown", true),
        (1, 0)
    );
    assert_eq!(
        std::fs::read(dest.join("sorted").join("a.mp3")).unwrap(),
        b"not really audio"
    );
    assert!(!a.exists(), "Move must remove the source");

    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&dest);
}

fn png(dir: &Path, name: &str, w: u32, h: u32) -> String {
    let p = dir.join(name);
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(w, h, image::Rgb([1, 2, 3])))
        .save(&p)
        .unwrap();
    p.to_string_lossy().into_owned()
}

/// Kavita/Komga/YACReader read `ComicInfo.xml`, and the community CBZ RFC
/// wants it FIRST so a reader does not have to scan the archive for it.
#[test]
fn cbz_leads_with_comicinfo_and_describes_every_page() {
    let dir = std::env::temp_dir().join(format!("st2k_cbzinfo_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    // Deliberately out of lexical order: page 2 must land before page 10.
    let imgs = vec![
        png(&dir, "page10.png", 30, 40),
        png(&dir, "page2.png", 10, 20),
        png(&dir, "page1.png", 50, 60),
    ];
    let out = dir.join("book.cbz");
    combine_to_cbz(&imgs, &out, OnOmit::Report).unwrap();

    let f = std::fs::File::open(&out).unwrap();
    let mut zip = zip::ZipArchive::new(f).unwrap();
    assert_eq!(zip.len(), 4, "3 pages + the sidecar");
    assert_eq!(
        zip.by_index(0).unwrap().name(),
        "ComicInfo.xml",
        "the sidecar must be the first entry"
    );

    let xml = {
        use std::io::Read;
        let mut e = zip.by_index(0).unwrap();
        let mut s = String::new();
        e.read_to_string(&mut s).unwrap();
        s
    };
    assert!(xml.contains("<PageCount>3</PageCount>"), "{xml}");
    assert!(
        xml.contains(r#"<Page Image="0" Type="FrontCover" ImageWidth="50" ImageHeight="60" />"#),
        "page 1 is the cover and carries its real size: {xml}"
    );
    assert!(
        xml.contains(r#"<Page Image="1" ImageWidth="10" ImageHeight="20" />"#),
        "natural sort must put page2 second: {xml}"
    );
    assert!(
        xml.contains(r#"<Page Image="2" ImageWidth="30" ImageHeight="40" />"#),
        "page10 sorts last, not second: {xml}"
    );
    assert!(
        !xml.contains("Image=\"3\""),
        "the sidecar counted itself as a page"
    );

    assert_eq!(zip.by_index(1).unwrap().name(), "001_page1.png");
    assert_eq!(zip.by_index(2).unwrap().name(), "002_page2.png");
    assert_eq!(zip.by_index(3).unwrap().name(), "003_page10.png");

    let _ = std::fs::remove_dir_all(&dir);
}

/// A page we cannot measure from its header prefix still gets a `<Page>` row,
/// just without the two optional size attributes.
#[test]
fn unmeasurable_page_still_gets_a_row() {
    let xml = comic_info_xml(&[Some((8, 9)), None]);
    assert!(xml.contains("<PageCount>2</PageCount>"));
    assert!(xml.contains(r#"<Page Image="1" />"#), "{xml}");
}

/// `files_to_folder` must not conjure a missing PARENT via `create_dir_all` — that
/// is the same "silently create/merge past the check" bug family the switch to a
/// non-recursive, atomic `create_dir` closes. The literal microsecond-scale TOCTOU
/// race (another process landing the folder in the gap between the exists check and
/// the create) can't be reproduced deterministically in-process, but this proves the
/// mechanism: `create_dir` never touches ancestors, `create_dir_all` silently would.
#[test]
fn files_to_folder_never_creates_missing_ancestor_directories() {
    let base = std::env::temp_dir().join(format!("st2k_f2f_missing_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let ghost_parent = base.join("ghost"); // deliberately never created
    let phantom_file = ghost_parent.join("photo.png");

    let _ = files_to_folder(&[phantom_file.to_string_lossy().into_owned()], "New folder");

    assert!(
        !ghost_parent.exists(),
        "create_dir must not silently conjure the missing parent chain the way \
         create_dir_all would"
    );
    let _ = std::fs::remove_dir_all(&base);
}

/// A folder literally named after a DOS device (or `<device>.ext`) must fall back to
/// "image" like the empty-input case — Windows can never create such a path, and the
/// prior failure was opaque (an unexplained create error deep in `files_to_folder`).
#[test]
fn sanitize_component_rejects_reserved_device_names_case_insensitively() {
    assert_eq!(sanitize_component("Con"), "image");
    assert_eq!(sanitize_component("con"), "image");
    assert_eq!(
        sanitize_component("CON.txt"),
        "image",
        "the stem is reserved too"
    );
    assert_eq!(sanitize_component("COM1"), "image");
    assert_eq!(sanitize_component("lpt9"), "image");
    // Merely CONTAINING a reserved word is fine — only an exact stem match blocks.
    assert_eq!(sanitize_component("Constitution"), "Constitution");
    assert_eq!(sanitize_component("My Con Notes"), "My Con Notes");
}

/// A tag value that happens to literally contain another placeholder token must not
/// get re-expanded when THAT token's own turn comes — each `$token` in the TEMPLATE
/// is substituted at most once, and only the template's own text is scanned.
#[test]
fn expand_template_does_not_re_expand_a_substituted_value() {
    let tags = crate::strip::AudioTags {
        artist: Some("$title".to_string()),
        title: Some("Real Title".to_string()),
        ..Default::default()
    };
    let out = expand_template("$artist - $title", &tags, "missing");
    assert_eq!(
        out, "$title - Real Title",
        "the literal text substituted for $artist must not be re-scanned for $title"
    );
}

/// A `copy_into` failure must remove whatever landed at the reserved destination,
/// even if the write got partway through and left a non-empty, truncated file —
/// `OutSlot`'s Drop only cleans up a still-EMPTY placeholder, so this can't be left
/// to Drop alone.
#[test]
fn cleanup_failed_dest_removes_a_non_empty_truncated_leftover() {
    let dir = std::env::temp_dir().join(format!("st2k_cleanup_dest_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("partial.bin");
    std::fs::write(&p, b"partial data from a failed copy, not empty").unwrap();
    assert!(p.exists());

    cleanup_failed_dest(&p);

    assert!(
        !p.exists(),
        "a non-empty truncated leftover must still be removed, not just an empty one"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// One unreadable page must not abort the whole CBZ — a page selected but
/// deleted before the combine runs (the concrete case P40's evidence calls out) has
/// to drop out and be COUNTED, with the rest still written.
#[test]
fn combine_to_cbz_drops_and_counts_an_unreadable_page_instead_of_aborting() {
    let dir = std::env::temp_dir().join(format!("st2k_cbz_drop_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let good = vec![
        png(&dir, "page1.png", 10, 10),
        png(&dir, "page3.png", 10, 10),
    ];
    // Selected, but gone by read time.
    let vanished = dir.join("page2.png");
    std::fs::write(&vanished, b"placeholder").unwrap();
    let vanished = vanished.to_string_lossy().into_owned();
    std::fs::remove_file(&vanished).unwrap();

    let mut imgs = good;
    imgs.push(vanished);
    let out = dir.join("book.cbz");

    let combined = combine_to_cbz(&imgs, &out, OnOmit::Report).unwrap();
    assert_eq!(
        combined.omitted.len(),
        1,
        "exactly the one unreadable page must be counted dropped"
    );
    // 2026-09-05 audit, F31: the omission names the page and says WHY.
    assert_eq!(combined.omitted[0].input, imgs[2]);
    assert_eq!(combined.omitted[0].cause, OmitCause::Unreadable);
    assert_eq!(combined.used, 2);
    assert_eq!(combined.output, out);

    let f = std::fs::File::open(&out).unwrap();
    let zip = zip::ZipArchive::new(f).unwrap();
    assert_eq!(
        zip.len(),
        3,
        "sidecar + the 2 readable pages, not aborted to nothing"
    );

    // 2026-09-05 audit, F31: strict writes nothing and lists the same page.
    let strict_out = dir.join("strict.cbz");
    let err = combine_to_cbz(&imgs, &strict_out, OnOmit::Fail)
        .expect_err("strict must refuse a partial archive");
    assert!(!strict_out.exists(), "strict must not write a partial CBZ");
    assert!(err.message().contains("omitted\t"), "{err}");

    // 2026-09-05 audit, F30: the finished CBZ as both an input and the output (a case
    // variant of its own path) is refused before anything is read, bytes untouched.
    // Against the pre-fix code this call succeeds and rewrites the archive over itself.
    let before = std::fs::read(&out).unwrap();
    let mut aliased = imgs.clone();
    aliased.push(out.to_string_lossy().into_owned());
    let upper = dir.join("BOOK.CBZ");
    let err =
        combine_to_cbz(&aliased, &upper, OnOmit::Report).expect_err("the output aliases an input");
    assert!(err.message().contains("same file"), "{err}");
    assert_eq!(
        std::fs::read(&out).unwrap(),
        before,
        "the source CBZ was modified"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `files_to_folder` must report which of `paths` actually moved, not just
/// "at least one moved". Two real files + one already-gone path (the same "deleted
/// between selection and invocation" shape G27's evidence describes) must come back
/// as moved=2, skipped=1 — not a bare `Ok` a caller could mistake for total success.
#[test]
fn files_to_folder_reports_moved_and_skipped_counts() {
    let dir = std::env::temp_dir().join(format!("st2k_f2f_counts_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let mut paths = Vec::new();
    for n in ["a.txt", "b.txt"] {
        let p = dir.join(n);
        std::fs::write(&p, b"x").unwrap();
        paths.push(p.to_str().unwrap().to_string());
    }
    // Selected, but gone by move time.
    let vanished = dir.join("c.txt");
    std::fs::write(&vanished, b"x").unwrap();
    let vanished_str = vanished.to_str().unwrap().to_string();
    std::fs::remove_file(&vanished).unwrap();
    paths.push(vanished_str);

    let (folder, moved, skipped) = files_to_folder(&paths, "Group").unwrap();
    assert_eq!(folder, dir.join("Group"));
    assert_eq!(moved, 2, "the two real files must be counted moved");
    assert_eq!(
        skipped, 1,
        "the vanished file must be counted skipped, not silently ignored"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// This fix only removes a bucket dir THIS call created — this pins the other,
/// equally important half: something already sitting at the bucket's path (here, a
/// plain file blocking `create_dir_all`, so the move never even starts) must be left
/// completely alone. A cleanup that fired unconditionally on any failed move — rather
/// than only for a bucket this call is responsible for — would delete a user's
/// unrelated file here instead.
#[test]
fn sort_by_dimensions_never_touches_a_pre_existing_non_directory_at_the_bucket_path() {
    let dir = std::env::temp_dir().join(format!("st2k_dims_cleanup_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // A bucket dir already occupied by a SAME-NAMED file (not a directory) makes
    // `create_dir_all` fail for that one image's move, without touching anything else.
    let bucket_path = dir.join("5x5");
    std::fs::write(&bucket_path, b"in the way").unwrap();
    let img = png(&dir, "photo.png", 5, 5);

    let (moved, skipped) = sort_by_dimensions(&[img]);
    assert_eq!(moved, 0);
    assert_eq!(skipped, 1);
    assert!(
        bucket_path.is_file(),
        "a pre-existing blocker at the bucket's path must never be removed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The other half of G105: an empty bucket dir this SAME call created for a file
/// whose move then fails must be removed, not left behind as junk. Made deterministic
/// (no race) by holding the SOURCE open for DELETE access is denied for the entire
/// call: `dims()`'s read still succeeds (reads stay shared), the bucket gets created
/// for it, but the rename out of it fails with a sharing violation every time while
/// the handle is held.
#[test]
fn sort_by_dimensions_removes_a_bucket_it_created_when_the_move_into_it_fails() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = std::env::temp_dir().join(format!("st2k_dims_cleanup2_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let img = png(&dir, "photo.png", 5, 5);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ only — no FILE_SHARE_DELETE
        .open(Path::new(&img))
        .unwrap();

    let (moved, skipped) = sort_by_dimensions(&[img]);
    drop(held);

    assert_eq!(
        moved, 0,
        "the rename must fail while the source denies delete access"
    );
    assert_eq!(skipped, 1);
    assert!(
        !dir.join("5x5").exists(),
        "the bucket this call created must be removed after its only move failed"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Pure unit coverage for the ownership decision itself (2026-09-05 audit, F14): a
/// successful `create_dir` means we own the directory we just made; `AlreadyExists`
/// (the shape a raced-away creation attempt returns) means we do not, whatever a separate
/// `exists()` check might have said a moment earlier.
#[test]
fn owns_new_dir_is_true_only_for_a_create_this_call_actually_performed() {
    let created: std::io::Result<()> = Ok(());
    assert!(owns_new_dir(&created));

    let raced_away: std::io::Result<()> =
        Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    assert!(!owns_new_dir(&raced_away));
}

/// F14's actual race, reproduced for real rather than simulated: two threads call
/// `claim_bucket_dir` on the SAME path, released together by a `Barrier` so the OS sees
/// both `create_dir` attempts as close to simultaneous as it can. `create_dir` is atomic
/// at the OS level, so exactly one of them must come back `owned == true` however the
/// scheduler interleaves them - this is deterministic, not a timing gamble.
///
/// Revert `claim_bucket_dir` to the pre-fix shape (`let bucket_is_new = !dir.exists();`
/// then `create_dir_all(&dir)`) and this test can fail: with the barrier forcing both
/// threads to reach the `exists()` check before either has created anything, BOTH observe
/// "not there yet" and BOTH get `bucket_is_new = true` - `create_dir_all` succeeds
/// unconditionally for both since it treats an already-present directory as a no-op - so
/// `owners` comes back 2, not 1. That double ownership is exactly the F14 bug: either side
/// believes it alone made the folder and may remove it out from under the other's use of
/// it on a later failed move.
#[test]
fn claim_bucket_dir_gives_ownership_to_exactly_one_racing_caller() {
    let dir = std::env::temp_dir().join(format!("st2k_dims_race_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let bucket = std::sync::Arc::new(dir.join("bucket"));
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

    let handles: Vec<_> = (0..2)
        .map(|_| {
            let bucket = std::sync::Arc::clone(&bucket);
            let barrier = std::sync::Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                claim_bucket_dir(&bucket)
            })
        })
        .collect();
    let results: Vec<(bool, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();

    assert!(
        results.iter().all(|(usable, _)| *usable),
        "both racing callers must see a usable directory afterwards"
    );
    let owners = results.iter().filter(|(_, owned)| *owned).count();
    assert_eq!(
        owners, 1,
        "exactly one racing caller must own the directory it created"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The end-to-end shape of F14: a bucket directory that ALREADY EXISTS before this call
/// even starts (standing in for one a genuinely concurrent creator just won, or a leftover
/// from an earlier run) must survive when this call's own move into it then fails. This is
/// the mirror of the pre-existing-file test above, but for the exact case F14 is about: a
/// pre-existing DIRECTORY, not a blocking file.
#[test]
fn sort_by_dimensions_never_removes_a_bucket_it_did_not_create_when_the_move_into_it_fails() {
    use std::os::windows::fs::OpenOptionsExt;

    let dir = std::env::temp_dir().join(format!("st2k_dims_notowned_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    // Simulate "someone else already made this bucket" by creating it before the call.
    let bucket_path = dir.join("5x5");
    std::fs::create_dir_all(&bucket_path).unwrap();

    let img = png(&dir, "photo.png", 5, 5);
    let held = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(1) // FILE_SHARE_READ only - no FILE_SHARE_DELETE
        .open(Path::new(&img))
        .unwrap();

    let (moved, skipped) = sort_by_dimensions(&[img]);
    drop(held);

    assert_eq!(
        moved, 0,
        "the rename must fail while the source denies delete access"
    );
    assert_eq!(skipped, 1);
    assert!(
        bucket_path.is_dir(),
        "a bucket this call did not create must survive its own failed move"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `date_taken_folder_name` reads the exact same `"YYYY-MM-DD HH.MM.SS"` shape
/// `RenamePattern::DateTaken` renames by, and takes just the date half.
#[test]
fn date_taken_folder_name_takes_the_date_half_of_the_capture_time() {
    assert_eq!(
        "2024-03-07 12.30.00".split_once(' ').map(|(d, _)| d),
        Some("2024-03-07")
    );
}

/// A plain PNG carries no EXIF capture date, so `sort_by_date_taken` must skip it
/// (not create a folder, not move it) — the same "no metadata → skip and count"
/// contract `sort_by_dimensions` has for an unreadable image.
#[test]
fn sort_by_date_taken_skips_a_file_with_no_capture_date() {
    let dir = std::env::temp_dir().join(format!("st2k_datetaken_skip_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let img = png(&dir, "photo.png", 5, 5);
    let (moved, skipped) = sort_by_date_taken(&[img]);
    assert_eq!(moved, 0, "no capture date means nothing gets moved");
    assert_eq!(skipped, 1);
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        1,
        "no date-named bucket folder should have been created"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
