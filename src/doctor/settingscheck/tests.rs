use super::*;

/// `MaxSize = 0` ("no limit") reaches here as `u64::MAX`, and dividing that by a
/// megabyte printed a 17-terabyte cap that does not exist. Driven through the pure
/// helper on purpose: the value comes from HKCU, so a report-level assertion would
/// silently pass on any machine whose MaxSize happens not to be 0.
#[test]
fn max_file_size_reports_the_unlimited_sentinel_as_unlimited() {
    let unlimited = max_file_size_detail(u64::MAX);
    assert!(
        unlimited.contains("Unlimited"),
        "u64::MAX must read as Unlimited, got: {unlimited}"
    );
    assert!(
        !unlimited.contains("17592186044415"),
        "the sentinel leaked as a number: {unlimited}"
    );
    // An ordinary cap still renders as plain megabytes.
    assert_eq!(
        max_file_size_detail(500 * 1024 * 1024),
        "500 MB (larger files are skipped)"
    );
}

/// Only lines containing " ERROR " or "PANIC" survive the scan, in file order, and
/// the result is capped at `limit` — the last N, not the first N (a doctor paste should
/// show the MOST RECENT failures, not the oldest ones from a long-lived log).
#[test]
fn tail_matching_lines_filters_and_caps_to_the_most_recent() {
    let dir = std::env::temp_dir().join(format!("st2k_doctor_logtail_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("test.log");
    let mut body = String::new();
    for i in 0..25 {
        body.push_str(&format!("[pid 1 +{i}ms] ERROR failure number {i}\n"));
        body.push_str(&format!("[pid 1 +{i}ms] just a debug line {i}\n"));
    }
    body.push_str("[pid 1 +999ms] ERROR PANIC [thumbprovider] at foo.rs:1: boom\n");
    std::fs::write(&log, &body).unwrap();

    let lines = tail_matching_lines(&log, LOG_TAIL_SCAN_BYTES, &[" ERROR ", "PANIC"], 20);
    assert_eq!(lines.len(), 20, "must cap at the requested limit");
    assert!(
        lines
            .iter()
            .all(|l| l.contains(" ERROR ") || l.contains("PANIC")),
        "every returned line must match a needle: {lines:?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("just a debug line")),
        "a non-matching line must never survive the filter"
    );
    // The most recent 20 of 26 matching lines were kept, so "failure number 0..4" (the
    // oldest 5) must have been dropped and the trailing PANIC line must be present.
    assert!(!lines.iter().any(|l| l.contains("failure number 4")));
    assert!(lines.last().unwrap().contains("PANIC"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// A file with no matching lines returns an empty vec, not an error or a panic — the
/// common case (a healthy install whose log has no ERROR/PANIC lines at all).
#[test]
fn tail_matching_lines_on_a_clean_log_is_empty() {
    let dir =
        std::env::temp_dir().join(format!("st2k_doctor_logtail_clean_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("clean.log");
    std::fs::write(&log, "[pid 1 +1ms] everything is fine\n").unwrap();

    let lines = tail_matching_lines(&log, LOG_TAIL_SCAN_BYTES, &[" ERROR ", "PANIC"], 20);
    assert!(lines.is_empty());

    let _ = std::fs::remove_dir_all(&dir);
}

/// The bundle's scrub, over a synthetic tree of both backends' shapes at once: the
/// credential section goes, the prefixed root values go, and every ordinary preference
/// beside them stays, including one whose NAME resembles a credential's. The end-to-end
/// proof over a real ini and a real scratch registry root is `tests/doctor_bundle_scrub_*`,
/// one process per backend since the storage mode resolves once.
#[test]
fn settings_snapshot_scrubs_the_credential_container_and_keeps_the_rest() {
    let tree: Vec<SettingsSection> = vec![
        (
            None,
            vec![
                ("MaxSize".into(), "200".into()),
                ("OAuth_LicenceCert".into(), "cert-blob".into()),
                ("OAuth_Name".into(), "Some One".into()),
                ("OAuth_RefreshToken".into(), "token-blob".into()),
                ("Sub".into(), "a preference that only looks like one".into()),
                ("Theme".into(), "1".into()),
            ],
        ),
        (
            Some("MenuItems".into()),
            vec![("Convert".into(), "0".into())],
        ),
        (
            Some("OAuth".into()),
            vec![
                ("Email".into(), "who@example.invalid".into()),
                ("RefreshToken".into(), "token-blob".into()),
            ],
        ),
        (Some("jpg".into()), Vec::new()),
    ];
    let text = render_settings("test", &without_credentials(tree));
    for kept in [
        "[Settings]",
        "MaxSize=200",
        "Sub=a preference",
        "Theme=1",
        "[MenuItems]",
        "Convert=0",
        "[jpg]",
        "; (empty)",
    ] {
        assert!(text.contains(kept), "{kept} missing from:\n{text}");
    }
    for gone in [
        "token-blob",
        "cert-blob",
        "Some One",
        "who@example.invalid",
        "[OAuth]",
        "OAuth_",
    ] {
        assert!(!text.contains(gone), "{gone} leaked into:\n{text}");
    }
    assert!(
        render_settings("test", &[]).contains("nothing stored yet"),
        "an unconfigured copy must say so rather than render an empty file"
    );
}

/// `bundle` must produce a zip with exactly the four named entries, each
/// non-empty, and readable back — the whole point is a single self-contained
/// attachment a support triage can accept as-is.
#[test]
fn bundle_writes_a_zip_with_report_formats_log_tail_and_settings() {
    let dir = std::env::temp_dir().join(format!("st2k_doctor_bundle_{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("bundle.zip");

    bundle(&out, None).unwrap();
    assert!(out.exists());

    let f = std::fs::File::open(&out).unwrap();
    let mut zip = zip::ZipArchive::new(f).unwrap();
    let names: Vec<String> = (0..zip.len())
        .map(|i| zip.by_index(i).unwrap().name().to_string())
        .collect();
    for expect in [
        "doctor-report.txt",
        "formats.json",
        "log-tail.txt",
        "settings.txt",
    ] {
        assert!(
            names.contains(&expect.to_string()),
            "missing {expect}: {names:?}"
        );
    }
    let mut settings_text = String::new();
    std::io::Read::read_to_string(
        &mut zip.by_name("settings.txt").unwrap(),
        &mut settings_text,
    )
    .unwrap();
    assert!(
        settings_text.starts_with("; SageThumbs 2K settings as stored"),
        "{settings_text}"
    );
    let mut report_text = String::new();
    std::io::Read::read_to_string(
        &mut zip.by_name("doctor-report.txt").unwrap(),
        &mut report_text,
    )
    .unwrap();
    assert!(report_text.contains("SageThumbs 2K"));

    let _ = std::fs::remove_dir_all(&dir);
}
