# SageThumbs 2K patch to `djvu-rs`: crate-type trim only

**The bilevel-mask fallback bounds fix (originally <https://github.com/matyushkin/djvu-rs/pull/801>,
opened 2026-09-08) landed upstream in djvu-rs 0.32.1 (2026-09-10).** We vendor 0.35.0 unmodified
on that front - the render fix in `src/djvu_render.rs` is exactly upstream's, no local patch
rides on it anymore. The only reason this vendored copy still exists is the `crate-type` trim
below.

**Delete all of this the moment upstream drops the `cdylib` crate-type declaration** (or cargo#6313
itself is fixed) - the `[patch.crates-io]` line in the workspace `Cargo.toml`, its
`[workspace] exclude` entry, `crates/vendor/djvu-rs`, this directory, and
`scripts/vendor-djvu.ps1` + `scripts/fetch-pristine-djvu.ps1`. The call site in
`container/djvu.rs` does not change when it goes.

## Why we still vendor (history)

The fuzz gate (`fuzz::parsers_survive_mutation_of_synthetic_seeds`, reached through
`container::fuzzseed::synthetic_djvu` -> `container::extract_cover` -> `djvu::extract`) found a
panic in djvu-rs 0.27.0's renderer: `index out of bounds: the len is N but the index is N` at
`src/djvu_render.rs`, inside `composite_rows_bilevel_one`'s per-pixel fallback loop.

A DjVu page's declared width lives in the `INFO` chunk; the bilevel mask's actual width comes
from decoding its own `Sjbz`/JB2 payload. On a file where those disagree - a mutated `INFO`
chunk claiming a page wider than the mask it ships - the fallback loop clamped the source
column (`px`) to `page_w - 1` only, then indexed `mask_row[px >> 3]`, where `mask_row` is sized
to the mask's own (narrower) width. The 1:1-scale fast path just above it already guards this
exact case (`ox0 + out_w <= mask.width`); the fallback below it did not.

This code runs inside Explorer's thumbnail host under `panic = "abort"` (see the crate's own
`safety.rs` boundary doctrine), so a crafted `.djvu` reaching it could abort the shell. We
carried a one-clamp patch for it (bounding `px` by `mask.width - 1` as well as `page_w - 1`)
until 0.32.1, where upstream's own fix (folded into the `#805` "keep the 1:1 bilevel fast path
inside the mask" fix in that release) does the same bounding - see
`crates/vendor/djvu-rs/src/djvu_render.rs`'s `last_col` clamp in `composite_rows_bilevel_one`'s
fallback loop. We verified this by diffing the fallback loop's logic directly, not just reading
the changelog: the clamp is semantically identical to ours.

## What the patch does now

One `Cargo.toml` hunk, vendoring mechanics rather than a fix:

- `crate-type = ["rlib"]` instead of upstream's `["cdylib", "rlib"]`. A path crate that is
  both is the cargo#6313 output-name collision this workspace already removed from its own
  crates: on Windows cargo drops the metadata hash from a local cdylib's outputs, and
  `cargo test --release --lib --tests` builds the crate twice (panic=abort for the bins,
  panic=unwind for the test harnesses), so the two units overwrote each other's
  `djvu_rs.dll` / `.pdb` / `libdjvu_rs.rlib` and the dependents failed with `E0463`. We only
  link the rlib. (DEVELOPMENT_GOTCHAS: "A vendored PATH crate that is cdylib+rlib".)
- `[package.metadata.cargo-machete] ignored = ["flate2", "wide"]`: upstream declares both and
  references neither, and our cargo-machete gate scans vendored trees.

## Maintaining it

**The patch file is the source of truth. The vendored copy is GENERATED.** Do not edit
`crates/vendor/djvu-rs` by hand: the next run of the script overwrites it and the change
disappears with nothing to say so.

```powershell
pwsh scripts\vendor-djvu.ps1            # regenerate at the pinned version
pwsh scripts\vendor-djvu.ps1 -Check     # verify the committed tree is exactly pristine + patch
pwsh scripts\vendor-djvu.ps1 -Version 0.33.0   # try a newer release
```

The vendored copy is still committed, deliberately, because cargo needs the path dependency
present at build time and CI checks out the repo without running the script. It is a build
input; the script is how it is produced.

**The pinned version must match what `Cargo.lock` would resolve unpatched** (djvu-rs 0.35.0;
0.35.0 still declares `crate-type = ["cdylib", "rlib"]`, checked 2026-09-22).
Patching a different codebase than the one that was tested is the failure this pin exists to
prevent.
