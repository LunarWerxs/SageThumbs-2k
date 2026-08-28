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

Do not attempt casually: the three giant Win32 wndproc dispatchers below (cognitive
579/480/402) are large, stateful, side-effect-heavy message loops. Splitting them safely
needs a dedicated session with manual UI verification (exercising the actual window,
not just `cargo test`), not a mechanical extract-and-move pass. Treat them as their own
project.

- `wndproc` src/bin/app/preview/window.rs:709 (cognitive 579, CC 205)
- `shot_wndproc` src/bin/app/screenshot/overlay/input.rs:128 (cognitive 480, CC 124)
- `wndproc` src/bin/app/settings_dlg/mod.rs:1041 (cognitive 402, CC 146)

## Queue

Ranked worst first (cognitive complexity, then cyclomatic complexity). Checked rows were
cleared by a burndown tranche; see git log for the commits.

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

| done | cog | CC | function | location |
|---|---|---|---|---|
| | 579 | 205 | `wndproc` | src/bin/app/preview/window.rs:709 |
| | 480 | 124 | `shot_wndproc` | src/bin/app/screenshot/overlay/input.rs:128 |
| | 402 | 146 | `wndproc` | src/bin/app/settings_dlg/mod.rs:1041 |
| | 208 | 77 | `main` | src/bin/app/main.rs:195 |
|x| 207 | 86 | `transform` | src/jpegtran.rs:461 |
|x| 203 | 73 | `decode_tile` | src/decode/jp2/mod.rs:257 |
|x| 182 | 46 | `decode_code_block` | src/decode/jp2/mq.rs:275 |
|x| 170 | 67 | `read_streams` | src/container/ole.rs:71 |
|x| 158 | 69 | `isobmff_has_hevc_aux_alpha` | src/decode/color.rs:600 |
|x| 153 | 55 | `parse` | src/decode/jp2/codestream.rs:316 |
| | 120 | 42 | `run_block` | src/bin/app/preview/markdown/inline.rs:192 |
| | 116 | 77 | `handle_key` | src/bin/app/screenshot/overlay/input.rs:620 |
|x| 114 | 49 | `parse_ply` | src/decode/mesh.rs:197 |
|x| 113 | 59 | `extract` | src/container/ilbm.rs:42 |
| | 111 | 43 | `about_wndproc` | src/bin/app/about.rs:798 |
| | 109 | 64 | `parse_sps` | src/flv.rs:552 |
| | 108 | 36 | `apply_v3_layout` | src/bin/app/settings_dlg/navrail.rs:875 |
| | 107 | 36 | `jpeg_span` | src/container/util.rs:143 |
| | 106 | 29 | `decode_rle` | src/container/xcf.rs:756 |
| | 93 | 48 | `run_action` | src/verbs/actions.rs:333 |
| | 92 | 52 | `run` | src/bin/cli.rs:140 |
| | 92 | 37 | `keyframe_mini_mp4` | src/flv.rs:56 |
| | 88 | 32 | `QueryContextMenu` | src/contextmenu/com.rs:52 |
| | 84 | 23 | `parse_packet` | src/decode/jp2/packet.rs:217 |
| | 76 | 32 | `decode_channels` | src/container/psp.rs:244 |
| | 76 | 43 | `daemon_wndproc` | src/bin/app/screenshot/daemon.rs:456 |
| | 75 | 42 | `decode_any_with_wic_target` | src/decode.rs:292 |
| | 74 | 47 | `dispatch` | src/bin/app/preview/mdhtml.rs:217 |
| | 72 | 37 | `recognize` | src/ocr.rs:130 |
| | 69 | 37 | `stream_source_with_caps` | src/streamsrc.rs:105 |
| | 68 | 33 | `render` | src/bin/app/preview/markdown.rs:265 |
| | 68 | 40 | `apply_settings` | src/bin/app/settings_dlg/values.rs:414 |
| | 66 | 30 | `stamp` | src/badge.rs:161 |
| | 66 | 38 | `run_shot` | src/bin/app/preview/shot.rs:13 |
| | 65 | 30 | `create` | src/bin/app/preview/webview.rs:43 |
| | 64 | 28 | `paint_into` | src/bin/app/preview/paint.rs:141 |
| | 64 | 28 | `tokenize` | src/bin/app/preview/highlight/lex.rs:199 |
| | 63 | 20 | `load_sync` | src/bin/app/preview/loader.rs:229 |
| | 62 | 27 | `draw_table` | src/bin/app/preview/markdown/tables.rs:14 |
| | 62 | 29 | `list_subclass` | src/bin/app/settings_dlg/list.rs:429 |
| | 61 | 28 | `strip_metadata` | src/strip.rs:95 |
| | 59 | 25 | `extract` | src/container/blend.rs:16 |
| | 58 | 22 | `ensure_visible` | src/bin/app/preview/selection.rs:323 |
| | 57 | 33 | `type_chunk_value` | src/container/apk.rs:668 |
| | 57 | 27 | `extract` | src/container/mobi.rs:10 |
| | 56 | 26 | `delimited_table` | src/bin/app/preview/docconv.rs:72 |
| | 55 | 23 | `popup_wndproc` | src/bin/app/settings_dlg/menuitems.rs:110 |
| | 54 | 18 | `largest_embedded_jpeg` | src/decode/tiers.rs:162 |
| | 53 | 23 | `jpeg_sof_dims` | src/container/util.rs:214 |
| | 53 | 18 | `filtr_1d_matches_the_original_three_buffer_algorithm` | src/decode/jp2/dwt.rs:259 |
| | 52 | 31 | `decode_scaled` | src/decode/exrscale.rs:73 |
| | 51 | 17 | `lib_side_translation_keys_all_survive_the_dll_subset` | src/i18n.rs:229 |
|x| 51 | 18 | `parse_obj` | src/decode/mesh.rs:150 |
| | 51 | 23 | `eyedropper_wndproc` | src/bin/app/eyedropper.rs:376 |
| | 50 | 15 | `cluster_keyframe` | src/mkv.rs:336 |
| | 50 | 28 | `parse_tag` | src/bin/app/preview/mdhtml.rs:98 |
| | 49 | 25 | `generate_locales` | src/build.rs:268 |
| | 47 | 30 | `assemble` | src/ocr/table.rs:81 |
|x| 47 | 22 | `render` | src/decode/mesh.rs:327 |
| | 47 | 17 | `ttf_wndproc` | src/bin/app/tags_to_folders.rs:45 |
| | 47 | 24 | `capture_monitor` | src/bin/app/screenshot/hdr.rs:192 |
|x| 46 | 28 | `decode_reduced` | src/decode/jp2/mod.rs:105 |
| | 46 | 22 | `nv12_frame_from_owned_bytes` | src/video.rs:247 |
| | 46 | 28 | `parse_prologue` | src/container/xcf.rs:119 |
| | 45 | 26 | `sample_location` | src/mp4.rs:470 |
| | 45 | 21 | `walk` | src/bin/app/preview/dbdoc.rs:407 |
| | 45 | 22 | `load` | src/bin/app/preview/loader.rs:20 |
| | 44 | 20 | `parse_apev2_cover` | src/container/audio/ape.rs:45 |
| | 44 | 27 | `read_info_verbose` | src/strip.rs:435 |
| | 44 | 27 | `do_action` | src/bin/app/preview/window/command.rs:46 |
|x| 44 | 27 | `parse_palette` | src/decode/jp2/codestream.rs:170 |
| | 44 | 27 | `decode_level` | src/container/xcf.rs:565 |
| | 44 | 25 | `read_info_impl` | src/strip.rs:284 |
| | 43 | 17 | `paint_lines` | src/bin/app/preview/highlight.rs:91 |
| | 43 | 18 | `tiff_has_raw_ifd_marker` | src/streamsrc/rawsniff.rs:192 |
| | 43 | 29 | `shot_paint` | src/bin/app/screenshot/overlay/paint.rs:50 |
| | 42 | 22 | `settings_wndproc` | src/bin/app/convert.rs:844 |
| | 42 | 27 | `decode_preview_with_raw_order` | src/decode.rs:1198 |
| | 42 | 34 | `encode_via_magick` | src/decode/magick.rs:996 |
| | 42 | 21 | `doc_append` | src/bin/app/preview/markdown/doc.rs:67 |
| | 42 | 18 | `parse_columns` | src/bin/app/preview/dbdoc.rs:658 |
| | 41 | 39 | `main` | scripts/compare-renders.py:119 |
| | 40 | 30 | `dispatch_tool` | src/mcp.rs:313 |
| | 40 | 13 | `collect_leaf_pages` | src/container/clip.rs:183 |
| | 39 | 24 | `run_capture_inner` | src/bin/app/screenshot/overlay.rs:424 |
| | 38 | 25 | `display_name` | src/bin/app/preview/font.rs:52 |
| | 38 | 23 | `start_convert` | src/bin/app/convert.rs:633 |
| | 38 | 36 | `extract_cover` | src/container/mod.rs:291 |
| | 38 | 22 | `convert_wndproc` | src/bin/app/convert.rs:1042 |
| | 38 | 15 | `neutralize_lossless_jpeg_orientation` | src/verbs/encode.rs:273 |
| | 38 | 21 | `parse_wav` | src/container/waveform.rs:106 |
|x| 37 | 14 | `isobmff_color_icc` | src/decode/color.rs:340 |
| | 37 | 14 | `size_of` | src/mp4.rs:425 |
| | 37 | 32 | `video_key` | src/bin/app/preview/window.rs:1532 |
| | 37 | 20 | `first_run_wndproc` | src/bin/app/first_run.rs:516 |
| | 37 | 24 | `to_sfnt` | src/bin/app/preview/woff.rs:39 |
| | 37 | 18 | `load_static` | src/bin/app/preview/loader.rs:341 |
| | 37 | 15 | `parse_iloc` | src/strip/isobmff.rs:174 |
| | 37 | 28 | `parse_keyframe_header` | src/bin/vdec/vp9.rs:185 |
| | 37 | 23 | `extract_best` | src/container/psp.rs:157 |
| | 36 | 14 | `feed` | src/bin/app/preview/mdhtml.rs:17 |
| | 36 | 15 | `cue_points` | src/mkv.rs:718 |
| | 36 | 13 | `strip` | src/strip/svgmeta.rs:26 |
| | 35 | 17 | `blocks_rgba8` | src/decode/dds.rs:759 |
| | 35 | 23 | `eligible_bt601_still` | src/decode/avifmf.rs:162 |
| | 35 | 21 | `fuzz_extract_cover` | src/container/mod.rs:1107 |
| | 34 | 15 | `reference` | src/decode/jp2/dwt.rs:260 |
|x| 34 | 17 | `avif_wic_verdict` | src/decode/color.rs:441 |
| | 34 | 20 | `scaled_pre_pass_sweep_by_format` | src/decode/tests.rs:2246 |
|x| 34 | 19 | `isobmff_associated_items` (was `associated_items`, hoisted out of `isobmff_has_hevc_aux_alpha`) | src/decode/color.rs:645 |
| | 34 | 15 | `filtr_1d` | src/decode/jp2/dwt.rs:76 |
| | 34 | 22 | `wndproc` | src/bin/app/prebuild_dlg.rs:238 |
| | 33 | 18 | `tiff_ifd0_is_reduced` | src/streamsrc/rawsniff.rs:135 |
| | 33 | 17 | `get_thumbnail_inner` | src/thumbprovider.rs:85 |
| | 33 | 19 | `parse_aiff` | src/container/waveform.rs:164 |
| | 33 | 15 | `mime_body` | src/bin/app/preview/mailmsg.rs:253 |
| | 33 | 24 | `read_layer_head` | src/container/xcf.rs:440 |
| | 33 | 27 | `segment_map` | src/mkv.rs:98 |
| | 33 | 16 | `gif_prefers_wic` | src/decode.rs:814 |
| | 33 | 27 | `extract` | src/container/max.rs:32 |
| | 32 | 10 | `check_registration` | src/doctor.rs:312 |
| | 32 | 14 | `spawn_decode` | src/bin/app/preview/content.rs:1024 |
| | 32 | 20 | `window_under` | src/bin/app/screenshot/overlay/input.rs:56 |
| | 32 | 19 | `scan_top_level` | src/mp4.rs:106 |
| | 32 | 15 | `thumbnail_from_resources` | src/container/psd.rs:107 |
| | 32 | 20 | `resolve_icon_path` | src/container/apk.rs:604 |
| | 32 | 29 | `epsi_preview` | src/container/eps.rs:71 |
|x| 31 | 27 | `parse_siz` | src/decode/jp2/codestream.rs:486 |
|x| 31 | 12 | `xform_block` | src/jpegtran.rs:425 |
| | 31 | 22 | `mp4_remux_moov` | src/streamsrc/mp4remux.rs:31 |
| | 31 | 13 | `find_sqli` | src/container/clip.rs:77 |
| | 30 | 20 | `parse_header` | src/decode/dds.rs:334 |
| | 30 | 22 | `scrub_mouse_down` | src/bin/app/preview/transport.rs:282 |
| | 30 | 24 | `encode_to_opts` | src/verbs/encode.rs:411 |
| | 30 | 15 | `col_at` | src/bin/app/preview/highlight.rs:397 |
| | 30 | 13 | `apply_text_attr` | src/container/audio/asf.rs:261 |
| | 30 | 15 | `encode_pam_streaming` | src/verbs/encode/streaming.rs:273 |
| | 30 | 26 | `render` | src/pdf.rs:281 |
| | 30 | 14 | `strip_html` | src/bin/app/preview/mailmsg.rs:519 |
| | 30 | 19 | `slice_walk` | src/flv.rs:268 |
| | 30 | 19 | `url_at` | src/bin/app/preview/markdown/parse.rs:444 |
| | 0 | 38 | `dxgi_layout` | src/decode/dds.rs:438 |
| | 0 | 49 | `parse_blocks` | src/bin/app/preview/markdown/parse.rs:622 |
| | 0 | 38 | `glyph` | src/badge.rs:89 |
| | 0 | 33 | `decode_entities` | src/bin/app/preview/mdhtml.rs:365 |
| | 0 | 36 | `system_ui_code` | src/i18n.rs:109 |
| | 0 | 37 | `native_name` | src/i18n.rs:166 |
