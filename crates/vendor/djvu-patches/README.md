# SageThumbs 2K patch to `djvu-rs`: bilevel-mask fallback bounds fix

**Delete all of this the moment a djvu-rs release carries the fix** (the `[patch.crates-io]`
line in the workspace `Cargo.toml`, its `[workspace] exclude` entry, `crates/vendor/djvu-rs`,
this directory, and `scripts/vendor-djvu.ps1` + `scripts/fetch-pristine-djvu.ps1`). The call
site in `container/djvu.rs` does not change when it goes.

## Why

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
`safety.rs` boundary doctrine), so a crafted `.djvu` reaching it could abort the shell.

## What the patch does

One clamp, added to the existing one: `px` is bounded by `mask.width - 1` as well as
`page_w - 1`, so `px >> 3` can never index past `mask_row`'s end. No behavior changes for any
well-formed file (where `mask.width == page_w` already holds, per the fast path's own
invariant) - only the out-of-bounds case is affected, and it now degrades to reading the
mask's last real column instead of panicking.

## Maintaining it

**The patch file is the source of truth. The vendored copy is GENERATED.** Do not edit
`crates/vendor/djvu-rs` by hand: the next run of the script overwrites it and the change
disappears with nothing to say so.

```powershell
pwsh scripts\vendor-djvu.ps1            # regenerate at the pinned version
pwsh scripts\vendor-djvu.ps1 -Check     # verify the committed tree is exactly pristine + patch
pwsh scripts\vendor-djvu.ps1 -Version 0.28.0   # try a newer release
```

The vendored copy is still committed, deliberately, because cargo needs the path dependency
present at build time and CI checks out the repo without running the script. It is a build
input; the script is how it is produced.

**The pinned version must match what `Cargo.lock` would resolve unpatched** (djvu-rs 0.27.0).
Patching a different codebase than the one that was tested is the failure this pin exists to
prevent.
