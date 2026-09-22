# Document optimizer

Issue: [#686](https://github.com/matyushkin/djvu-rs/issues/686)

The optimizer is intentionally conservative. The first vertical slice exposes
a typed request, a dry-run plan, an audit report, and an atomic CLI output path.
It removes only `FREE` IFF padding chunks. `FREE` has no decoded image or
document semantics, so the following content is retained byte-for-byte:

- image chunks and page order;
- text, annotations, metadata, bookmarks, and links;
- shared dictionaries, thumbnails, and unknown chunk IDs.

`OptimizationPreset::Archival` applies the same cleanup and then a measured
lossy step, described under [Archival re-encode](#archival-re-encode-814-slice-3).
It never invokes a lossy codec without a quality floor and never claims a
target size was reached. If `--target-size` cannot be met by the selected
rewrites, the JSON plan/report sets `target_met` to `false` and names the
reason.

`--max-ssim-loss` is the quality floor of the archival preset. Under the
lossless preset the FREE-cleanup path is pixel-exact by construction and does
**not** measure SSIM: the plan/report keeps `quality_floor_met: true` and emits
an explicit warning so callers cannot mistake the threshold for an active gate.

The CLI always requires a separate `--output` path, rejects input/output path
aliasing, stages the result beside the destination, syncs it, and renames it
into place only after the optimizer succeeds. This keeps the input untouched
on a failed or interrupted write.

## Progress ([#814](https://github.com/matyushkin/djvu-rs/issues/814), slice 1)

`Optimizer::with_progress(hook)` installs a `Fn(&ProgressEvent) + Send + Sync`
hook. The optimizer calls it on the calling thread, once per component per
phase, after that component is handled:

| Phase | Reported by | Components | `bytes_so_far` |
|-------|-------------|------------|----------------|
| `plan` | `plan` and `optimize` | the root's children of a `DJVM`, or the single root `FORM` | encoded size of the components walked, headers included |
| `rewrite` | `optimize` | the components the plan rewrites; none when the plan is a pass-through | input payload of the components rewritten |
| `verify` | `optimize` | the components of the re-parsed output | as for `plan`, over the output |

Each `ProgressEvent` carries the phase, the zero-based `component_index`, the
`component_count` of its phase, the four-byte `component_id` (`DJVU`, `DJVI`,
`THUM`, `DIRM`, `NAVM`, `FREE`, …) and `bytes_so_far`. Within one phase the
index increases by one per event and `bytes_so_far` never decreases. The hook
lives on the `Optimizer`, not on `OptimizationRequest`, so the request stays a
plain comparable value.

The `verify` phase is new with the hook: `optimize` re-parses its own output
and fails with `OptimizeError::Verification` instead of returning bytes whose
page count differs from the input's.

`djvu optimize` shows a one-line progress indicator on stderr when stderr is a
terminal, and clears it before printing the JSON. A pipe or a redirect sees
neither the line nor its escape codes. Nothing in the JSON plan or report
changes for this slice.

## Cancellation ([#814](https://github.com/matyushkin/djvu-rs/issues/814), slice 2)

`Optimizer::with_cancel(hook)` installs a `Fn() -> bool + Send + Sync` hook,
the same cooperative contract as `ExportObserver::cancelled` for the export
writers. The optimizer polls it on the calling thread before parsing the input
and before each component of each phase. Once it returns `true`, `plan` and
`optimize` return `OptimizeError::Cancelled`. Work already begun on a
component completes first; no partial output is ever returned, and the
progress events reported before the stop are exactly those of the components
that were handled.

Rewrites are applied one component at a time in plan order, so a stop in the
`rewrite` phase lands on a component boundary. The `djvu` binary stages its
output only after `optimize` succeeds, so a cancelled run leaves no file
behind. The binary installs no signal handler of its own: an interrupt ends the
process before anything is staged.

## Archival re-encode ([#814](https://github.com/matyushkin/djvu-rs/issues/814), slice 3)

With `--preset archival --max-ssim-loss L` the optimizer re-encodes each
page's IW44 background and keeps the result only when it is both smaller and
within the floor. The measure is SSIM over the luma channel between the layer
decoded from the input and the same layer decoded from its replacement; the
loss is `1 - ssim`, and the floor `L` bounds it. The number describes what
the re-encode cost, not how the page compares with the paper it was scanned
from: there is no reference better than the input.

The search is a bisection over the IW44 slice count, from one slice up to the
count the input carries: quality grows with slices, and encoding more of them
than the input has cannot recover detail the input already lost. Each probe
encodes, decodes and measures the page, so the `plan` phase is where an
archival run spends its time; `optimize` reuses what its own plan encoded and
never encodes a page twice. A page whose background fails to decode, or whose
best re-encode within the floor is not smaller, is left untouched and counted
in one warning per reason.

The JB2 text mask is re-encoded only with `--lossy-text`
(`OptimizationRequest::with_lossy_text`): lossy symbol matching
(`Jb2EncodeOptions::lossy_text`, threshold 0.02) under the same floor, measured
the same way on the mask rendered as ink on paper. Masks that use a shared
(`INCL`) or page (`Djbz`) dictionary are skipped. The text layer is what a
scan is kept for, so this stays opt-in.

Without `--max-ssim-loss` the archival preset re-encodes nothing and says so
in a warning that names the flag. `PM44`/`BM44` legacy photo files, `FG44`
foreground layers and thumbnails are not re-encoded in this slice.

Each re-encoded component reports its `quality` in the plan and report:

```json
{"path":[3],"chunk_id":"BG44","action":"reencode-background",
 "input_bytes":48213,"output_bytes":19870,
 "reason":"IW44 background re-encoded at 41 slices: ...",
 "quality":{"ssim":0.9873,"ssim_loss":0.0127,"slices":41}}
```

`quality` is `null` for a pixel-exact action (`remove-free-chunk`) and
`slices` is `null` for a mask (`reencode-mask`). The top-level `min_ssim` is
the lowest SSIM among the re-encoded components, `null` when nothing was
re-encoded. `quality_floor_met` is now measured: every selected re-encode was
held to the floor before it was selected.

This slice marks `OptimizationRequest`, `RewriteAction`, `RewrittenComponent`,
`OptimizationPlan` and `OptimizationReport` `#[non_exhaustive]`, an intended
break recorded in [`api-compatibility.md`](api-compatibility.md) §2, so the
next slice can add fields without another one.

The remaining roadmap is target-size search.
