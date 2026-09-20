#![cfg(test)]

use super::*;

/// A file that landed at SOME sizes and missed others must not report as done. This is the
/// whole point of the Partial verdict: the old code collapsed it to Built, so a run claimed
/// 100% while Explorer still had to extract the missing bucket on first browse (issue #26).
#[test]
fn a_missed_size_is_never_reported_as_built() {
    assert!(matches!(
        verdict(true, false, false),
        Some(Outcome::Partial)
    ));
    assert!(matches!(
        verdict(false, true, false),
        Some(Outcome::Partial)
    ));
    assert!(matches!(verdict(true, true, false), Some(Outcome::Partial)));
}

/// The clean outcomes still say what they always said, so the summary a user reads for a
/// healthy run is unchanged.
#[test]
fn a_complete_file_reports_built_or_already() {
    assert!(matches!(verdict(true, false, true), Some(Outcome::Built)));
    assert!(matches!(verdict(false, true, true), Some(Outcome::Already)));
    // Built wins over already: some size needed real work, which is what happened.
    assert!(matches!(verdict(true, true, true), Some(Outcome::Built)));
}

/// Nothing anywhere is the caller's Failed path, and it must stay distinguishable from
/// Partial — "we got some of it" and "we got none of it" are different user-facing answers.
#[test]
fn nothing_anywhere_is_not_a_partial() {
    assert!(verdict(false, false, false).is_none());
    assert!(verdict(false, false, true).is_none());
}

/// PROVE THE PREMISE, don't assume it. Everything above rests on the claim that Windows
/// really does turn our registered command into the argument `E:"` for a drive root. That
/// is a claim about `CommandLineToArgvW`'s escaping rules, so ask `CommandLineToArgvW`.
///
/// Without this, the fix could be repairing a mangling that never happens (and quietly
/// corrupting nothing, but also fixing nothing) and every other test here would still pass,
/// because they all take the mangled string as a given.
#[test]
fn windows_really_does_mangle_a_quoted_drive_root() {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::CommandLineToArgvW;

    // Exactly what `foldermenu::apply` writes, with `%1` substituted by the shell.
    let parse = |line: &str| -> Vec<String> {
        let wide: Vec<u16> = line.encode_utf16().chain(std::iter::once(0)).collect();
        let mut argc = 0i32;
        unsafe {
            let argv = CommandLineToArgvW(PCWSTR(wide.as_ptr()), &mut argc);
            assert!(!argv.is_null(), "CommandLineToArgvW failed on {line}");
            let out = (0..argc as usize)
                .map(|i| (*argv.add(i)).to_string().expect("argv is valid UTF-16"))
                .collect();
            let _ = windows::Win32::Foundation::LocalFree(Some(
                windows::Win32::Foundation::HLOCAL(argv.cast()),
            ));
            out
        }
    };

    let exe = r"C:\Program Files\SageThumbs2K\SageThumbs2K.exe";

    // An ordinary folder: three clean arguments, the path intact. This is why the entry
    // always worked here.
    let ok = parse(&format!("\"{exe}\" --prebuild \"E:\\Photos\""));
    assert_eq!(ok.len(), 3, "ordinary folder should parse cleanly: {ok:?}");
    assert_eq!(ok[2], r"E:\Photos");

    // A drive root: the trailing `\"` is read as an escaped quote, so the path is
    // destroyed. THIS is issue #26.1, demonstrated rather than asserted.
    let broken = parse(&format!("\"{exe}\" --prebuild \"E:\\\""));
    assert_eq!(
        broken[2], "E:\"",
        "the premise of unmangle_shell_path no longer holds: Windows parsed the drive root \
         as {:?} rather than the expected mangled `E:\"`",
        broken[2]
    );
    assert!(
        !std::path::Path::new(&broken[2]).exists(),
        "the mangled argument must be a path that cannot exist, which is why the verb \
         silently did nothing"
    );

    // And the repair turns that back into the drive root the user right-clicked.
    assert_eq!(unmangle_shell_path(&broken[2]), r"E:\");
}

/// A drive root reaches us as `E:"`, because Explorer substituted `E:\` into a quoted
/// token and `CommandLineToArgvW` then ate the backslash as a quote escape. This is the
/// whole of issue #26.1: the entry worked on every folder and did nothing on a drive.
#[test]
fn a_mangled_drive_root_is_repaired() {
    assert_eq!(unmangle_shell_path("E:\""), r"E:\");
    assert_eq!(unmangle_shell_path("C:\""), r"C:\");
    // A UNC share root mangles identically and repairs identically.
    assert_eq!(
        unmangle_shell_path("\\\\server\\share\""),
        r"\\server\share\"
    );
}

/// Ordinary folders are NOT touched. `%1` only produces a trailing backslash for a root,
/// so every normal path must come through byte-for-byte — including one ending in a
/// quote-free backslash, and one containing the spaces the quoting exists for.
#[test]
fn ordinary_folder_paths_pass_through_untouched() {
    for p in [
        r"E:\Photos",
        r"C:\Users\sam\My Pictures",
        r"D:\a b\c d\e",
        r"E:\Photos\", // already correct: no quote, so nothing to repair
        "",
    ] {
        assert_eq!(unmangle_shell_path(p), p, "must not rewrite {p}");
    }
}

/// `OFFLINE_ATTRS` must cover all three HSM/cloud-placeholder flags `doctor.rs` enumerates
/// (OFFLINE, RECALL_ON_OPEN, RECALL_ON_DATA_ACCESS) — a file carrying only RECALL_ON_OPEN
/// used to sail past this mask and get hydrated/downloaded by WTS_EXTRACT.
#[test]
fn offline_attrs_mask_covers_all_three_recall_flags() {
    const OFFLINE: u32 = 0x0000_1000;
    const RECALL_ON_OPEN: u32 = 0x0004_0000;
    const RECALL_ON_DATA_ACCESS: u32 = 0x0040_0000;
    for (name, flag) in [
        ("OFFLINE", OFFLINE),
        ("RECALL_ON_OPEN", RECALL_ON_OPEN),
        ("RECALL_ON_DATA_ACCESS", RECALL_ON_DATA_ACCESS),
    ] {
        assert!(
            OFFLINE_ATTRS & flag != 0,
            "OFFLINE_ATTRS must include {name} ({flag:#010x})"
        );
    }
}

/// The walk must pick up supported files, honour `recurse`, and never wander into a
/// junction — the loop guard that keeps "pre-build my D: drive" from never terminating.
#[test]
fn walk_is_shallow_by_default_and_deep_on_request() {
    let root = std::env::temp_dir().join(format!("st2k-prebuild-{}", std::process::id()));
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).expect("scratch tree");
    std::fs::write(root.join("a.png"), b"x").expect("a");
    std::fs::write(sub.join("b.png"), b"x").expect("b");
    std::fs::write(root.join("notes.txt"), b"x").expect("txt");

    let mut rep = Report::default();
    let snap = crate::settings::format_enabled_snapshot();
    let mut shallow = Vec::new();
    walk(&root, &Options::default(), 0, &mut shallow, &mut rep, &snap);
    assert_eq!(shallow.len(), 1, "non-recursive must stop at the top level");
    assert!(
        shallow[0].ends_with("a.png"),
        "and must skip the unsupported .txt"
    );

    let mut deep = Vec::new();
    let opts = Options {
        recurse: true,
        ..Default::default()
    };
    walk(&root, &opts, 0, &mut deep, &mut rep, &snap);
    assert_eq!(deep.len(), 2, "recursive must reach the subfolder");

    let _ = std::fs::remove_dir_all(&root);
}

/// The depth cap is the only thing standing between a junction cycle and an endless walk.
#[test]
fn walk_stops_at_the_depth_cap() {
    let root = std::env::temp_dir().join(format!("st2k-depth-{}", std::process::id()));
    let deep = root.join("a").join("b").join("c");
    std::fs::create_dir_all(&deep).expect("tree");
    std::fs::write(deep.join("x.png"), b"x").expect("x");

    let mut rep = Report::default();
    let snap = crate::settings::format_enabled_snapshot();
    let mut out = Vec::new();
    let opts = Options {
        recurse: true,
        max_depth: 1,
        ..Default::default()
    };
    walk(&root, &opts, 0, &mut out, &mut rep, &snap);
    assert!(out.is_empty(), "a file below the cap must not be collected");

    let _ = std::fs::remove_dir_all(&root);
}

/// A relative path has to become the absolute, non-extended form the shell parses, or
/// every item fails with FILE_NOT_FOUND and the run reports a 100% failure rate.
#[test]
fn parsing_path_is_absolute_and_carries_no_extended_prefix() {
    let f = std::env::temp_dir().join(format!("st2k-pp-{}.png", std::process::id()));
    std::fs::write(&f, b"x").expect("write");
    let got = parsing_path(&f.to_string_lossy());
    assert!(
        !got.starts_with(r"\\?\"),
        "extended prefix must be stripped"
    );
    assert!(Path::new(&got).is_absolute(), "must be absolute");
    let _ = std::fs::remove_file(&f);
}

/// Requested edges must land on real cache buckets, and two requests that resolve to the
/// same bucket must collapse — otherwise the run extracts the same thumbnail twice and
/// the report claims work that never happened.
#[test]
fn sizes_snap_to_cache_buckets_and_dedupe() {
    assert_eq!(
        normalize_sizes(&[256]),
        vec![256],
        "an exact bucket is kept"
    );
    assert_eq!(
        normalize_sizes(&[200]),
        vec![256],
        "a request rounds UP to the bucket that will hold it"
    );
    assert_eq!(
        normalize_sizes(&[100, 200, 250]),
        vec![256],
        "three requests inside one bucket collapse to a single extraction"
    );
    assert_eq!(
        normalize_sizes(&[768, 96, 256]),
        vec![96, 256, 768],
        "order is normalised so the report reads predictably"
    );
    assert_eq!(
        normalize_sizes(&[99_999]),
        vec![2560],
        "anything past the top bucket clamps to it rather than being dropped"
    );
    // The three buckets the corrected list added or fixed. 1024 is NOT a Windows 10/11
    // bucket (it was Windows 7's), so a request in that range belongs in 1280 — getting
    // this wrong made the run report a size Windows does not keep.
    assert_eq!(normalize_sizes(&[1024]), vec![1280], "1024 is not a bucket");
    assert_eq!(normalize_sizes(&[1281]), vec![1920]);
    assert_eq!(
        normalize_sizes(&[2000]),
        vec![2560],
        "the raised thumbnail ceiling must have a bucket to land in"
    );
    assert_eq!(normalize_sizes(&[40]), vec![48], "small buckets exist too");
    assert_eq!(
        normalize_sizes(&[0]),
        vec![256],
        "a zero is not a size; fall back to the default rather than asking for nothing"
    );
    assert_eq!(normalize_sizes(&[]), vec![256], "empty falls back too");
    assert_eq!(
        normalize_sizes(&DEFAULT_SIZES),
        DEFAULT_SIZES.to_vec(),
        "the shipped default must already be canonical, or every run pays to normalise it"
    );
}

/// THE REGRESSION THAT SHIPPED FOR THE WHOLE LIFE OF THIS FEATURE. One extraction fills
/// every smaller bucket, so whichever size is attempted FIRST is the only one that gets a
/// real render. Ascending order therefore built 96 and derived the rest, and Explorer threw
/// the derived entries away and re-extracted on first browse — after a run that reported
/// complete success. Largest-first is the fix; see `build_order` for the measurements.
#[test]
fn the_largest_bucket_is_always_extracted_first() {
    assert_eq!(build_order(&[96, 256, 768]), vec![768, 256, 96]);
    // The shipped default is the case that was broken, so pin it specifically rather than
    // trusting the general property above.
    let shipped = build_order(&normalize_sizes(&DEFAULT_SIZES));
    assert_eq!(
        shipped.first().copied(),
        Some(768),
        "the default run must extract at its LARGEST bucket first, or every bigger view \
         re-extracts on first browse; got {shipped:?}"
    );
    // `normalize_sizes` sorts ascending, so a caller that forgets to reorder gets exactly
    // the old bug back. Prove the two disagree, or this test proves nothing.
    assert_ne!(
        normalize_sizes(&DEFAULT_SIZES),
        shipped,
        "build_order must actually reorder; if these ever match, the guard is vacuous"
    );
}

/// The 2026-09-05 audit's F12, at the one place both front ends now go through. Each row
/// is (what the caller supplied, what the policy must answer): `Ok` for a list that is
/// entirely understood, `Err(fragment)` for one that is not, where the fragment is what
/// the message MUST name so the caller can find the element that did it.
#[test]
fn the_size_list_policy_parses_every_element_or_names_the_one_that_failed() {
    let cases: &[(&str, Result<Vec<u32>, &str>)] = &[
        // Accepted, and unchanged from what the old filter_map did with a clean list.
        ("96,256,768", Ok(vec![96, 256, 768])),
        ("256", Ok(vec![256])),
        // Whitespace around an element is a shell quoting a list, not a mistake.
        (" 96 , 256 ", Ok(vec![96, 256])),
        ("\t96\t", Ok(vec![96])),
        // Bigger than any bucket is still a request that can be honoured; normalize_sizes
        // clamps it, which is deliberately NOT this function's job.
        ("99999", Ok(vec![99999])),
        // The headline case: one bad element used to be dropped and the run went ahead.
        ("96,typo,768", Err("element 2")),
        ("typo", Err("element 1")),
        ("96.5", Err("element 1")),
        // An empty element, from a stray or trailing comma.
        ("96,,768", Err("element 2")),
        ("96,", Err("element 2")),
        ("", Err("element 1")),
        // Zero and negatives are not thumbnail edges.
        ("0", Err("element 1")),
        ("96,0", Err("element 2")),
        ("-96", Err("element 1")),
        // Overflow: one past u32::MAX, told apart from junk text.
        ("4294967296", Err("too large")),
        ("99999999999999999999999999", Err("too large")),
    ];
    for (spec, want) in cases {
        let got = parse_size_list_str("--size", spec);
        match want {
            Ok(sizes) => assert_eq!(got.as_ref(), Ok(sizes), "--size {spec:?}"),
            Err(fragment) => {
                let err = got.expect_err(&format!("--size {spec:?} must be refused"));
                assert!(
                    err.contains(*fragment),
                    "--size {spec:?}: message must name {fragment:?}, got {err:?}"
                );
            }
        }
    }
    // An empty SUPPLIED list is refused rather than silently becoming the default: the
    // caller asked for something specific, so answering with something else is the bug.
    let err = parse_size_list("'sizes'", std::iter::empty::<&str>()).expect_err("empty list");
    assert!(err.contains("no sizes given"), "got {err}");
    assert!(
        err.contains("96,256,768"),
        "the message should name the default it is NOT silently using, got {err}"
    );
}

/// The MCP front end hands each JSON array element over as its compact JSON text, so the
/// same parser sees a wrong-typed element as the wrong type it is. Proven here rather
/// than only in `mcp.rs`, since it is the policy that has to hold, not one caller.
#[test]
fn a_json_element_of_the_wrong_type_is_refused_by_the_same_policy() {
    for (element, fragment) in [
        ("\"96\"", "not a whole number"),
        ("true", "not a whole number"),
        ("null", "not a whole number"),
        ("[96]", "not a whole number"),
    ] {
        let err = parse_size_list("'sizes'", ["96", element])
            .expect_err("a non-number element must be refused");
        assert!(
            err.contains("element 2") && err.contains(fragment),
            "{element}: got {err}"
        );
    }
}

/// Reordering must not lose, duplicate or invent a bucket — the run would then report
/// sizes it never attempted.
#[test]
fn build_order_is_a_permutation_of_its_input() {
    for req in [
        vec![96u32, 256, 768],
        vec![256],
        vec![16, 32, 48, 96, 256, 768, 1280, 1920, 2560],
        vec![],
    ] {
        let mut got = build_order(&req);
        let mut want = req.clone();
        got.sort_unstable();
        want.sort_unstable();
        assert_eq!(got, want, "build_order changed the SET for {req:?}");
        // And every adjacent pair really is descending.
        let ordered = build_order(&req);
        assert!(
            ordered.windows(2).all(|w| w[0] > w[1]),
            "not descending: {ordered:?}"
        );
    }
}

/// A path that does not exist must come back unchanged rather than panicking — the walk
/// races with a user deleting files underneath it.
#[test]
fn parsing_path_passes_through_a_missing_file() {
    assert_eq!(
        parsing_path("Z:\\nope\\missing.png"),
        "Z:\\nope\\missing.png"
    );
}
