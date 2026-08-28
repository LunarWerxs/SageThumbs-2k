# Complexity burndown

What this is: a ranked queue of functions over the cognitive/cyclomatic complexity gates,
for gradual reduction via extract-helper refactors. No behavior change intended anywhere
in this list.

Scan source: Odin's portable arkitect roster, 2026-08-28. This repo's own `.arkitect`
config does not run cognitive/cyclomatic checks, so `bun run arkitect:counts` (or
equivalent) reports 0 for this category. That mismatch between the portable scan and the
repo's own gate is a standing risk flag: the local gate will not catch new complexity
regressions in this dimension until it grows a check of its own.

Method: extract coherent, well-named helper functions out of each oversized function. No
behavior change. Run `cargo test` (scoped to the touched module when the full suite is
slow) after each function, plus `cargo clippy` on the touched files, before moving to the
next item.

**Threshold discipline (learned in tranche 2): a land helper still has to clear the gate on
its own, and the gate is stricter than nesting alone.** The scan's error/gating threshold is
cognitive ~30 / cyclomatic ~20 (the "warning" tier sits lower, around cognitive 15, and is
not gating). Two things bit tranche 2 specifically:

1. **This scanner counts the `?` operator as a branch point**, unlike typical
   SonarSource-style cognitive-complexity conventions that treat early-return via `?` as
   free. A dispatcher with one `?`-propagating call per match arm (e.g. a 6-9-way marker
   switch, each arm calling a fallible `parse_*` helper) racks up complexity from the `?`s
   alone even after every arm's OWN nesting is flattened to one line. `parse_headers`
   (jpegtran.rs) and `parse` (jp2/codestream.rs) both needed a second pass for exactly this:
   flattening nesting got them from >100 down to ~40-70, but they still gated until each
   arm's fallible call was changed to return its `Result`/`Option` via `.map()` (no `?`
   inside the arm) and the whole match's error got propagated through a SINGLE `?` after the
   match, not one per arm. See those two functions for the pattern (`HeaderMarkerOutcome` /
   `MarkerOutcome`).
2. **A brand-new helper spawned mid-extraction is not automatically under the gate**: check
   it, don't assume it. Splitting a giant function frequently produces a "coordinator" helper
   (a marker-dispatch loop, a per-line state-machine `handle_line`) whose OWN complexity can
   still land in the 20s-40s if it has many branches, even with zero remaining nesting.

**When extracting, land every new helper comfortably under cognitive 30 / cyclomatic 20**,
not just "smaller than before", and for any function with more than ~5 fallible calls
dispatched from one match/if-chain, prefer the defer-the-`?` pattern over one `?` per arm.

Do not attempt casually: these giant Win32 wndproc dispatchers are large, stateful,
side-effect-heavy message loops. Splitting them safely needs a dedicated session, not a
casual mechanical extract-and-move pass. Treat each as its own project.

**Standing note on wndproc refactors:** a message-dispatcher split like this is mechanical
by construction (one match arm -> one named handler, same routing, same LRESULT returns,
same unsafe blocks) and passes build + `cargo test` + clippy cleanly because none of those
tools can drive real window messages through the OS message loop. That only proves the
*code* still means what it meant before, not that the *window* still behaves right under a
mouse and keyboard. **A human UI click-through (open a real preview window and exercise
drag/resize/keys/toolbar/PDF paging/video transport by hand) is still recommended before
the next release**, for this row and for any future wndproc split in this file.

- `shot_wndproc` src/bin/app/screenshot/overlay/input.rs:128 (cognitive 480, CC 124)

**Tranche 11 (2026-08-28): the next 9 worst rows cleared**, worst first: two test functions
had their nested logic hoisted out to named module-scope helpers rather than split by
phase, since a `#[test]` fn's job is the assertions, not a multi-step pipeline,
`filtr_1d_matches_the_original_three_buffer_algorithm` (decode/jp2/dwt.rs) had its nested
`reference` fn (itself over the gate from the nesting) split into `reference_reversible`/
`reference_irreversible`, sharing the `n <= 1` edge case in `reference`;
`lib_side_translation_keys_all_survive_the_dll_subset` (i18n.rs) had its directory-walk-and-
scan body split into `scan_lib_t_calls` (the walk) and `scan_file_t_calls` (one file's `t("…")`
matches). `parse_tag` (preview/mdhtml.rs) split into `parse_tag_attrs`/`parse_one_attr` for
the attribute-list loop; `generate_locales` (build.rs) split into `read_locales`/
`write_locales_table`/`build_coverage_report`/`append_locale_gap_report`/
`write_coverage_file`/`write_keys_module`/`write_dll_keys`, plus a hoisted `is_dll_key`
predicate (was a closure); `assemble` (ocr/table.rs) split into its four documented steps as
named helpers (`rebuild_visual_rows`, `typical_space_width`, `find_columns`,
`assemble_rows`), plus a hoisted `gap_of` (was a closure); `ttf_wndproc`
(tags_to_folders.rs) split into `on_create`/`on_command`/`on_command_ok`, same
wndproc-dispatcher method as prior tranches; `capture_monitor` (screenshot/hdr.rs) split
into `create_capture_device`/`start_capture_session`/`wait_for_frame`/
`frame_src_texture`/`staged_copy`, preserving the original's inconsistent early-return
cleanup exactly (only the HDR-format-mismatch branch explicitly closes the WGC
session/pool before returning `None`; every other failure path propagates via `?` with no
explicit close, same as before, not a bug we introduced, and not one we fixed here);
`nv12_frame_from_owned_bytes` (video.rs) split into `nv12_reader_for_bytes` (source-reader
setup + NV12 media-type negotiation) and `read_first_nv12_sample` (the bounded sample-read
loop); `parse_prologue` (container/xcf.rs) split into `parse_xcf_version`/
`parse_image_properties`/`parse_layer_ptrs`. No behavior change anywhere in this tranche.
Verified per file (scoped `cargo test`, or `cargo check`/`cargo clippy` alone for the two
files with no unit tests of their own, tags_to_folders.rs and screenshot/hdr.rs) and at the
end: `cargo test --lib` (753 passed, 0 failed, 18 ignored, matching baseline) and `cargo
test --bin SageThumbs2K` (362 passed, matching tranche 10's baseline), `cargo clippy
--workspace --all-targets -- -D warnings` clean, and `cargo fmt --all --check` clean (after
one `cargo fmt --all` pass to reflow four call sites/signatures the edits left over-width).

**Tranche 12 (2026-08-28): the next 10 worst rows cleared**, worst first: `sample_location`
(mp4.rs) split into `locate_chunk_for_sample` (the `stsc` run-length walk) and
`walk_to_sample_offset` (the capped intra-chunk size walk); `walk` (preview/dbdoc.rs), the
b-tree page visitor, split into `page_header` (page-type/ncells/ptr_base validation) and
`walk_cell` (one cell's full read, using `std::ops::ControlFlow` to distinguish "malformed,
abort the whole walk" from "skip to the next cell", matching the original's `return`/`continue`
mix exactly); `load` (preview/loader.rs) split into `reset_viewer_state` (the per-document reset
block) plus one `try_show_*` helper per early-return content hook (archive listing, DB markdown,
mail markdown, font specimen) and `dispatch_by_content_kind` (the final classify+match), same
shape as the wndproc precedent but for an early-return dispatcher instead of a message loop;
`parse_apev2_cover` (container/audio/ape.rs) split into `read_apev2_item` (one item's
size/flags/key/value read), `apev2_cover_rank` (front/back ranking), and `consider_apev2_cover`
(the description/image validation + best-candidate update); `do_action`
(preview/window/command.rs), a toolbar-button dispatcher, split into one `on_btn_*` handler per
non-trivial `Btn` arm, same wndproc-dispatcher method as prior tranches; `decode_level`
(container/xcf.rs) split into `read_level_tile_pointers` (header + tile-pointer-list read),
`tile_read_window` (the encoded-length estimate/shrink logic), and `decode_and_blit_tile` (one
tile's read+decode+blit/accumulate); `read_info_impl` (strip.rs) split into
`resolve_image_dimensions` (the three-tier dims probe) with its full-decode tier further split
into `resolve_dims_via_full_decode`, and `apply_exif_metadata` (make/model/datetime/DPI/GPS);
`read_info_verbose` (strip.rs) split into `write_file_section`/`write_image_section`/
`write_exif_section`/`write_extra_facts_section`, one per report section, each returning the
bool the caller needs for the final "(none)" line; `paint_lines` (preview/highlight.rs) split
into `gutter_metrics` (line-number column geometry), `record_line_hit` (selection hit-rect
bookkeeping), `draw_gutter_line_number`, and `draw_code_runs`; `tiff_has_raw_ifd_marker`
(streamsrc/rawsniff.rs) split its per-entry IFD0 scan into `raw_ifd_entry_marker`, again using
`ControlFlow<bool>` to keep the three original outcomes (found → `Break(true)`, malformed →
`Break(false)`, keep scanning → `Continue`) distinct from a single boolean return. No behavior
change anywhere in this tranche. Verified per file (scoped `cargo test`) and at the end: `cargo
test --lib` (753 passed, 0 failed, 18 ignored, matching baseline) and `cargo test --bin
SageThumbs2K` (362 passed, matching tranche 11's baseline), `cargo clippy --workspace
--all-targets -- -D warnings` and `cargo clippy --bin SageThumbs2K --features html-preview -- -D
warnings` both clean, and `cargo fmt --all --check` clean (after one `cargo fmt --all` pass,
plus a manual fix for a doc-comment left orphaned above the wrong function by one of the
extractions, caught by the fmt-check diff and by re-reading the file rather than by any tool).

**Tranche 13 (2026-08-28): the next 10 worst rows cleared**, worst first: `shot_paint`
(screenshot/overlay/paint.rs) split into `selection_rect`, `blit_selection_bright`,
`paint_annotations`, `paint_selection_chrome`, `paint_eyedropper_loupe`, and
`blit_frame_to_window`, one per paint phase; `settings_wndproc` (convert.rs), the small
quality-settings popup dispatcher, split into `settings_popup_on_create`/
`settings_popup_on_hscroll`/`settings_popup_on_command`/`settings_popup_on_command_ok`, same
wndproc-dispatcher method as prior tranches; `decode_preview_with_raw_order` (decode.rs)
split into one `try_*_tier` function per format tier (`try_jp2_reduced_tier`,
`try_wic_scaled_jpeg_tier`, `try_video_tier`, `try_xcf_tier`, `try_djvu_tier`,
`try_container_cover_tier`, `try_pdf_tier`), each returning `Option<DynamicImage>` or
`Option<Result<DynamicImage>>` so a tier that has no answer falls through exactly as the
original's inline `if`-chain did; `encode_via_magick` (decode/magick.rs) split into
`magick_encode_target`/`magick_encode_args`/`spawn_magick_child`/`pipe_magick_encode`/
`wait_for_magick_child`/`finish_magick_encode`, matching the function's own
prepare/spawn/pipe/wait/finish phases; `doc_append` (preview/markdown/doc.rs) split into
`doc_append_item`/`doc_append_code`/`doc_append_table` per non-trivial `Block` arm, plus a
hoisted `runs` helper (was a nested fn); `parse_columns` (preview/dbdoc.rs) split into
`table_is_without_rowid`, `parse_pk_constraint_list`, `parse_column_def`,
`column_defs_and_pk` (the walk), and `reorder_without_rowid`; `dispatch_tool` (mcp.rs), an
MCP tool-name dispatcher, split into `dispatch_convert`/`dispatch_pdf`/`dispatch_batch` per
non-trivial tool, same wndproc-dispatcher-style method applied to a string match, plus a
hoisted `want_str_array` helper shared by the two list-taking tools; `collect_leaf_pages`
(container/clip.rs) split its table-INTERIOR-page arm into `queue_interior_children`;
`run_capture_inner` (screenshot/overlay.rs) split into `virtual_screen_metrics`,
`freeze_screen_to_dc`, `build_dimmed_copy`, `seed_dpi_for_capture`, `build_shot_state`, and
`run_overlay_message_loop`, matching its own setup/freeze/dim/seed/state/message-loop
phases; `display_name` (preview/font.rs) split into `find_name_table`, `name_record_rank`,
`read_name_record` (returning a 3-way `NameRecordOutcome`, malformed/skip/value, to
preserve the original's per-record `?` that aborted the WHOLE scan on a malformed record,
distinct from the name-id filter that only skips one record), and `best_name` (the walk).
`scripts/compare-renders.py:main` (cognitive 41/CC 39) was left unchecked: it is a Python
script, not part of this Rust gate or its `cargo test`/`clippy`/`fmt` verification loop.
No behavior change anywhere in this tranche. Verified per file (scoped `cargo test`) and at
the end: `cargo test --lib` (753 passed, 0 failed, 18 ignored, matching baseline) and `cargo
test --bin SageThumbs2K --features html-preview` (363 passed, matching tranche 12's
baseline), `cargo clippy --workspace --all-targets --features html-preview -- -D warnings`
and `cargo clippy --bin SageThumbs2K -- -D warnings` both clean, and `cargo fmt --all
--check` clean (after one `cargo fmt --all` pass to reflow three call sites/signatures the
edits left over-width, plus a blank-line fix for a doc comment that picked up a
`doc_lazy_continuation` clippy lint once moved from a plain `//` comment to a `///` one).

**Tranche 14 (2026-08-28): the next 8 worst rows cleared**, worst first: `extract_cover`
(container/mod.rs), the ebook/comic content-sniff dispatcher, was a single 30-branch if-chain;
grouped into four category helpers (`try_generic_archive_cover`, `try_creative_app_cover`,
`try_ebook_and_cad_cover`, `try_misc_cover`) tried in order via `.or_else()` chaining, preserving
the exact original priority (APK-before-zip, PSD-falls-through-to-magick, etc.) since each helper
still tries its own members in the same sequence internally; `start_convert` (convert.rs) split
into `resolve_convert_outdir` (the placeholder-vs-explicit-folder read), `convert_one_file` (one
source's whole job list) and `produce_convert_job` (one resize job's dispatch by target kind);
`convert_wndproc` (convert.rs), a batch-dialog message dispatcher, split into
`on_convert_create`/`on_convert_command`/`on_convert_progress`/`on_convert_done`, same
wndproc-dispatcher method as prior tranches; `neutralize_lossless_jpeg_orientation`
(verbs/encode.rs) had its nested `normalize` fn (itself the bulk of the complexity, since a nested
`fn`'s body counts toward its enclosing function under this scanner) hoisted to module scope as
`zero_out_ifd0_orientation`; `parse_wav` (container/waveform.rs) split into `scan_wav_chunks` (the
RIFF chunk walk) and `parse_wav_fmt_chunk` (one `fmt ` chunk's field read), with a malformed `fmt`
chunk still fatal to the whole scan via propagated `?`, matching the original; `size_of`
(mp4.rs, `SampleSizes`) split its two match arms into `stsz_size_of`/`stz2_size_of`; `video_key`
(preview/window.rs) split into `video_key_seek`/`video_key_volume`/`video_key_toggle`, tried via
`||` short-circuit in the same priority the original flat match checked keys in; `first_run_wndproc`
(first_run.rs) split into `on_first_run_create`/`on_first_run_command` plus a hoisted
`is_dim_caption` predicate (was a 7-way inline `||` chain), same wndproc-dispatcher method. No
behavior change anywhere in this tranche. Verified per file (scoped `cargo test`, or `cargo build`
alone for the three files with no unit tests of their own, convert.rs, first_run.rs,
preview/window.rs) and at the end: `cargo test --lib` (753 passed, 0 failed, 18 ignored, matching
baseline) and `cargo test --bin SageThumbs2K --features html-preview` (363 passed, matching tranche
13's baseline), `cargo clippy --workspace --all-targets --features html-preview -- -D warnings`
clean (after adding `WavFmt`/`WavData` type aliases for a clippy::type_complexity hit on
`scan_wav_chunks`'s return type, and `#[allow(clippy::too_many_arguments)]` on
`produce_convert_job`, same pattern already used elsewhere in these files), and `cargo fmt --all
--check` clean (after one `cargo fmt --all` pass to reflow `resolve_convert_outdir`'s
over-width `is_placeholder` line).

**Tranche 15 (2026-08-28): the next 9 worst rows cleared**, worst first: `to_sfnt`
(preview/woff.rs), the WOFF-to-sfnt reconstructor, split into `read_woff_table` (one
table-directory entry's read/decompress), `read_woff_tables` (the sorted-by-tag walk), and
`assemble_sfnt` (header + directory + data write), with the local `Table` struct hoisted to
module scope as `WoffTable`; `load_static` (preview/loader.rs), the headless-shot sync-decode
dispatcher, split into `try_static_source_view`/`try_static_archive_listing`/
`try_static_db_markdown`/`try_static_mail_markdown` (one per content-specific hook, same shape
as `load`'s existing `try_show_*` helpers) plus `apply_static_image`/
`apply_static_text_or_markdown`/`apply_static_content_kind` for the final by-kind dispatch;
`parse_iloc` (strip/isobmff.rs) split into `parse_iloc_sizes` (version + field-width read),
`parse_iloc_count`, and `parse_iloc_item` (one entry, returning the offset-past-entry paired
with its optional `IlocEntry` type alias); `parse_keyframe_header` (bin/vdec/vp9.rs), a VP9
bitstream-header parse whose ~20 bit-reader calls each count as a branch under this scanner,
split into `parse_frame_marker_and_profile`/`verify_keyframe_flags`/`parse_color_config`/
`parse_frame_size`, one per spec-section field group, each with its own small `take` closure;
`extract_best` (container/psp.rs) split into `collect_composite_parts` (the sub-block walk),
`best_composite` (the largest-by-pixel-area rank), and `extract_jpeg_composite`/
`extract_plane_composite` per storage-kind branch; `feed` (preview/mdhtml.rs), the raw-HTML
tokenizer's state-machine loop, split into `advance_in_comment`/`advance_in_skip_tag`/
`advance_at_lt`/`advance_text`, one per loop state, each returning `ControlFlow<(), usize>`
(`Break` when the fragment ends mid-state, matching the original's early `return`) so `feed`
itself became a thin four-way dispatch; `cue_points` (mkv.rs) split its triple-nested
CuePoint/CueTrackPositions/child walk into `cue_track_position` (one CueTrackPositions
occurrence's video-track-position-if-matched plus any-track fallback) and `parse_cue_point`
(one CuePoint, combining possibly-several such occurrences exactly as the original's
`get_or_insert`/`is_none()` guards did); `strip` (strip/svgmeta.rs) split into
`try_drop_element` (the top-level metadata-element cut, falling through to the normal path on
"not a candidate" or "never closed", same as the original's nested `if let`) and `step_depth`
(the nesting-depth tracker); `blocks_rgba8` (decode/dds.rs), the BC1-7 block-grid walk, split
into `try_fast_block_mean` (the whole-block mean shortcut), `decode_block_tile` (the per-format
match, returning `false` for BC6H to abort the whole walk exactly as the original's bare
`return` did), and `write_decoded_block` (mean vs. tile-copy write). No behavior change
anywhere in this tranche. Verified per file (scoped `cargo test`, or `cargo build`/`cargo
check` alone for mdhtml.rs, which has no unit tests of its own) and at the end: `cargo test
--lib` (753 passed, 0 failed, 18 ignored, matching baseline), `cargo test --bin SageThumbs2K
--features html-preview` (363 passed, matching tranche 14's baseline), `cargo test --bin
st2k` (22 passed, matching baseline), `cargo clippy --workspace --all-targets --features
html-preview -- -D warnings` clean (after adding an `IlocEntry` type alias for a
`clippy::type_complexity` hit on `parse_iloc_item`'s return type, and
`#[allow(clippy::too_many_arguments)]` on `try_fast_block_mean`, same pattern already used
elsewhere in these files), and `cargo fmt --all --check` clean (after one `cargo fmt --all`
pass to reflow several signatures/expressions the edits left over-width).

**Tranche 16 (2026-08-28): the next 11 worst rows cleared**, worst first: `eligible_bt601_still`
(decode/avifmf.rs) had its nested `walk` fn (itself over the gate from the nesting) hoisted to
module scope as `walk_ipco_boxes` with its `Found` struct renamed `FoundIpcoBoxes`, plus a
`validate_bt601_eligibility` helper for the final CICP/av1C eligibility checks; `fuzz_extract_cover`
(container/mod.rs), a `#[test]` fuzzer, split into `fuzz_seed_corpus`, `mutate_fuzz_input`, and
`report_fuzz_crashes`; `reference` (decode/jp2/dwt.rs) was found already under gate, a leftover row
from tranche 11's split of its own caller (`filtr_1d_matches_the_original_three_buffer_algorithm`)
that this doc never checked off, so it needed no further work, only verification; `filtr_1d`
(decode/jp2/dwt.rs), the production 1D lifting step, split into `lift_single_sample`,
`lift_reversible`, and `lift_irreversible`, one per original branch; `wndproc` (prebuild_dlg.rs)
split into `on_pb_create`/`on_pb_progress`/`on_pb_done`/`on_pb_command`, same wndproc-dispatcher
method as prior tranches; `tiff_ifd0_is_reduced` (streamsrc/rawsniff.rs) split its per-entry IFD0
scan into `reduced_subfile_entry`, using `ControlFlow<bool>` the same way its sibling
`tiff_has_raw_ifd_marker`/`raw_ifd_entry_marker` already does in this file; `get_thumbnail_inner`
(thumbprovider.rs) split into `decode_thumb_source` (the `StreamSource` dispatch) and
`retry_decode_by_extension` (the declined-tiers fallback); `parse_aiff` (container/waveform.rs)
split into `parse_aiff_comm`, `scan_aiff_chunks`, and `finish_aiff_pcm`, matching the `parse_wav`
precedent already in this file: every early `?` abort in the scan becomes a loop `break`, which is
behaviorally identical since a missing `comm`/`ssnd` fails the caller's own `comm?`/`ssnd?` either
way; `mime_body` (preview/mailmsg.rs) split its multipart-part classification loop body into
`handle_multipart_part`; `read_layer_head` (container/xcf.rs) split its property-record loop into
`read_layer_properties` + `apply_layer_property`, collecting into a new `LayerProps` struct instead
of four loose locals; `segment_map` (mkv.rs) split into `scan_segment_front` (the front-of-segment
element walk, returning a new `FrontScan` struct) and `resolve_via_seekhead` (filling in whatever
the front walk missed). No behavior change anywhere in this tranche. Verified per file (scoped
`cargo test`, `mime_body` via the `SageThumbs2K` bin target since `preview/mailmsg.rs` is a bin
module, `wndproc`/`get_thumbnail_inner` via `cargo build`/`cargo test --lib` compile-clean since
neither has unit tests of its own) and at the end: `cargo test --lib` (753 passed, 0 failed, 18
ignored, matching baseline) and `cargo test --bin SageThumbs2K --features html-preview` (363 passed,
matching tranche 15's baseline), `cargo clippy --workspace --all-targets --features html-preview --
-D warnings` clean (after adding an `AiffCommSsnd` type alias for a `clippy::type_complexity` hit on
`scan_aiff_chunks`'s return type, same pattern already used elsewhere in these files) and `cargo
clippy --bin SageThumbs2K -- -D warnings` clean, and `cargo fmt --all --check` clean (after one
`cargo fmt --all` pass to reflow two comments/call sites the edits left over-width).

**Tranche 17 (2026-08-28): the next 10 worst rows cleared**, worst first:
`scaled_pre_pass_sweep_by_format` (decode/tests.rs), a `#[test]` measurement sweep, split into
`sweep_dims_and_scales`/`sweep_best_us`/`sweep_ms`/`sweep_fidelity`/`sweep_print_row` (the two
nested `best_us`/`ms` fns hoisted to module scope too, since a nested fn's body counts toward
its enclosing function under this scanner); `gif_prefers_wic` (decode.rs) split its two
heaviest match arms into `gif_skip_extension`/`gif_full_canvas_descriptor`; `extract`
(container/max.rs) split into `thumbnail_value_offset` (the property-list walk) and
`read_cf_value` (the `VT_CF` tag/data read); `check_registration` (doctor.rs) split into
`check_one_handler` (one COM handler's registered/exists/loads checks) and
`check_approved_list`; `spawn_decode` (preview/content.rs) had its worker-thread closure split
into `try_post_streamed`/`try_post_quick_first_paint`/`try_post_animation` (plus a new
`post_anim` mirroring the existing `post_render`) /`decode_and_post_static`, one per
decode-tier attempt, leaving the closure a thin sequence of early returns; `window_under`
(screenshot/overlay/input.rs) split into `window_is_candidate` (visible/minimized/cloaked
gate), `window_visual_bounds` (DWM extended-frame-bounds with `GetWindowRect` fallback), and
`clamp_to_overlay`; `scan_top_level` (mp4.rs) had its per-box header read (size32/extended/
`decode_box_size`) split into `read_top_level_header`, folding both prior break-on-failure
points into one `else`-branch on its `Option`; `thumbnail_from_resources` (container/psd.rs)
split into `resource_block_header` (one `8BIM` block's id/data range) and `thumbnail_jpeg`
(the id/format/magic checks on that block's payload); `resolve_icon_path` (container/apk.rs)
split into `find_package` (package-id lookup with the package-0 shared-library fallback),
`collect_icon_candidates` (the TYPE-chunk walk, recursing into `resolve_icon_path` for
`TYPE_REFERENCE`), and `best_density_candidate` (the final max-by-density pick); `epsi_preview`
(container/eps.rs) split into `parse_epsi_header` (the `%%BeginPreview:` field parse and
bounds validation, collected into a new `EpsiHeader` struct), `read_epsi_packed` (the hex-line
read loop), `unpack_epsi_rows` (the lower-left-up-to-top-down unpack), and `epsi_terminates`
(the blank-line-skipping `%%EndPreview` check). No behavior change anywhere in this tranche.
Verified per file (scoped `cargo test`, or `cargo build`/compile-clean for the two files with
no unit tests of their own, preview/content.rs and container/max.rs beyond its one existing
smoke test) and at the end: `cargo test --lib` (753 passed, 0 failed, 18 ignored, matching
baseline) and `cargo test --bin SageThumbs2K --features html-preview` (363 passed, matching
tranche 16's baseline), `cargo clippy --workspace --all-targets --features html-preview -- -D
warnings` clean, and `cargo fmt --all --check` clean (one `cargo fmt --all` pass needed
mid-tranche). One mid-edit mistake, caught before it reached clippy: moving
`gif_prefers_wic`'s helpers into place first landed them between its existing doc comment and
its `fn`, stealing that comment for the wrong function; fixed by moving the helpers above the
doc comment instead.

**Tranche 18 (2026-08-28): the next 10 worst rows cleared**, worst first:
`mp4_remux_moov` (streamsrc/mp4remux.rs) split into `locate_mdat_and_moov` (the top-level
box walk), `retain_head_with_rewritten_mdat` (keep-and-rewrite of the retained head), and
`read_tail_moov` (the tail pull); `find_sqli` (container/clip.rs) split its nested
`CHNKHead`-pointer if-chain into `chnk_head_shortcut`, leaving the sequential hop-walk as
`chnk_walk_fallback`; `render` (pdf.rs) split into `copy_bytes_to_pdf_stream`,
`scaled_page_dims`, and `rasterize_page_to_png`, one per phase; `encode_to_opts`
(verbs/encode.rs) split its three heaviest match arms into `encode_lossy_webp` (still
`#[cfg(feature = "webp-lossy")]`), `encode_png_variant`, and `encode_pnm_variant`;
`scrub_mouse_down` (preview/transport.rs) split into `nav_arrow_hit` (the sibling-nav zone
test) and `dispatch_scrub_click` (the play/speed/arrows/loop/volume/mute/track chain, same
order); `parse_header` (decode/dds.rs) split its DX10/FourCC/masks branch into
`resolve_pixel_layout`; `slice_walk` (flv.rs) split into `flv_first_tag_pos` (header
validate) and `read_tag_header` (one tag's 11-byte header + truncation check); `url_at`
(preview/markdown/parse.rs) split into `is_url_left_boundary`, `url_scheme_at`,
`scan_url_bytes`, and `trim_trailing_punct`, one per parse phase; `col_at`
(preview/highlight.rs) split into `display_units` (the tab/surrogate-aware display map
build), `measure_display_extents` (the single `GetTextExtentExPointW` call), and
`nearest_char_boundary` (the caret-snap walk); `encode_pam_streaming`
(verbs/encode/streaming.rs) split into `write_pam_header` and `write_pam_pixels`. No
behavior change anywhere in this tranche. Verified per file (scoped `cargo test`, or
`cargo build`/`cargo test --bin SageThumbs2K` for the bin-only modules: preview/transport.rs,
preview/markdown/parse.rs, preview/highlight.rs) and at the end: `cargo test --lib` (753
passed, 0 failed, 18 ignored, matching baseline) and `cargo test --bin SageThumbs2K
--features html-preview` (363 passed, matching tranche 17's baseline), `cargo clippy
--workspace --all-targets --features html-preview -- -D warnings` and `cargo clippy --lib
--features webp-lossy -- -D warnings` both clean, and `cargo fmt --all --check` clean.

## Queue

Ranked worst first (cognitive complexity, then cyclomatic complexity). Checked rows were
cleared by a burndown tranche; see git log for the commits.

**Tranche 10 (2026-08-28): the next 8 worst rows cleared**, worst first: `extract`
(container/mobi.rs) split into `cover_via_exth_or_base` (the EXTH-lookup-then-first-image
phase); `delimited_table` (preview/docconv.rs) split into `parse_delimited_rows` (the
RFC-4180-ish parse loop) and `render_row_table` (the GFM pipe-table render); `popup_wndproc`
(settings_dlg/menuitems.rs) split into `on_create`/`on_notify`/`on_measureitem`/
`on_drawitem`/`on_command`/`on_destroy`, same wndproc-dispatcher method as prior tranches;
`largest_embedded_jpeg` (decode/tiers.rs) split into `span_at_soi` (one SOI candidate's
measure-and-fold), `consider_candidate` (the capped/overall bookkeeping) and `bump_seen`
(the 64-candidate cap); `jpeg_sof_dims` (container/util.rs) split into `next_marker` (the
fill-byte-skip-and-read-marker prologue, the same shape `jpeg_span` already has inline) and
`sof0_dims` (the SOF0/1/2 width/height read); `decode_scaled` (decode/exrscale.rs) split
into `resolve_rgb_layer`, `resolve_channels`, `validate_layer_dims`, `channel_slot` (the
per-line R/G/B/A slot lookup, replacing a 4-arm match), and `image_from_accumulators`, all
called from the original decode/filter/decompress skeleton, which stays in `decode_scaled`
unchanged; `eyedropper_wndproc` (eyedropper.rs) split into `on_mousemove`/`on_button_down`/
`on_keydown_space`/`on_keydown_tab`/`on_keydown_digit`/`on_destroy`, same wndproc-dispatcher
method; `cluster_keyframe` (mkv.rs) split into `simple_block_keyframe` and
`block_group_keyframe` per `id` arm. No behavior change anywhere in this tranche. Verified
per file (scoped `cargo test`) and at the end: `cargo test --lib` (753 passed, 0 failed, 18
ignored, matching baseline) and `cargo test --bin SageThumbs2K` (362 passed, matching
tranche 9's baseline), `cargo clippy --lib -- -D warnings` and `cargo clippy --bin
SageThumbs2K -- -D warnings` both clean, and `cargo fmt --all --check` clean (after one
`cargo fmt --all` pass to reflow three call sites/signatures the edits left over-width).

**Tranche 9 (2026-08-28): the next 9 worst rows cleared**, worst first: `paint_into`
(preview/paint.rs) split into `paint_content` (a thin `ContentKind` match) plus one
`paint_content_*` helper per arm, and `paint_caption`/`paint_caption_title`/
`paint_caption_toolbar` for the caption strip; `tokenize` (preview/highlight/lex.rs) split into
one `try_*` helper per token kind (carried block comment, line comment, block comment open,
string, number, identifier), tried in order from a thin scan loop; `load_sync`
(preview/loader.rs) split into `load_sync_play_video`/`load_sync_pdf`/`load_sync_frame` per
branch; `draw_table` (preview/markdown/tables.rs) split into its five documented steps as named
helpers (`measure_natural_col_widths`, `measure_row_heights`, `draw_table_rows`,
`draw_table_grid`, `draw_dropped_col_note`); `list_subclass` (settings_dlg/list.rs) split its
`WM_NOTIFY` arm into `on_notify` (dispatch) plus `on_notify_header_endtrack` and
`on_notify_header_customdraw`, same wndproc-dispatcher method as prior tranches;
`strip_metadata` (strip.rs) split its JPEG/PNG match arms into `strip_jpeg`/`strip_png`;
`extract` (container/blend.rs) split into `read_block_header` and `decode_test_block`, this
scanner counts the `?` operator as a branch, and the original had ~16 of them inline;
`decode_test_block`'s outer `Option` preserves the exact original short-circuit semantics (a
failure there used to abort the whole search via `?`, not just skip one block) via
`Option<Option<_>>`, documented at the helper; `ensure_visible` (preview/selection.rs) split into
`ensure_visible_text`/`ensure_visible_markdown` per `ContentKind` arm; `type_chunk_value`
(container/apk.rs) split into `find_entry_offset` (the sparse/dense entry-offset table lookup)
and `read_res_value` (the `Res_value` read at that offset). No behavior change anywhere in this
tranche. Verified per file (scoped `cargo test`) and at the end: `cargo test --lib` (753 passed,
0 failed, 18 ignored, matching baseline) and `cargo test --bin SageThumbs2K --features
html-preview` (363 passed, matching tranche 8's baseline), `cargo clippy --lib -- -D warnings`
and `cargo clippy --bin SageThumbs2K [--features html-preview] -- -D warnings` all clean (two
owner-draw helpers and one grid helper picked up `#[allow(clippy::too_many_arguments)]`, same
pattern already used elsewhere in these files), and `cargo fmt --all --check` clean.

**Tranche 1 (2026-08-28): 8 of 8 planned rows cleared**, in order: `transform` (jpegtran.rs),
`decode_tile` (jp2/mod.rs), `decode_code_block` (jp2/mq.rs), `read_streams` (ole.rs),
`isobmff_has_hevc_aux_alpha` (color.rs), `parse` (jp2/codestream.rs), `parse_ply` (mesh.rs),
`extract` (ilbm.rs). Each was split into named helper functions covering one phase of the
original function's work; no behavior change, verified with `cargo test` (module-scoped
during the pass, full suite at the end: 753 passed, 0 failed, 18 ignored) plus `cargo clippy`
and `cargo fmt --all --check` on every touched file. The `color.rs` item additionally hoisted
four already-nested helper functions (`boxes`/`item_id`/`associated_items`/
`auxl_targets_primary`) out to module scope, which is why `isobmff_associated_items` below
picked up a new location without changing its own complexity number.

**Tranche 2 (2026-08-28): the residue tranche 1 left behind, 24 gating functions across the
same 8 files, cleared.** Odin's own rescan after tranche 1 (`gatingErrors` 187) showed the
first pass had split each giant function into helpers, but several of those helpers were
STILL over the gate, one file was untouched by name (`jpegtran.rs:xform_block`), and
`isobmff_has_hevc_aux_alpha`, `parse` (codestream.rs) and `extract` (ilbm.rs) each had more
work to do beyond their tranche-1 split. Covered: `parse_headers`/`decode_scan`/
`xform_block` (jpegtran.rs); `progression_order`/`decode_reduced`/
`build_component_resolutions`/`build_precinct_states` (jp2/mod.rs); `cleanup_pass`
(jp2/mq.rs); `read_mini_stream` (ole.rs); `isobmff_has_hevc_aux_alpha`/`isobmff_color_icc`/
`avif_wic_verdict`/`isobmff_associated_items` (color.rs); `parse`/`parse_palette`/
`parse_siz`/`index_tile_part` (jp2/codestream.rs); `parse_obj`/`render`/`parse_ply_header`
(mesh.rs); `extract` (ilbm.rs). `parse_headers` and `parse` (codestream.rs) needed a SECOND
pass each after the first extraction still gated; see the threshold-discipline note above
for why. Verified with `cargo test --lib` (753 passed, 0 failed, 18 ignored, matching
tranche 1's baseline exactly) plus `cargo clippy --workspace --all-targets -D warnings` and
`cargo fmt --all --check`, both clean, after every file. Odin's portable rescan afterward:
**0 cognitive/cyclomatic gating errors left in any of the 8 files** (verified against the
raw per-finding JSON, not just the truncated "top 50" audit `.md`); repo-wide gating errors
187 -> 163.

**Tranche 3 (2026-08-28): `wndproc` (window.rs), the first of the three giant Win32
dispatchers, cleared.** Split the 767-line message dispatcher into a thin match (one line per
message, each calling a named handler) plus ~30 extracted handler functions, one per
nontrivial message arm, with `WM_MOUSEMOVE`, `WM_LBUTTONDOWN` and `WM_KEYDOWN` further split
into sub-handlers (`mousemove_drag`/`mousemove_hover`; `lbuttondown_pane`;
`keydown_copy_select`/`keydown_video_and_home`/`keydown_page_nav`/`keydown_lifecycle`) once
their own extracted complexity was still too high for one function. Same message routing,
same `LRESULT`s (including every `DefWindowProc` fallthrough), same unsafe blocks, no
behavior change intended. Verified with `cargo test --bin SageThumbs2K` (the module's own 11
`preview::window::tests` pass), `cargo test --lib` (753 passed, 0 failed, 18 ignored,
matching the tranche 1/2 baseline exactly, this tranche touched a `bin`, not `lib`, so the
lib suite is an unchanged-behavior check rather than direct coverage), `cargo clippy --bin
SageThumbs2K -- -D warnings`, and `cargo fmt --all --check`, all clean. See the standing note
above: build/test/clippy passing is preflight-level verification, not proof the actual window
still behaves right under a mouse and keyboard, a human UI click-through before the next
release is still recommended.

**Tranche 4 (2026-08-28): `shot_wndproc` (screenshot/overlay/input.rs), the second of the
three giant Win32 dispatchers, cleared.** Split the message dispatcher into a thin match
(one line per message, each calling a named `on_*` handler) plus ~20 extracted functions,
with the two heaviest arms further split: `WM_LBUTTONDOWN` into
`on_lbuttondown`/`on_lbuttondown_selected`/`try_color_flyout_click`/`try_text_flyout_click`/
`try_toolbar_button_click`/`apply_selection_click`, and `WM_MOUSEMOVE` into
`on_mousemove`/`update_eyedropper_loupe`/`update_active_drag`/`update_window_hint`/
`update_hover_button`. `WM_LBUTTONUP`'s OCR-launch branch became
`finish_selection_drag`/`finish_draw_drag`, and `WM_CHAR`'s surrogate-pair handling became
`push_typed_char`, both to keep the coordinator function's own complexity under the gate (see
the threshold-discipline note above). Same message routing, same `LRESULT`s (including the
`WM_SETCURSOR` `DefWindowProcW` fallthrough), same unsafe blocks, no behavior change intended.
Verified with `cargo test --bin SageThumbs2K overlay::input` (the module's own 9
`screenshot::overlay::input::tests` pass), `cargo test --lib --bins` (388 passed across all
three bin/lib targets, 0 failed), `cargo clippy --bin SageThumbs2K -- -D warnings`, and `cargo
fmt --all --check`, all clean. Same preflight-only caveat as tranche 3: a human UI
click-through before the next release is still recommended.

**Tranche 8 (2026-08-28): the next 9 worst rows cleared**, worst first: `run_action`
(verbs/actions.rs) split into per-verb `handle_*` helpers (handle_convert,
handle_transform, handle_clipboard, handle_wallpaper, handle_combine_to_pdf,
handle_combine_to_cbz, handle_ocr, handle_strip_metadata, handle_resize_img,
handle_shrink_for_email, handle_compress_to_size, handle_set_folder_icon,
handle_files_to_folder, handle_sort_by_dimensions, handle_tags_to_folders), leaving
`run_action` a thin 21-arm dispatcher; `daemon_wndproc` (screenshot/daemon.rs) split into
named `on_*` message handlers, same wndproc-dispatcher method as tranches 3-5;
`decode_any_with_wic_target` (decode.rs) split into per-tier helpers (try_jxl_tier,
try_dds_tier, try_wic_thumbnail_fastpath, try_image_tier/`ImageTierOutcome`,
try_raw_preview_tier, route_isobmff_wic_quirks/`WicQuirkRoute`,
try_embedded_jpeg_last_resort); `stream_source_with_caps` (streamsrc.rs) split into
try_video_source, try_exr_source, try_raw_preview_fast, oversized_rescue; `render`
(preview/markdown.rs) split its `Block` match's five heaviest arms into paint_heading/
paint_para/paint_code/paint_item/paint_quote (Rule/Table/Image stayed inline, already
shallow); `apply_settings` (settings_dlg/values.rs) split into 10 per-section helpers
(apply_thumbnail_and_badge_settings, apply_menu_and_misc_toggles,
apply_menu_item_list_order, apply_menu_preview_and_theme, apply_screenshot_tool_prefs,
apply_container_settings, apply_tuning_numbers, apply_screenshot_hotkeys,
apply_quick_preview_and_screenshot_enable, apply_format_flags), called in the same order;
`stamp` (badge.rs) split into badge_geometry (+`BadgeGeom`), put_px, paint_chip,
paint_glyphs; `run_shot` (preview/shot.rs) split each `--shot` CLI option's handling into
apply_wait_ms, apply_size, apply_wheel, apply_scroll, apply_sel, apply_find,
bench_repaint_if_requested; `create` (preview/webview.rs, `html-preview` feature) split
into resolve_profile_dir, create_environment, create_controller, apply_lockdown,
install_local_mode_guards. No behavior change anywhere in this tranche. Verified per file
(scoped `cargo test`/`cargo build`) and at the end: `cargo test --lib` (753 passed, 0
failed, 18 ignored, matching baseline) and `cargo test --bin SageThumbs2K` (362 passed,
363 with `--features html-preview`, the one extra being a feature-gated test), `cargo
clippy --lib -- -D warnings` and `cargo clippy --bin SageThumbs2K -- -D warnings` both
clean (also re-checked with `--features html-preview` for the webview.rs row, since that
module only compiles under the feature), and `cargo fmt --all --check` clean.

**Tranche 7 (2026-08-28): 6 more of the next-worst rows cleared.** `run` (cli.rs) split
into `run_thumbnail`/`run_convert`/`run_batch`/`run_prebuild`/`run_rotate`/`run_compress`/
`run_bench_decode`/`run_register` per-verb helpers plus a `prebuild_sizes` helper, leaving
`run` a thin per-verb dispatcher; `QueryContextMenu` (contextmenu/com.rs) split into
`selection_kinds`/`command_budget`/`insert_quick_verb_groups`/`insert_sagethumbs_submenu`;
`parse_packet` (jp2/packet.rs) split into `parse_block_contribution` (the triple-nested
walk's per-block body) plus `decode_zero_bitplanes`/`grow_lblock`; `decode_channels`
(container/psp.rs) split into `collect_channel_planes` (the sub-block walk) and `render_rgb`
(the depth-specific render), with a `ChannelPlanes` type alias for the intermediate tuple;
`dispatch` (preview/mdhtml.rs) split into `dispatch_inline`/`dispatch_block`/
`dispatch_list_table` category handlers (leaf-toggle tags, paired open/close block tags,
lists + HTML table tags) tried in sequence; `recognize` (ocr.rs) split into
`decode_source`/`decode_to_bitmap`/`create_ocr_engine`/`collect_lines`/`line_word_boxes`.
No behavior change anywhere in this tranche. Verified per file (scoped `cargo test`) and at
the end: `cargo test --lib` (753 passed, 0 failed, 18 ignored, matching baseline) and
`cargo test --bin SageThumbs2K` (362 passed, matching tranche 5's baseline), `cargo clippy
--lib -- -D warnings` and `cargo clippy --bin SageThumbs2K -- -D warnings` both clean, `cargo
fmt --all --check` clean.

**Tranche 6 (2026-08-28): 9 of the next-worst rows cleared** (`main`/`run_shot_mode` split
(main.rs), `run_block` (inline.rs), `handle_key` (overlay/input.rs), `about_wndproc` (about.rs),
`parse_sps` + `keyframe_mini_mp4` (flv.rs), `apply_v3_layout` (navrail.rs), `jpeg_span`
(container/util.rs), `decode_rle` (xcf.rs)). This tranche's code was already committed (git log:
`1ef2113` through `e1a36d1`) by an earlier pass through this same queue that split each function
into named helpers (e.g. `dispatch_diagnostic_modes`/`dispatch_update_modes`/etc. out of `main`;
`tokenize_runs`/`break_into_lines`/`draw_wrapped_lines` out of `run_block`;
`on_key_escape`/`on_key_enter`/`on_key_undo_redo`/etc. out of `handle_key`; per-message handlers
out of `about_wndproc`; `parse_chroma_format`/`skip_pic_order_cnt_fields`/
`parse_frame_geom_fields`/`frame_geom_to_pixels` out of `parse_sps`, `read_flv_header`/
`handle_video_tag`/`TagOutcome` out of `keyframe_mini_mp4`; `place_row` out of
`apply_v3_layout`; `skip_length_prefixed_segment`/`skip_entropy_coded_scan` out of `jpeg_span`;
`decode_rle_opcode`/scatter helpers out of `decode_rle`) but this doc was never updated to check
the rows off and no verification pass had been recorded. This session verified it: every listed
function now reads as a thin coordinator delegating to its extracted helpers (visually confirmed
under gate), `cargo test --lib` (753 passed, 0 failed, 18 ignored, matching the established
baseline exactly) and `cargo test --bin SageThumbs2K` (362 passed, 0 failed, matching tranche 5's
baseline) both pass, `cargo clippy --bin SageThumbs2K -- -D warnings` and `cargo clippy --lib -- -D
warnings` are clean, and `cargo fmt --all --check` is clean.

**Tranche 5 (2026-08-28): `wndproc` (settings_dlg/mod.rs), the third and last of the three
giant Win32 dispatchers, cleared.** Split the message dispatcher into a thin chain of
message-category dispatchers (lifecycle/app messages, command-or-notify, paint/draw, timer-or-
scroll, each an `Option<LRESULT>` fall-through, same pattern the top-level `wndproc` now
uses) plus named handlers for every nontrivial message arm. `WM_COMMAND`'s ~30-arm inner match
was itself over the gate even as a single extracted function, so it was further split into
`on_command_dialog`/`on_command_shot`/`on_command_sync_nav`/`on_command_admin` (grouped by
concern: dialog chrome, screenshot controls, sync/nav/nudge, admin buttons), with
`on_select_clear_all`/`on_search_filter_changed`/`on_shot_set_dir`/`on_shot_restart`/
`on_banner_click` pulled out of their arms for the same reason. `WM_NOTIFY` became
`on_notify_begindrag`/`on_notify_customdraw`/`on_notify_itemchanged`/`on_notify_link_or_tip`,
and `WM_DRAWITEM`'s owner-draw-static branch became `on_drawitem_static`. The pre-match
`WM_CTLCOLORSTATIC` special-casing (four live-state color overrides ahead of the generic
`dark_ctlcolor`) became its own `special_ctlcolor`, checked first, exactly as before. Same
message routing, same `LRESULT`s (including every `DefWindowProcW` fallthrough), same unsafe
blocks, no behavior change intended. Verified with `cargo test --bin SageThumbs2K settings_dlg`
(the module's own 27 `settings_dlg::*` tests pass), `cargo test --lib` (753 passed, 0 failed,
18 ignored, matching the tranche 1/2 baseline exactly) and `cargo test --bin SageThumbs2K` (362
passed, 0 failed), `cargo clippy --bin SageThumbs2K -- -D warnings`, and `cargo fmt --all
--check`, all clean. Same preflight-only caveat as tranches 3 and 4: a human UI click-through
before the next release is still recommended.

| done | cog | CC | function | location |
|---|---|---|---|---|
|x| 579 | 205 | `wndproc` | src/bin/app/preview/window.rs:709 |
|x| 480 | 124 | `shot_wndproc` | src/bin/app/screenshot/overlay/input.rs:128 |
|x| 402 | 146 | `wndproc` | src/bin/app/settings_dlg/mod.rs:1041 |
|x| 208 | 77 | `main` | src/bin/app/main.rs:195 |
|x| 207 | 86 | `transform` | src/jpegtran.rs:461 |
|x| 203 | 73 | `decode_tile` | src/decode/jp2/mod.rs:257 |
|x| 182 | 46 | `decode_code_block` | src/decode/jp2/mq.rs:275 |
|x| 170 | 67 | `read_streams` | src/container/ole.rs:71 |
|x| 158 | 69 | `isobmff_has_hevc_aux_alpha` | src/decode/color.rs:600 |
|x| 153 | 55 | `parse` | src/decode/jp2/codestream.rs:316 |
|x| 120 | 42 | `run_block` | src/bin/app/preview/markdown/inline.rs:192 |
|x| 116 | 77 | `handle_key` | src/bin/app/screenshot/overlay/input.rs:620 |
|x| 114 | 49 | `parse_ply` | src/decode/mesh.rs:197 |
|x| 113 | 59 | `extract` | src/container/ilbm.rs:42 |
|x| 111 | 43 | `about_wndproc` | src/bin/app/about.rs:798 |
|x| 109 | 64 | `parse_sps` | src/flv.rs:552 |
|x| 108 | 36 | `apply_v3_layout` | src/bin/app/settings_dlg/navrail.rs:875 |
|x| 107 | 36 | `jpeg_span` | src/container/util.rs:143 |
|x| 106 | 29 | `decode_rle` | src/container/xcf.rs:756 |
|x| 93 | 48 | `run_action` | src/verbs/actions.rs:333 |
|x| 92 | 52 | `run` | src/bin/cli.rs:140 |
|x| 92 | 37 | `keyframe_mini_mp4` | src/flv.rs:56 |
|x| 88 | 32 | `QueryContextMenu` | src/contextmenu/com.rs:52 |
|x| 84 | 23 | `parse_packet` | src/decode/jp2/packet.rs:217 |
|x| 76 | 32 | `decode_channels` | src/container/psp.rs:244 |
|x| 76 | 43 | `daemon_wndproc` | src/bin/app/screenshot/daemon.rs:456 |
|x| 75 | 42 | `decode_any_with_wic_target` | src/decode.rs:292 |
|x| 74 | 47 | `dispatch` | src/bin/app/preview/mdhtml.rs:217 |
|x| 72 | 37 | `recognize` | src/ocr.rs:130 |
|x| 69 | 37 | `stream_source_with_caps` | src/streamsrc.rs:105 |
|x| 68 | 33 | `render` | src/bin/app/preview/markdown.rs:265 |
|x| 68 | 40 | `apply_settings` | src/bin/app/settings_dlg/values.rs:414 |
|x| 66 | 30 | `stamp` | src/badge.rs:161 |
|x| 66 | 38 | `run_shot` | src/bin/app/preview/shot.rs:13 |
|x| 65 | 30 | `create` | src/bin/app/preview/webview.rs:43 |
|x| 64 | 28 | `paint_into` | src/bin/app/preview/paint.rs:141 |
|x| 64 | 28 | `tokenize` | src/bin/app/preview/highlight/lex.rs:199 |
|x| 63 | 20 | `load_sync` | src/bin/app/preview/loader.rs:229 |
|x| 62 | 27 | `draw_table` | src/bin/app/preview/markdown/tables.rs:14 |
|x| 62 | 29 | `list_subclass` | src/bin/app/settings_dlg/list.rs:429 |
|x| 61 | 28 | `strip_metadata` | src/strip.rs:95 |
|x| 59 | 25 | `extract` | src/container/blend.rs:16 |
|x| 58 | 22 | `ensure_visible` | src/bin/app/preview/selection.rs:323 |
|x| 57 | 33 | `type_chunk_value` | src/container/apk.rs:668 |
|x| 57 | 27 | `extract` | src/container/mobi.rs:10 |
|x| 56 | 26 | `delimited_table` | src/bin/app/preview/docconv.rs:72 |
|x| 55 | 23 | `popup_wndproc` | src/bin/app/settings_dlg/menuitems.rs:110 |
|x| 54 | 18 | `largest_embedded_jpeg` | src/decode/tiers.rs:162 |
|x| 53 | 23 | `jpeg_sof_dims` | src/container/util.rs:214 |
|x| 53 | 18 | `filtr_1d_matches_the_original_three_buffer_algorithm` | src/decode/jp2/dwt.rs:259 |
|x| 52 | 31 | `decode_scaled` | src/decode/exrscale.rs:73 |
|x| 51 | 17 | `lib_side_translation_keys_all_survive_the_dll_subset` | src/i18n.rs:229 |
|x| 51 | 18 | `parse_obj` | src/decode/mesh.rs:150 |
|x| 51 | 23 | `eyedropper_wndproc` | src/bin/app/eyedropper.rs:376 |
|x| 50 | 15 | `cluster_keyframe` | src/mkv.rs:336 |
|x| 50 | 28 | `parse_tag` | src/bin/app/preview/mdhtml.rs:98 |
|x| 49 | 25 | `generate_locales` | src/build.rs:268 |
|x| 47 | 30 | `assemble` | src/ocr/table.rs:81 |
|x| 47 | 22 | `render` | src/decode/mesh.rs:327 |
|x| 47 | 17 | `ttf_wndproc` | src/bin/app/tags_to_folders.rs:45 |
|x| 47 | 24 | `capture_monitor` | src/bin/app/screenshot/hdr.rs:192 |
|x| 46 | 28 | `decode_reduced` | src/decode/jp2/mod.rs:105 |
|x| 46 | 22 | `nv12_frame_from_owned_bytes` | src/video.rs:247 |
|x| 46 | 28 | `parse_prologue` | src/container/xcf.rs:119 |
|x| 45 | 26 | `sample_location` | src/mp4.rs:470 |
|x| 45 | 21 | `walk` | src/bin/app/preview/dbdoc.rs:407 |
|x| 45 | 22 | `load` | src/bin/app/preview/loader.rs:20 |
|x| 44 | 20 | `parse_apev2_cover` | src/container/audio/ape.rs:45 |
|x| 44 | 27 | `read_info_verbose` | src/strip.rs:435 |
|x| 44 | 27 | `do_action` | src/bin/app/preview/window/command.rs:46 |
|x| 44 | 27 | `parse_palette` | src/decode/jp2/codestream.rs:170 |
|x| 44 | 27 | `decode_level` | src/container/xcf.rs:565 |
|x| 44 | 25 | `read_info_impl` | src/strip.rs:284 |
|x| 43 | 17 | `paint_lines` | src/bin/app/preview/highlight.rs:91 |
|x| 43 | 18 | `tiff_has_raw_ifd_marker` | src/streamsrc/rawsniff.rs:192 |
|x| 43 | 29 | `shot_paint` | src/bin/app/screenshot/overlay/paint.rs:50 |
|x| 42 | 22 | `settings_wndproc` | src/bin/app/convert.rs:844 |
|x| 42 | 27 | `decode_preview_with_raw_order` | src/decode.rs:1198 |
|x| 42 | 34 | `encode_via_magick` | src/decode/magick.rs:996 |
|x| 42 | 21 | `doc_append` | src/bin/app/preview/markdown/doc.rs:67 |
|x| 42 | 18 | `parse_columns` | src/bin/app/preview/dbdoc.rs:658 |
| | 41 | 39 | `main` | scripts/compare-renders.py:119 |
|x| 40 | 30 | `dispatch_tool` | src/mcp.rs:313 |
|x| 40 | 13 | `collect_leaf_pages` | src/container/clip.rs:183 |
|x| 39 | 24 | `run_capture_inner` | src/bin/app/screenshot/overlay.rs:424 |
|x| 38 | 25 | `display_name` | src/bin/app/preview/font.rs:52 |
|x| 38 | 23 | `start_convert` | src/bin/app/convert.rs:633 |
|x| 38 | 36 | `extract_cover` | src/container/mod.rs:291 |
|x| 38 | 22 | `convert_wndproc` | src/bin/app/convert.rs:1042 |
|x| 38 | 15 | `neutralize_lossless_jpeg_orientation` | src/verbs/encode.rs:273 |
|x| 38 | 21 | `parse_wav` | src/container/waveform.rs:106 |
|x| 37 | 14 | `isobmff_color_icc` | src/decode/color.rs:340 |
|x| 37 | 14 | `size_of` | src/mp4.rs:425 |
|x| 37 | 32 | `video_key` | src/bin/app/preview/window.rs:1532 |
|x| 37 | 20 | `first_run_wndproc` | src/bin/app/first_run.rs:516 |
|x| 37 | 24 | `to_sfnt` | src/bin/app/preview/woff.rs:39 |
|x| 37 | 18 | `load_static` | src/bin/app/preview/loader.rs:341 |
|x| 37 | 15 | `parse_iloc` | src/strip/isobmff.rs:174 |
|x| 37 | 28 | `parse_keyframe_header` | src/bin/vdec/vp9.rs:185 |
|x| 37 | 23 | `extract_best` | src/container/psp.rs:157 |
|x| 36 | 14 | `feed` | src/bin/app/preview/mdhtml.rs:17 |
|x| 36 | 15 | `cue_points` | src/mkv.rs:718 |
|x| 36 | 13 | `strip` | src/strip/svgmeta.rs:26 |
|x| 35 | 17 | `blocks_rgba8` | src/decode/dds.rs:759 |
|x| 35 | 23 | `eligible_bt601_still` | src/decode/avifmf.rs:162 |
|x| 35 | 21 | `fuzz_extract_cover` | src/container/mod.rs:1107 |
|x| 34 | 15 | `reference` | src/decode/jp2/dwt.rs:260 |
|x| 34 | 17 | `avif_wic_verdict` | src/decode/color.rs:441 |
|x| 34 | 20 | `scaled_pre_pass_sweep_by_format` | src/decode/tests.rs:2246 |
|x| 34 | 19 | `isobmff_associated_items` (was `associated_items`, hoisted out of `isobmff_has_hevc_aux_alpha`) | src/decode/color.rs:645 |
|x| 34 | 15 | `filtr_1d` | src/decode/jp2/dwt.rs:76 |
|x| 34 | 22 | `wndproc` | src/bin/app/prebuild_dlg.rs:238 |
|x| 33 | 18 | `tiff_ifd0_is_reduced` | src/streamsrc/rawsniff.rs:135 |
|x| 33 | 17 | `get_thumbnail_inner` | src/thumbprovider.rs:85 |
|x| 33 | 19 | `parse_aiff` | src/container/waveform.rs:164 |
|x| 33 | 15 | `mime_body` | src/bin/app/preview/mailmsg.rs:253 |
|x| 33 | 24 | `read_layer_head` | src/container/xcf.rs:440 |
|x| 33 | 27 | `segment_map` | src/mkv.rs:98 |
|x| 33 | 16 | `gif_prefers_wic` | src/decode.rs:814 |
|x| 33 | 27 | `extract` | src/container/max.rs:32 |
|x| 32 | 10 | `check_registration` | src/doctor.rs:312 |
|x| 32 | 14 | `spawn_decode` | src/bin/app/preview/content.rs:1024 |
|x| 32 | 20 | `window_under` | src/bin/app/screenshot/overlay/input.rs:56 |
|x| 32 | 19 | `scan_top_level` | src/mp4.rs:106 |
|x| 32 | 15 | `thumbnail_from_resources` | src/container/psd.rs:107 |
|x| 32 | 20 | `resolve_icon_path` | src/container/apk.rs:604 |
|x| 32 | 29 | `epsi_preview` | src/container/eps.rs:71 |
|x| 31 | 27 | `parse_siz` | src/decode/jp2/codestream.rs:486 |
|x| 31 | 12 | `xform_block` | src/jpegtran.rs:425 |
|x| 31 | 22 | `mp4_remux_moov` | src/streamsrc/mp4remux.rs:31 |
|x| 31 | 13 | `find_sqli` | src/container/clip.rs:77 |
|x| 30 | 20 | `parse_header` | src/decode/dds.rs:334 |
|x| 30 | 22 | `scrub_mouse_down` | src/bin/app/preview/transport.rs:282 |
|x| 30 | 24 | `encode_to_opts` | src/verbs/encode.rs:411 |
|x| 30 | 15 | `col_at` | src/bin/app/preview/highlight.rs:397 |
| | 30 | 13 | `apply_text_attr` | src/container/audio/asf.rs:261 |
|x| 30 | 15 | `encode_pam_streaming` | src/verbs/encode/streaming.rs:273 |
|x| 30 | 26 | `render` | src/pdf.rs:281 |
| | 30 | 14 | `strip_html` | src/bin/app/preview/mailmsg.rs:519 |
|x| 30 | 19 | `slice_walk` | src/flv.rs:268 |
|x| 30 | 19 | `url_at` | src/bin/app/preview/markdown/parse.rs:444 |
| | 0 | 38 | `dxgi_layout` | src/decode/dds.rs:438 |
| | 0 | 49 | `parse_blocks` | src/bin/app/preview/markdown/parse.rs:622 |
| | 0 | 38 | `glyph` | src/badge.rs:89 |
| | 0 | 33 | `decode_entities` | src/bin/app/preview/mdhtml.rs:365 |
| | 0 | 36 | `system_ui_code` | src/i18n.rs:109 |
| | 0 | 37 | `native_name` | src/i18n.rs:166 |
