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
target size was reached. With a floor, `--target-size` drives a search for the
least loss that meets the budget, described under
[Target-size search](#target-size-search-814-slice-4). If the target cannot be
met by the selected rewrites, the JSON plan/report sets `target_met` to
`false` and names the reason.

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

## Target-size search ([#814](https://github.com/matyushkin/djvu-rs/issues/814), slice 4)

With `--preset archival --max-ssim-loss L --target-size N` the optimizer
looks for the least loss whose output fits in `N` bytes. The floor `L` stays
the outer bound: no re-encode ever loses more than `L`, and the target only
tightens the ceiling below it. `--target-size` without a floor changes
nothing in the selection; the plan then merely reports whether the safe
rewrites happen to meet the target, as before.

The search is a bisection over one loss ceiling `C` in `(0, L]` shared by
every layer of the document, in twelve steps. At each ceiling every layer is
re-chosen as under slice 3, the smallest re-encode with `1 - ssim <= C` that
is also smaller than its input, and the output size is predicted. A ceiling
whose predicted output fits becomes the new upper bound, otherwise the new
lower bound; the selection returned is the one at the final upper bound, so
the output is always within the floor and, when `target_met` is `true`,
within the target. One ceiling for the whole document means no page pays for
another: pages lose at most what the ceiling allows, and the search lowers it
until the budget is met.

The prediction is exact. The emitted size depends only on the chunk lengths,
so each candidate is sized by a dry emission with placeholder payloads of the
chosen lengths, which costs no codec work. Every background probe (one slice
count of one page) is encoded, decoded and measured once and then memoised,
so the search reuses the probes the floor selection already made and adds
only the slice counts the tighter ceilings need. A page's mask is probed once,
as under slice 3. `optimize` reuses the chunks its plan encoded for the final
selection and re-encodes a page only when the chunks it held belong to a
different slice count.

Three outcomes are named in the plan and report:

- *Met by cleanup alone.* When `FREE` removal already meets the target, no
  layer is re-encoded and a warning says `target size N bytes is met by
  structural cleanup alone; no layer is re-encoded`.
- *Met.* `target_met` is `true`, `output_bytes <= N`, and each re-encoded
  component's `reason` names the ceiling the search settled on:
  `... within the target-size search ceiling 0.0062 (max_ssim_loss 0.02)`.
- *Unreachable.* When even the selection at the floor is larger than `N`, the
  floor selection is kept, `target_met` is `false`, and a warning says
  `target size N bytes is unreachable within max_ssim_loss L: the smallest
  output within the floor is M bytes`. `quality_floor_met` stays `true`: the
  optimizer never trades the floor for the target.

The search runs inside the `plan` phase after the per-component walk, so it
reports no progress events of its own; the walk, where the floor selection
(the expensive probes) is made, reports one `plan` event per component as
before. The cancel hook is polled before each layer at each step, so a stop
lands within one probe. No public field is added: `target_size`,
`target_met`, `min_ssim` and per-component `quality` carry the result.

A worked example on `tests/fixtures/boy.djvu` (4803 bytes, floor 0.02):
the floor alone gives 3362 bytes at 91 slices (SSIM 0.9805); a target of
4000 bytes gives 3872 bytes at 97 slices (SSIM 0.9938, ceiling 0.0062); a
target of 3000 bytes is reported unreachable with the 3362-byte floor
selection kept.
