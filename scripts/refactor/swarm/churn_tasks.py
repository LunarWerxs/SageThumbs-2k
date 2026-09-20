"""Build the zswarm task file for the churn-hotspots family: one task per untested hot file.

Usage: python churn_tasks.py [--exclude a.rs,b.rs] [--only a.rs,b.rs] [--out PATH]
"""
import argparse
import json
import os

ROOT = os.path.normpath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "..", ".."))

RUST_FILES = [
    "src/previewhandler.rs",
    "src/bin/app/preview/content.rs",
    "src/bin/app/preview/shot.rs",
    "src/thumbprovider.rs",
    "src/bin/app/preview/mod.rs",
    "src/bin/app/preview/webview.rs",
    "src/bin/app/screenshot/spacehook.rs",
    "src/bin/app/preview/window/clipboard.rs",
    "src/bin/app/settings_dlg/nudge.rs",
    "src/bin/app/tags_to_folders.rs",
    "src/bin/app/files_to_folder.rs",
    "src/bin/app/settings_dlg/menuitems.rs",
    "src/bin/app/win/pickers.rs",
]

SCHEMA = {
    "type": "object",
    "properties": {
        "status": {"type": "string", "enum": ["done", "skipped"]},
        "tests_added": {"type": "array", "items": {"type": "string"}},
        "extracted": {"type": "array", "items": {"type": "string"}},
        "reason": {"type": "string"},
    },
    "required": ["status", "tests_added", "extracted", "reason"],
}

RUST_PROMPT = """You are adding REAL unit tests to ONE Rust file in SageThumbs 2K, a Windows shell extension (the cwd is the repo root; files are UTF-8 with LF line endings). The Architect flags `{file}` as a churn hotspot with no co-located tests: hot code nobody tests. Add a `#[cfg(test)] mod tests {{ ... }}` at the END of this file with tests that exercise THIS file's own logic: the pure parts (parsing, formatting, path / size / geometry computations, state transitions, classification, table lookups, the string a menu or a command line builds).

Read the file first. Then read two neighbouring files that already carry `#[cfg(test)] mod tests` (98 files under src/bin/app do; `grep -l "cfg(test)" src/bin/app/*.rs` finds them) and mirror the house style: test names are sentences in snake_case, one behaviour per test, `use super::*;` at the top, `assert_eq!` with a message only when the value is not self-explaining.

RULES:
- Tests must be meaningful: each pins a behaviour a reader would want protected (an edge case, a boundary, an error path, an invariant). No tests that merely call a function and assert it did not panic; no tests of Windows API calls, windows, COM objects, the registry, the clipboard, files outside a temp dir, or timing. Aim for 3 to 8 tests.
- If the file has no pure logic reachable from a test, extract the smallest pure part (the decision inside a message handler, the string a builder produces, the rectangle a layout computes) into a private `fn` at nesting zero WITHOUT changing behaviour (copy the code verbatim, pass what it needs by reference with explicit types, keep the call site identical in effect), and test that. Those extractions are the ONLY edits allowed outside the new test module. Do not reformat, reorder or rename anything else.
- This crate forbids unwrap/expect in shipped code; tests are exempt (clippy.toml allow-unwrap-in-tests), so unwrap in tests is fine.
- Anything the tests need comes from std or from this crate's EXISTING dependencies (check Cargo.toml before naming a crate; no new dependencies). Temp files go under `std::env::temp_dir()` with a unique name and are removed at the end of the test.
- Never run cargo, rustc or rustfmt: the author runs the suite over the whole batch, and concurrent cargo runs collide on the target dir. Write code that compiles: read the signatures of everything you call, match types exactly, no references to temporaries, no `?` inside a test that returns `()`.
- Do not touch any other file.

Answer through submit_result with: status (done | skipped), tests_added (the test function names), extracted (the private fns you extracted, or []), reason ("" or one line saying why nothing could be tested)."""

MJS_PROMPT = """You are adding REAL tests to `scripts/gen-site.mjs` in SageThumbs 2K (the cwd is the repo root; files are UTF-8 with LF line endings). The Architect flags it as a churn hotspot with no co-located test: hot code nobody tests. It generates the project's GitHub Pages site and runs its work at module top level from `process.argv` (line 47), so it cannot be imported by a test today.

The script ALREADY carries a self-test (`runSelfTest()` near line 416, with `fixtureHtml()` building its fixtures, run by a `--self-test` flag): the check cannot see it because it is not a sibling `*.test.mjs`. Do this, in order:
1. Read the whole script. Make it importable WITHOUT changing what it does when run: move the top-level work into `function main(argv)` and call it only when the file is the entry point (`if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) main(process.argv.slice(2));`). `export` the pure helpers a test can drive (parseArgs, capabilityParts, sourceSentence, codecSentences, codecSentence, capabilitySentence, capabilityParagraphs, buildFormatWall, foreignFormatGroups, applyAll, fixtureHtml, and any other function that takes values and returns values). Do not change any output, any file the script writes, or the order of its work.
2. Write `scripts/gen-site.test.mjs` using Node's built-in runner (`import test from "node:test"; import assert from "node:assert/strict";`, no dependencies), importing from `./gen-site.mjs`. MOVE every assertion of `runSelfTest()` into it as named tests (one behaviour per test), then add tests for the pure helpers that the self-test does not cover (an empty list, a boundary, a path with a space, escaping). Aim for 8 to 14 tests. Never run the site generator itself from a test, and never write outside `os.tmpdir()`.
3. Make `--self-test` keep working by having it run `node --test` on the sibling test file (spawnSync of process.execPath with ["--test", thisTestFile]) and exit with its status, so nothing that calls the flag today changes behaviour; delete `runSelfTest()` once its assertions live in the test file.
4. Run `node --test scripts/gen-site.test.mjs` and `node scripts/gen-site.mjs --self-test`; both must pass. Then run the script the way its usage text says a dry run works (read the header; if there is no dry-run mode, do not run it) to confirm the entry-point guard still works.
Do not touch any other file. Answer through submit_result with: status (done | skipped), tests_added (test names), extracted (the functions you exported), reason ("" or why skipped)."""

BUILD_PROMPT = """You are moving the locale-table generation out of `src/build.rs` (the sagethumbs2k crate's build script) into the workspace's `crates/build-support` crate, where it can be unit-tested: cargo never compiles a build script with cfg(test), so a build script cannot carry tests. The Architect flags `src/build.rs` as a churn hotspot with no tests; the honest fix is a thin build.rs calling tested code. The cwd is the repo root; files are UTF-8 with LF line endings.

Read `src/build.rs` in full, `crates/build-support/src/lib.rs` (its existing API and its `mod tests`, for the house style), `crates/build-support/Cargo.toml`, and the root `Cargo.toml` `[build-dependencies]`.

Do this, in order:
1. Create `crates/build-support/src/locales.rs` and declare it in lib.rs as `pub mod locales;`. Move these functions from build.rs into it VERBATIM (bodies unchanged; `pub` where build.rs calls them, private otherwise): is_dll_key, read_locales, write_locales_table, append_locale_gap_report, build_coverage_report, placeholders, enforce_locale_parity, write_coverage_file, write_keys_module, write_dll_keys, to_upper_snake, plus any private helper only they use. Keep `generate_locales` (the orchestration, with every `println!("cargo:...")` directive) in build.rs, calling `build_support::locales::...`. If a moved function prints a `cargo:` directive itself, leave that println in place (build-support runs inside the build-script process, so the directive still reaches cargo) and say so in your answer. Keep every doc comment with the function it documents.
2. If a moved function needs a crate that build-support lacks (`toml`, for instance), add it to `crates/build-support/Cargo.toml` `[dependencies]` at the SAME version the root `[build-dependencies]` pins. No other new dependency.
3. Add `#[cfg(test)] mod tests` at the end of locales.rs with 5 to 8 tests that pin real behaviour: to_upper_snake on a key with digits and underscores; placeholders on a value with no brace, one placeholder, a repeated one and a malformed one; is_dll_key on a matching and a non-matching key; enforce_locale_parity on a matching set and on a set with a missing key (`#[should_panic]` or a matched Result, whichever shape the function has); build_coverage_report on a two-language map with a gap; read_locales on a temp dir under std::env::temp_dir() holding two small toml files, removed at the end. unwrap in tests is fine.
4. Do not change any generated byte (the locales table, the keys module, the coverage file): this is a move, not a rewrite.
Never run cargo, rustc or rustfmt (the author builds and runs `cargo test -p build-support` afterwards; concurrent cargo runs collide). Write code that compiles: imports at the top of locales.rs for everything the moved bodies use (std::collections::{BTreeMap, BTreeSet}, std::fs, std::path::Path, toml), matching signatures, explicit paths.
Do not touch any other file. Answer through submit_result with: status (done | skipped), tests_added (test names), extracted (the functions moved), reason ("" or why skipped)."""

PY_PROMPT = """You are adding REAL tests to `scripts/compare-renders.py` in SageThumbs 2K (the cwd is the repo root; files are UTF-8 with LF line endings). The Architect flags it as a churn hotspot with no co-located test: hot code nobody tests. The pytest convention it checks for is a sibling named `test_<name>.py`, so the file to create is `scripts/test_compare-renders.py`.

Do this, in order:
1. Read the script. Its pure parts (`as_8bit`, `normalized`, `mean_delta`, `centre`, `classify_pair`, `load_expected`, `validate_args`, `print_*` report builders, whatever else takes values and returns values) are the targets; `render`, the cargo lookup and the process pools are not.
2. Write `scripts/test_compare-renders.py` with the standard library's `unittest`, loading the target by path (the hyphen in the name rules out `import`): `importlib.util.spec_from_file_location("compare_renders", Path(__file__).with_name("compare-renders.py"))`. If the module imports a third-party package at top level (PIL, numpy), import the module inside a `setUpClass` guarded by `unittest.SkipTest` when that package is missing, so a machine without it skips rather than errors. Each test pins a behaviour a reader would want protected (a boundary, an empty input, a classification edge, an error path); aim for 4 to 8 tests. Build any image the tests need in memory or under `tempfile`; never read the corpus.
3. Add `if __name__ == "__main__": unittest.main()` and run `python scripts/test_compare-renders.py` to make it pass.
Do not modify `compare-renders.py` unless a one-line change is needed to make a function reachable from a test (say so). Do not touch any other file. Answer through submit_result with: status (done | skipped), tests_added (test names), extracted (functions you had to change, or []), reason ("" or why skipped)."""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--exclude", default="")
    ap.add_argument("--only", default="")
    ap.add_argument("--out", default=os.path.join(ROOT, "tmp/churn-tasks.json"))
    a = ap.parse_args()
    excluded = set(x for x in a.exclude.split(",") if x)
    only = set(x for x in a.only.split(",") if x)

    tasks = []
    for f in RUST_FILES:
        if f in excluded or (only and f not in only):
            continue
        tasks.append({"id": f.replace("/", "_").replace(".rs", ""), "prompt": RUST_PROMPT.format(file=f)})
    if "scripts/gen-site.mjs" not in excluded and (not only or "scripts/gen-site.mjs" in only):
        tasks.append({"id": "scripts_gen-site", "prompt": MJS_PROMPT})
    if "scripts/compare-renders.py" not in excluded and (not only or "scripts/compare-renders.py" in only):
        tasks.append({"id": "scripts_compare-renders", "prompt": PY_PROMPT})
    if "src/build.rs" not in excluded and (not only or "src/build.rs" in only):
        tasks.append({"id": "src_build", "prompt": BUILD_PROMPT})

    job = {"defaults": {"cwd": ROOT, "tools": "all", "schema": SCHEMA, "max_turns": 40, "timeout_s": 1500, "model": "deepseek-flash-or"}, "tasks": tasks}
    with open(a.out, "w", encoding="utf-8") as fh:
        json.dump(job, fh, indent=1)
    print("tasks", len(tasks), "->", a.out)
    for t in tasks:
        print(" ", t["id"])


if __name__ == "__main__":
    main()
