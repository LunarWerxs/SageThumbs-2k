# Contributing to djvu-rs

Thank you for your interest in contributing! This document covers everything you need
to get started.

## Quick start

```sh
git clone https://github.com/matyushkin/djvu-rs
cd djvu-rs
git config core.hooksPath .githooks  # install pre-commit checks
cargo test          # run the full test suite
cargo clippy -- -D warnings
cargo fmt --check
```

Rust **1.88** or later is required (uses let-chains from edition 2024).

## Before you open a PR

All of the following must pass locally:

```sh
cargo fmt --check                  # formatting
cargo clippy -- -D warnings        # no lint warnings
cargo test                         # unit + integration + doctests
cargo build --no-default-features  # no_std check
```

## Hard rules

These match the crate's own invariants — violations will be requested to fix in review:

| Rule | Reason |
|------|--------|
| No `unwrap()` / `expect()` / `panic!` in library code | Caller decides how to handle errors |
| No `String` as error type — use a typed `thiserror` enum | Callers can match on variants |
| No slice `[i]` without a bounds check — use `.get()` | Prevents panics on malformed input |
| Every public item needs a `///` doc comment | docs.rs is the primary API surface |

## Adding a new feature

1. **Write a failing test first** (unit test or integration test under `tests/`).
2. Implement the minimum code to make it pass.
3. Refactor under green tests.

No test → no merge.

## Adding or updating corpus files

Test fixtures live in `tests/corpus/`. They must be:

- **Pre-1928 public domain** (US copyright law) *or* CC0 / public domain by explicit grant.
- Small — prefer files under 5 MB; multi-page files up to ~25 MB are acceptable for
  benchmarks only.
- Listed in `tests/corpus/README.md` with source URL and license confirmation.

Do not commit files whose copyright status is unclear.

## Benchmarks

Criterion benchmarks are in `benches/`. Run them with:

```sh
cargo bench
```

If your change affects codec performance, include before/after numbers in the PR
description (machine specs + `cargo bench` output).

## Spec compliance

This crate is written from the public DjVu v3 specification only (see `SOURCES.md`).
If you are implementing a new codec feature, cite the relevant section of the spec in
a comment. Do not copy code from djvulibre or any GPL-licensed source.

## Commit style

Commits must follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).
This is not just style — **release-please reads commit messages to determine the next
version number and generate `CHANGELOG.md` automatically.**

```
<type>[optional scope]: <short description>

[optional body]

[optional footer: BREAKING CHANGE: <description>]
```

Common types and their effect on versioning:

| Type | Changelog section | Version bump |
|------|------------------|-------------|
| `feat` | Added | minor |
| `fix` | Fixed | patch |
| `perf` | Performance | patch |
| `docs` | — (no entry) | patch |
| `refactor` | — (no entry) | patch |
| `chore` | — (no entry) | none |
| `feat!` or `BREAKING CHANGE:` footer | — | major (minor while `0.x`) |

Examples:

```
fix: clamp overflow in IW44 normalize for extreme coefficients
feat(render): add render_gray8 for single-channel grayscale output
feat!: remove deprecated DjVuPage::extract_mask — use raw_chunk instead
perf(iw44): SIMD YCbCr→RGB using wide::i32x8
docs: document Rotation enum variants
chore(ci): upgrade actions/checkout to v4
```

See `RELEASING.md` for how commits drive the automated release process.

## Opening a pull request

- Target the `main` branch.
- Keep PRs focused — one logical change per PR.
- Link to any relevant issue in the description.
- If the change touches the public API, update `README.md` examples if needed.

## Triaging an OSS-Fuzz finding

OSS-Fuzz reports arrive as private issues containing a testcase plus a
reproducer command. Workflow:

1. **Reproduce locally**. Drop the testcase into `fuzz/corpus/<target>/`
   and run:
   ```sh
   cargo +nightly fuzz run <target> fuzz/corpus/<target>/<testcase> -- -runs=1
   ```
   If it doesn't reproduce, check that you're on the same commit OSS-Fuzz
   was building (the report includes a SHA).
2. **Minimise** with `cargo fuzz tmin` if the input is large.
3. **Add a unit test** under `tests/regression_<codec>.rs` (or the
   appropriate file) that loads the minimised testcase and asserts the
   expected error variant. This locks the fix.
4. **Fix the underlying bug**. Most findings so far have been
   missing-bounds-check or unchecked-arithmetic in malformed-input paths.
   Stick to the "Hard rules" above — return a typed error, do not panic.
5. **Commit the minimised testcase** alongside the fix in
   `fuzz/corpus/<target>/`. It becomes part of the seed corpus on the
   next OSS-Fuzz build via `oss-fuzz/build.sh`.
6. **Mark the OSS-Fuzz issue as fixed** by including
   `Fixes oss-fuzz/<id>` in the commit body so the bot closes it after
   the next successful build.

## Reporting bugs

Open a GitHub issue. Include:

- Rust version (`rustc --version`)
- A minimal reproducer (ideally a `#[test]` that fails), or the DjVu file if it can
  be shared publicly.
- The panic message or error output.
