# Tile-first rendering contract

Issue [#691](https://github.com/matyushkin/djvu-rs/issues/691) tracks a
tile-oriented rendering API for viewer engines. This document is the formal
contract for the parts that have landed; the API lives in
[`src/djvu_tile.rs`](../src/djvu_tile.rs) (`djvu_rs::djvu_tile`).

## Slice 1: tile grid, coordinate space, deterministic tiles

### Coordinate space

Tiles live in **display space**: the pixel space of the final rendered image
*after* the combined rotation (INFO-chunk rotation + `RenderOptions::rotation`)
has been applied — exactly the pixels `render_pixmap` returns. The origin is
the top-left corner of the rotated page image, `x` grows right, `y` grows
down. Callers never deal with pre-rotation coordinates;
`TileLayout` performs the pull-back to the region renderer's pre-rotation
`RenderRect` internally (mapping table in the `to_render_rect` doc comment).

### Scale

Scale is expressed the same way as everywhere else in the render API: via
`RenderOptions::width`/`height` (build them with `fit_to_width`/`fit_to_box`).
`TileLayout::output_width()`/`output_height()` give the display canvas —
equal to `opts.width × opts.height` for identity/180° combined rotation,
swapped for 90°/270°.

### Grid and edge behavior

For tile size `ts`, the grid has `ceil(W / ts)` columns and `ceil(H / ts)`
rows over the `W × H` display canvas. Tile `(col, row)` covers

```
x ∈ [col·ts, min((col+1)·ts, W))    y ∈ [row·ts, min((row+1)·ts, H))
```

Edge tiles are **clipped** to the canvas — a tile pixmap never contains
padding pixels, so blitting tiles at their `tile_rect` offsets tiles the
canvas exactly once.

### Determinism guarantees (tested)

- **Assembly parity.** Blitting every tile at its display rectangle
  reproduces `render_pixmap` output **byte-for-byte** — including scaled
  renders, all four combined rotations, and the 1/4-resolution mask fast
  path taken at background subsample ≥ 4
  (`assembled_tiles_match_full_render_*` tests).
- **Order independence.** A tile's pixels are a pure function of
  `(page bytes, opts, tile_size, col, row)`. Request order never changes a
  byte — for the cached entry point this holds across cold misses, warm
  hits, and any interleaving (`tile_pixels_independent_of_request_order`).
- **Cache transparency.** `render_tile_cached` is byte-identical to
  `render_tile`; it routes through `render_region_tiled`, a memoization of
  the same compositor (see C4_TILE_CACHE notes there).

### Layer selection

Tiles composite the same layer stack as `render_pixmap` (mask + foreground +
background). Selecting individual layers (mask-only, background-only) is a
later slice of #691.

### Rejected options

`TileLayout::new` returns `RenderError::UnsupportedOption` for:

- `Resampling::Lanczos3` — a windowed whole-image resampling post-pass; its
  kernel windows straddle tile boundaries, so per-tile assembly cannot be
  byte-identical. Follow-up in #691.
- `aa: true` — a post-pass that *halves* the output; the tile grid would no
  longer match the produced pixels. Request the target size via
  `opts.width`/`height` instead.

`permissive: true` is allowed and inherits the region renderer's recovery
semantics (the cached path falls back to uncached rendering in that mode).

### Resource limits

Each tile render is bounded by the same `max_render_pixels` check as any
region render (limits inherited from the parent document at parse time).

## Slice 2: cache budget, invalidation, prefetch

Slice 2 makes the composited-tile cache behind `render_tile_cached` a
controllable resource. All of it is `std`-only; prefetch additionally needs
the `parallel` feature. **Cache state never changes rendered bytes — only
latency**; every entry point below preserves slice 1's determinism
guarantees.

### Usage and budget

- `tile_cache_usage(page)` returns `TileCacheUsage { bytes, budget, tiles }`
  for the page's cache. `tiles` counts *internal* 256-px composited tiles
  (the cache's granularity), not caller-grid tiles. Reading usage never
  decodes or renders.
- `set_tile_cache_budget(page, max_bytes)` overrides the per-page byte
  budget (default 8 MiB), evicting oldest-first down to the new bound
  immediately. Budget `0` disables composited-tile caching for the page.
  The override survives a document-level cache *downgrade* but resets to the
  default when the page's whole render cache is dropped (it lives with the
  cache it bounds).

### Invalidation

- `clear_tile_cache(page)` drops every cached tile, returning bytes freed.
  Decoded layers stay warm; only memoized compositor output is dropped.
- `invalidate_tile_region(page, opts, region)` drops exactly the cached
  tiles intersecting a **display-space** rectangle (same space as
  `TileLayout::tile_rect`, clipped to the canvas) and returns bytes freed.
  The region is pulled back through the combined rotation and then mapped
  proportionally into **every** cached render size, rounding outward — a
  tile touching the region at any scale is dropped, never kept.

### Prefetch

- `prefetch_tiles(doc, page_index, opts, tile_size, col, row, radius)`
  schedules background composition of all grid tiles within Chebyshev
  distance `radius` of the center tile (at most `(2·radius + 1)²`, clipped
  to the grid) on the shared rayon pool, returning the scheduled count. It
  warms the same cache `render_tile_cached` reads — no separate buffer, no
  race: whichever side composites a tile first, the other observes it. It is
  a hint: out-of-range pages are a no-op, background errors are swallowed
  (a foreground render surfaces them), and retained bytes stay bounded by
  the page's budget.

Tests: `cache_usage_budget_and_clear`,
`invalidate_region_drops_overlapping_tiles_across_scales`,
`invalidate_maps_display_rect_through_rotation`, `prefetch_tiles_warms_cache`.

## Slice 3: progressive quality steps and cancellation

One entry point carries both:
`render_tile_with(page, opts, tile_size, col, row, &TileRenderControls)`.
Default controls reproduce `render_tile` byte-for-byte; `use_cache: true`
reproduces `render_tile_cached`.

### Quality steps

- `quality_step: Some(k)` composites the tile from BG44 background chunks
  `0..=k` only (full foreground and mask), for
  `k < progressive_steps(page)`. The tile is **byte-identical** to the
  matching crop of the full-page progressive frame
  `render_progressive_step(page, opts, k)`, under every rotation.
- **Never-regress by construction.** Each BG44 chunk is a wavelet
  refinement over the same base image: step `k+1` adds detail to step `k`,
  never replaces or coarsens it. Walking `0..progressive_steps` is the same
  monotonic ladder full-page progressive rendering rides; the tile API adds
  no resampling or re-quantization that could break it.
- On pages without BG44 data the ladder has exactly one step: `Some(0)` is
  the full render (mirroring `render_progressive_step`), larger steps error
  with `ChunkOutOfRange`.
- Partial-quality pixels are decoded per call and **never** enter the
  composited-tile cache, so no later full-quality render can observe them.

### Cancellation

- `TileCancelToken` is a shared one-way flag: clones observe one another,
  `cancel()` is sticky. Pass a clone in `TileRenderControls::cancel`, or to
  `prefetch_tiles_cancellable` (same schedule count as `prefetch_tiles`;
  cancellation bounds how much of the schedule runs).
- Checkpoints sit **between** units of work: before each tile, before each
  internal 256-px cache tile, and between layer decode and composite. An
  in-flight unit always completes; cancelled calls return
  `TileError::Cancelled`.
- Cancellation never changes bytes and never corrupts caches: a cache tile
  is inserted only after its composite finished, so an abandoned call
  leaves either complete tiles or nothing.

Tests: `progressive_tiles_match_progressive_frames`,
`progressive_tiles_match_under_rotation`,
`render_tile_with_matches_dedicated_entry_points`,
`quality_steps_on_bilevel_page`, `cancelled_token_aborts_every_mode`,
`prefetch_tiles_cancellable_behaviour`.

## Slice 3b: async and WASM surfaces

Thin adapters over `render_tile_with` — every byte guarantee above carries
over unchanged; neither surface adds rendering logic of its own.

### Async (`djvu_async`, feature `async`)

- `render_tile_async(page, opts, tile_size, col, row, controls)` runs the
  sync entry point inside `tokio::task::spawn_blocking`. Cancel from any
  task via a clone of the token in `controls.cancel`; the render stops at
  its next checkpoint with `TileError::Cancelled` (wrapped in
  `AsyncTileError::Tile`).
- `render_tile_progressive_stream(page, opts, tile_size, col, row, cancel)`
  is the tile-granular counterpart of `render_progressive_stream`: one
  frame per quality step, coarsest first, each byte-identical to the
  matching crop of the full-page progressive frame. On cancellation the
  stream yields one `Cancelled` error and ends (the token is sticky).

Tests: `tile_async_matches_sync_tile`,
`tile_progressive_stream_frames_match_quality_steps`,
`tile_cancellation_surfaces_and_ends_stream`.

### WASM (`wasm`, feature `wasm`)

`WasmPage` gains `tile_cols`/`tile_rows` (grid dimensions; tile `(col, row)`
starts at canvas pixel `(col·tile_size, row·tile_size)`),
`render_tile` (full quality, composited-tile cache, returns a `WasmPixmap`
carrying the clipped tile dimensions), `render_tile_progressive`
(quality step `chunk_n`, matching `render_progressive`'s numbering, never
cached), and `render_tile_into_pixmap` (buffer-reuse variant, #611
pattern). Cancellation tokens are not exposed: wasm renders are
synchronous calls that JS cannot interrupt mid-call. Parity is enforced by
the sync tile tests the bindings delegate to.

## Renderer fix that fell out of slice 1

`render_region`/`render_region_tiled` previously always composited against
the full-resolution JB2 mask, while `render_into`/`render_rows` switch to a
pre-downsampled 1/4-resolution mask at background subsample ≥ 4 (no bold, no
FGbz). Region renders of heavily downscaled pages therefore diverged from
the matching crop of the full render. Both region entry points now take the
same `resolve_sub4_mask` decision (`render_region_matches_full_render_crop_at_sub4`).

## Renderer fix that fell out of slice 3

`render_progressive` frames used to depend on cache warmth: when a prior
full render at the same strong downscale had retained the 1/4-resolution
mask (#607 fast path), `decode_layers` handed the progressive path a
maskless layer set and the text layer silently vanished from every later
progressive frame. The fast path is now restricted to full-background
decodes, which the non-progressive paths resolve through
`resolve_sub4_mask` (`render_progressive_ignores_mask_sub4_warmth`).

## Planned follow-ups (#691)

- Layer selection (mask/foreground/background) per tile request.
- Lanczos-3 tiles (needs kernel-window-aware tile aprons).
