#![cfg(test)]

use super::*;

fn args(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn as_strs<'a>(pos: &'a [&'a String]) -> Vec<&'a str> {
    pos.iter().map(|s| s.as_str()).collect()
}

/// A008's headline case: `st2k thumbnail --size 128 in.png out.png` used to read
/// `pos[0]` as `"128"` because `pos` only excluded tokens starting with `--`, not a
/// known flag's VALUE.
#[test]
fn a_known_value_flags_value_never_lands_in_positionals() {
    let rest = args(&["--size", "128", "in.png", "out.png"]);
    assert_eq!(as_strs(&positionals(&rest)), vec!["in.png", "out.png"]);
}

#[test]
fn a_known_value_flag_is_skipped_regardless_of_its_position_among_positionals() {
    // The flag arriving BEFORE the positional it belongs after used to shift every
    // positional over by one (`rotate --by right in.jpg` read pos[0] as "right").
    let rest = args(&["--by", "right", "in.jpg"]);
    assert_eq!(as_strs(&positionals(&rest)), vec!["in.jpg"]);
}

#[test]
fn an_unrecognized_flags_own_token_is_excluded_but_its_value_is_not_swallowed() {
    let rest = args(&["--weird", "9", "in.png"]);
    // We have no table entry saying "--weird" takes a value, so we don't assume one:
    // only the flag's own spelling is excluded, matching pre-fix behaviour for a flag
    // no verb here defines.
    assert_eq!(as_strs(&positionals(&rest)), vec!["9", "in.png"]);
}

#[test]
fn short_recurse_flag_does_not_pollute_positionals() {
    // Before this fix, `-r` (prebuild's short --recurse alias) didn't start with
    // "--", so it slipped straight into `pos` as a bogus input path.
    let rest = args(&["-r", "C:\\folder"]);
    assert_eq!(as_strs(&positionals(&rest)), vec!["C:\\folder"]);
    assert!(has_flag(&rest, "-r"));
}

#[test]
fn every_value_flag_consumes_its_value_rather_than_leaking_it_as_an_input() {
    // A value-taking flag missing from VALUE_FLAGS does not error — its VALUE quietly
    // becomes a positional, i.e. a bogus input path. `bench-decode --runs 3` reported a
    // file literally named "3" as a decode FAILURE before `--runs` was registered. This
    // walks the whole list so the next flag added cannot repeat it.
    for f in VALUE_FLAGS {
        let rest = args(&[f, "7", "in.png"]);
        assert_eq!(
            as_strs(&positionals(&rest)),
            vec!["in.png"],
            "{f} did not consume its value; it leaked into the positionals"
        );
    }
}

#[test]
fn flag_does_not_treat_the_next_flag_as_its_own_value() {
    let rest = args(&["--size", "--recurse", "in.png"]);
    assert_eq!(flag(&rest, "--size"), None);
}

#[test]
fn flag_num_defaults_when_absent_but_errors_on_garbage() {
    let absent = args(&["thumbnail"]);
    assert_eq!(flag_num(&absent, "--size", 256u32), Ok(256));

    let bad = args(&["--size", "12x"]);
    assert!(
        flag_num(&bad, "--size", 256u32).is_err(),
        "an unparseable --size must error, not silently fall back to the default"
    );
}

#[test]
fn flag_num_opt_distinguishes_absent_from_unparseable() {
    let absent = args(&["convert"]);
    assert_eq!(flag_num_opt::<u8>(&absent, "--webp-quality"), Ok(None));

    let bad = args(&["--webp-quality", "abc"]);
    assert!(
        flag_num_opt::<u8>(&bad, "--webp-quality").is_err(),
        "an unparseable --webp-quality must error, not silently behave as \"not requested\""
    );
}

/// 2026-09-05 audit, F12: `prebuild --size 96,typo,768` used to DROP the unparseable
/// element and fill the cache for 96 and 768, reporting success for a run nobody asked
/// for. Every element is parsed now, and the first bad one names itself. The rows below
/// are the CLI half of the shared policy; `prebuild::parse_size_list_str` states it and
/// `mcp.rs` runs the same rows through the JSON front end, so the two cannot drift.
#[test]
fn prebuild_size_list_parses_every_element_or_refuses_the_whole_flag() {
    let default = sagethumbs2k_core::prebuild::DEFAULT_SIZES.to_vec();
    assert_eq!(
        prebuild_sizes(&args(&["--recurse"])),
        Ok(default),
        "an ABSENT --size is the only route to the defaults"
    );

    for (spec, want) in [
        ("96,256,768", Ok(vec![96u32, 256, 768])),
        ("512", Ok(vec![512])),
        (" 96 , 256 ", Ok(vec![96, 256])),
        ("99999", Ok(vec![99999])),
        ("96,typo,768", Err("element 2")),
        ("96,,768", Err("element 2")),
        ("96,", Err("element 2")),
        ("0", Err("element 1")),
        ("96,0", Err("element 2")),
        ("-96", Err("element 1")),
        ("4294967296", Err("too large")),
        ("", Err("element 1")),
    ] {
        let got = prebuild_sizes(&args(&["--size", spec]));
        match want {
            Ok(sizes) => assert_eq!(got, Ok(sizes), "--size {spec:?}"),
            Err(fragment) => {
                let err = got.expect_err("must be refused");
                assert!(
                    err.starts_with("--size:") && err.contains(fragment),
                    "--size {spec:?}: got {err:?}"
                );
            }
        }
    }
}

/// The refusal has to happen BEFORE any cache work: the run is rejected at argument
/// parsing, so `cli::prebuild` (and its elevation guard, and the folder walk) is never
/// reached. Checked through `run` rather than the helper, since that is the path a user
/// actually takes.
#[test]
fn a_bad_prebuild_size_fails_before_the_run_starts() {
    let dir = scratch("prebuild_size");
    let err = run(&args(&[
        "prebuild",
        dir.to_str().unwrap(),
        "--size",
        "96,typo",
    ]))
    .expect_err("a partly invalid --size must not start a run");
    assert!(
        err.starts_with("--size:") && err.contains("typo"),
        "got {err}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn scratch(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "st2k_bin_{tag}_{}_{}",
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

fn save_png(path: &std::path::Path) -> String {
    image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        40,
        30,
        image::Rgb([20, 120, 200]),
    ))
    .save(path)
    .unwrap();
    path.to_str().unwrap().to_string()
}

/// 2026-09-05 audit, F33: `thumbnail in.png out.png extra.png` used to render out.png and
/// say nothing about the third name. The arity check runs before the verb, so nothing is
/// written and the error names the stray argument. Against the pre-fix code `out.png`
/// exists after the call.
#[test]
fn a_single_input_verb_rejects_an_extra_file_before_any_write() {
    let dir = scratch("arity");
    let src = save_png(&dir.join("in.png"));
    let extra = save_png(&dir.join("extra.png"));
    let extra_before = std::fs::read(&extra).unwrap();
    let out = dir.join("out.png");

    let err = run(&args(&["thumbnail", &src, out.to_str().unwrap(), &extra])).unwrap_err();
    assert!(err.contains("takes at most 2"), "{err}");
    assert!(
        err.contains("extra.png"),
        "must name the stray argument: {err}"
    );
    assert!(
        !out.exists(),
        "nothing may be written when the arguments are wrong"
    );
    assert_eq!(std::fs::read(&extra).unwrap(), extra_before);

    // Same shape without the flag noise: `strip a b` and `rotate a b`.
    let err = run(&args(&["strip", &src, &extra])).unwrap_err();
    assert!(err.contains("takes at most 1"), "{err}");
    let err = run(&args(&["rotate", &src, &extra, "--by", "right"])).unwrap_err();
    assert!(err.contains("takes at most 1"), "{err}");
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        2,
        "no sibling may have been written"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Every single-input verb in the table refuses a surplus argument with the SAME message,
/// so a script sees one shape; `formats` takes none at all. Against the pre-fix code
/// `formats foo` prints the list and the rest fail for unrelated reasons.
#[test]
fn every_fixed_arity_verb_reports_a_surplus_argument_the_same_way() {
    for verb in [
        "thumbnail",
        "thumb",
        "convert",
        "rotate",
        "compress",
        "strip",
        "ocr",
        "info",
        "doctor",
        "diag",
        "register",
        "unregister",
        "upload",
        "upload-hosts",
        "upload-host",
        "devmode",
        "formats",
        "wallpaper-prepare",
        "folder-icon",
    ] {
        let err = run(&args(&[verb, "a", "b", "c"])).unwrap_err();
        assert!(
            err.contains("takes at most") && err.contains("unexpected"),
            "{verb}: {err}"
        );
    }
}

/// 2026-09-05 audit, E01: a `--json` batch report is something a script can ACT on.
/// `--retry-from` runs exactly the files the saved report lists as failed, with the
/// options given on this command line, and reports the retry the same way so a second
/// retry chains off the first. The refusals are the ways a script gets it wrong: a file
/// that is not a batch report, a report with nothing to retry, inputs given as well, or
/// the flag with no path. Against the pre-fix code the flag is unknown and its value is
/// read as an input path.
#[test]
fn retry_from_runs_only_the_failed_inputs_and_refuses_anything_else() {
    let dir = scratch("retry");
    let good = save_png(&dir.join("good.png"));
    let broken = dir.join("broken.png");
    std::fs::write(&broken, b"not a png").unwrap();
    let broken = broken.to_str().unwrap().to_string();
    let out = dir.join("out");
    let out = out.to_str().unwrap().to_string();

    let first = run(&args(&[
        "batch", "convert", &good, &broken, "--to", "webp", "--out", &out, "--json",
    ]))
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&first).unwrap();
    assert_eq!(v["status"], "partial");
    let report = dir.join("report.json");
    std::fs::write(&report, &first).unwrap();
    let report = report.to_str().unwrap().to_string();

    // The broken file is fixed between the runs, so the retry has something to convert.
    save_png(std::path::Path::new(&broken));
    let retry = run(&args(&[
        "batch",
        "convert",
        "--retry-from",
        &report,
        "--to",
        "webp",
        "--out",
        &out,
        "--json",
    ]))
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&retry).unwrap();
    assert_eq!(v["status"], "ok", "{retry}");
    assert_eq!(v["requested"], 1, "only the failed file is run: {retry}");
    assert_eq!(v["results"][0]["input"], broken.as_str());
    assert!(
        v["results"][0]["output"]
            .as_str()
            .unwrap()
            .starts_with(&out),
        "the retry honours this command line's --out: {retry}"
    );

    // Chaining: the retry's own report is a valid report, with nothing left to retry.
    let clean = dir.join("clean.json");
    std::fs::write(&clean, &retry).unwrap();
    let err = run(&args(&[
        "batch",
        "convert",
        "--retry-from",
        clean.to_str().unwrap(),
        "--to",
        "webp",
    ]))
    .unwrap_err();
    assert!(err.contains("nothing to retry"), "{err}");

    // Not a batch report: a pdf report, a bare array, and an image.
    for (what, bytes) in [
        (
            "pdf",
            br#"{"output":"x.pdf","status":"ok","requested":1,"combined":1,"omitted":[]}"#.to_vec(),
        ),
        ("array", br#"[{"input":"a.png"}]"#.to_vec()),
        ("png", std::fs::read(&good).unwrap()),
    ] {
        let bad = dir.join(format!("{what}.json"));
        std::fs::write(&bad, bytes).unwrap();
        let err = run(&args(&[
            "batch",
            "convert",
            "--retry-from",
            bad.to_str().unwrap(),
            "--to",
            "webp",
        ]))
        .unwrap_err();
        assert!(
            err.starts_with("--retry-from:") && err.contains("not "),
            "{what}: the refusal must say the file is not a batch report: {err}"
        );
    }

    // Inputs alongside the flag, and the flag with no path.
    let err = run(&args(&[
        "batch",
        "convert",
        &good,
        "--retry-from",
        &report,
        "--to",
        "webp",
    ]))
    .unwrap_err();
    assert!(err.contains("do not also give them"), "{err}");
    let err = run(&args(&["batch", "convert", "--retry-from", "--to", "webp"])).unwrap_err();
    assert!(err.contains("needs the report's path"), "{err}");
    assert!(
        VALUE_FLAGS.contains(&"--retry-from"),
        "the report path must never be read as an input"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// The list verbs keep taking a list: `pdf`/`cbz` with two inputs still combine, and the
/// new `--strict`/`--json` flags reach the verb rather than being read as file names.
#[test]
fn multi_input_verbs_still_take_a_list_and_their_flags_are_not_inputs() {
    let dir = scratch("multi");
    let a = save_png(&dir.join("a.png"));
    let b = save_png(&dir.join("b.png"));
    let out = dir.join("out.pdf");
    let text = run(&args(&[
        "pdf",
        out.to_str().unwrap(),
        &a,
        &b,
        "--json",
        "--strict",
    ]))
    .unwrap();
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["combined"], 2);
    assert!(out.exists());

    let comic = dir.join("out.cbz");
    let text = run(&args(&["cbz", comic.to_str().unwrap(), &a, &b])).unwrap();
    assert_eq!(text, comic.to_str().unwrap());
    assert!(has_flag(&args(&["--strict"]), "--strict"));
    assert!(
        BOOL_FLAGS.contains(&"--strict"),
        "--strict must never be read as an input"
    );

    let _ = std::fs::remove_dir_all(&dir);
}
