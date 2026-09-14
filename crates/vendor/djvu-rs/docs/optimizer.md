# Document optimizer

Issue: [#686](https://github.com/matyushkin/djvu-rs/issues/686)

The optimizer is intentionally conservative. The first vertical slice exposes
a typed request, a dry-run plan, an audit report, and an atomic CLI output path.
It removes only `FREE` IFF padding chunks. `FREE` has no decoded image or
document semantics, so the following content is retained byte-for-byte:

- image chunks and page order;
- text, annotations, metadata, bookmarks, and links;
- shared dictionaries, thumbnails, and unknown chunk IDs.

`OptimizationPreset::Archival` is accepted as a typed policy, but it currently
selects the same lossless cleanup. It does not silently invoke a lossy codec or
claim that a target size was reached. If `--target-size` cannot be met by
removing padding, the JSON plan/report sets `target_met` to `false` and names
the reason.

`--max-ssim-loss` is accepted for forward compatibility with archival
re-encode, but the current FREE-cleanup path is pixel-exact by construction and
does **not** measure SSIM. When the flag is set, the plan/report keeps
`quality_floor_met: true` and emits an explicit warning so callers cannot
mistake the threshold for an active gate.

The CLI always requires a separate `--output` path, rejects input/output path
aliasing, stages the result beside the destination, syncs it, and renames it
into place only after the optimizer succeeds. This keeps the input untouched
on a failed or interrupted write. The remaining roadmap is quality-aware
archival re-encoding, target-size search, progress callbacks, and cancellation
throughout a long-running codec pass.
