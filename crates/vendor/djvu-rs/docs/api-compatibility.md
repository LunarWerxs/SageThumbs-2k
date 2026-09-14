# API compatibility, stability, and resource-limit policy

This document is the checked-in, enforceable contract for consumers of the
`djvu-rs` crate and its workspace codec crates (`djvu-iff`, `djvu-bzz`,
`djvu-jb2`, `djvu-iw44`, `djvu-zp`, `djvu-bitmap`, `djvu-pixmap`). It defines:

- what "stable", "experimental", and "deprecated" mean for this project;
- the semantic-versioning and deprecation-window rules;
- the minimum supported Rust version (MSRV) policy;
- the supported feature combinations and build targets
  (see also [`feature-matrix.md`](feature-matrix.md));
- the `Send`/`Sync` and thread-safety expectations of the public types;
- the panic-free contract for untrusted input;
- the resource-limit (memory / work / decompression) contract and how limit
  failures surface as typed errors.

It is enforced in CI. See [Enforcement](#enforcement) for the exact gates.

> **Pre-1.0 note.** The crate is currently `0.x`. Under Cargo's SemVer rules a
> `0.MINOR.PATCH` release treats **MINOR** as the breaking-change axis and
> **PATCH** as the compatible axis. Everything below is written so it reads the
> same way once the crate reaches `1.0`; until then, substitute "minor bump"
> for "major bump" wherever a breaking change is described.

## 1. Surface tiers

Every public item belongs to exactly one tier. The tier is declared in the
item's rustdoc and, where a whole area is involved, gated behind a Cargo
feature whose name signals the tier.

### Stable

The default surface. Stable items follow SemVer: a breaking change to their
signature, behavior, or error contract requires a major bump (pre-1.0: a minor
bump). This includes:

- `Document`, `Page`, and the crate-root re-exports of the document model
  (`DjVuDocument`, `DjVuPage`, `DjVuBookmark`, `PageInfo`, `Rotation`,
  `ComponentId`, `ComponentKind`, …).
- The error hierarchy (`DjVuError`, `IffError`, `Jb2Error`, `Iw44Error`,
  `BzzError`, `DocError`, `RenderError`) — see the error-stability rule below.
- The rendering entry points in [`djvu_render`] (`RenderOptions`,
  `render_pixmap`, `render_region`, `render_coarse`, `render_progressive`, …).
- The codec entry points of the workspace crates that the README advertises as
  `no_std`-callable: `iff::parse_form`, `bzz::bzz_decode`, `jb2::decode_dict`,
  `iw44::Iw44Image::decode_chunk`.
- The text / annotation / metadata parsers and their data models.
- The writer surfaces behind their feature gates (`pdf`, `epub`, `cbz`,
  `tiff`): once a `djvu_to_*` entry point ships, its signature is stable.

**Error stability rule.** Every public error *enum* carries
`#[non_exhaustive]`: consumers must not rely on the absence of variants, and
adding a new variant is a compatible change. Renaming or removing an existing
variant, or changing the data it carries, is breaking. Matching on a stable
variant and reading its documented fields is supported; a `match` on an error
enum must include a `_` arm.

This was a rule "in spirit" until 0.32.0, enforced only by review. It is now
literal, which is what lets the `Public API breakage` CI gate be a hard failure
instead of an advisory one: before, every intended new variant reported as
breakage, so the gate had to run with `continue-on-error` and could not block a
real, unintended break either. Any new public error enum must be marked the same
way.

### Experimental

Opt-in, may change or be removed in any release **including a patch release**,
and is excluded from the API-breakage gate. Experimental status is signalled
two ways, both of which must be present:

- the rustdoc opens with a bolded `**Experimental:**` note explaining the
  instability, and
- the surface is reachable only behind an experimental feature flag.

Current experimental features: `experimental`, `iw44-probe`, `alloc-profile`,
`ocr-onnx`, `wasm-threads`. Current experimental/placeholder items:
`ocr-neural` (`CandleBackend` — `load()` returns an unsupported error and there
is no stable model contract). These are **out of scope** for API freezing per
issue #695 and remain free to change.

### Deprecated

Still stable and still shipped, but scheduled for removal. A deprecated item:

- carries `#[deprecated(since = "x.y.z", note = "use … instead")]` on code
  items, or an explicit "Deprecated" marker in its rustdoc / the feature table
  for feature-level aliases;
- keeps working, unchanged, for the **deprecation window** below;
- names its replacement.

**Deprecation window.** A deprecated item remains available for at least **two
minor releases** after the release that introduced the deprecation, and never
less than **90 days**, whichever is longer. Removal happens only in a
breaking (major; pre-1.0 minor) release.

Current deprecated surfaces:

| Item | Kind | Replacement |
|------|------|-------------|
| `bzz_new` module | re-export alias | `bzz` |
| `iw44_new` module | re-export alias | `iw44` |
| `ocr-neural-candle` feature | no-op feature alias | `ocr-neural` |

These aliases are kept intentionally cheap (a `pub use` or a no-op feature) so
they can outlive the minimum window without cost.

## 2. Semantic versioning

- **Patch** (`0.y.Z`): bug fixes, performance work, new **stable** APIs that are
  purely additive, new error variants, new features that default to off.
- **Minor** (`0.Y.z`, pre-1.0 = breaking): removals of deprecated items after
  their window, signature/behavior changes to stable items, MSRV raises.
- Experimental surfaces are exempt: they may break in any release.

Unintended breakage of the **stable** surface is caught by
[`cargo-semver-checks`](#enforcement) in CI, which compares the PR against the
latest published version and understands the `0.x` breaking axis.

## 3. Minimum supported Rust version (MSRV)

- MSRV is **Rust 1.88** (edition 2024; let-chains). It is declared in
  `rust-version` in every workspace `Cargo.toml`.
- Raising the MSRV is a **minor** (pre-1.0 breaking) change and is called out in
  the changelog.
- MSRV is a **required** CI gate (the `MSRV (1.88)` job builds the crate on the
  pinned toolchain). A PR that uses a newer-than-MSRV language or std feature
  fails that gate.

## 4. Feature combinations and targets

The supported combinations and build targets, and the CI jobs that keep them
green, are enumerated in [`feature-matrix.md`](feature-matrix.md). The load-
bearing invariants:

- **Decode-only default tree.** The default (`std`) build must not pull in any
  writer/encoder dependency (zip, zopfli, jpeg-encoder, clap, …). Enforced by
  `scripts/check_feature_hygiene.sh` in the required `Lint` gate (#509).
- **`no_std` + `alloc`.** `--no-default-features` must build on the host and on
  `wasm32-unknown-unknown`, and the four codec entry points must be callable
  from a `#![no_std]` consumer (`tests/no_std_smoke`). Required via the
  `Test (stable)` and `wasm32 build check` gates.
- **wasm.** `wasm`, `wasm-lazy`, and `+simd128` all build for
  `wasm32-unknown-unknown`. Required via the `wasm32 build check` gate.
- **Additivity.** Enabling any documented feature must not break another
  feature that already built. Any documented combination must compile.

`wasm-threads` is the one documented target deliberately **outside** the
required gate: it needs a nightly toolchain and `-Z build-std`, so it is checked
by the opt-in `make wasm-threads-check` only.

## 5. `Send` / `Sync` and thread-safety

The public types' thread-safety is part of the contract and asserted at compile
time in [`tests/send_sync_contract.rs`](../tests/send_sync_contract.rs); a
regression there is a breaking change and fails CI.

| Type | `Send` | `Sync` | Notes |
|------|:------:|:------:|-------|
| `Document` | ✓ | ✓ | Owns an `Arc<dyn AsRef<[u8]> + Send + Sync>` backing; safe to share across threads. |
| `DjVuDocument` | ✓ | ✓ | The document model; shared-dictionary caches use `Arc<SharedDict>` with interior sync. |
| `Page<'a>` / `DjVuPage` | ✓ | ✓ | Borrow of the parent document; carries no interior mutability observable to callers. |
| `DjVuDocumentMut` / `PageMut<'doc>` | ✓ | — by borrow | The mutable editor is `Send`; a `PageMut` borrows `&mut`, so exclusivity (not `Sync`) is how concurrent edits are prevented. |
| `Pixmap` / `GrayPixmap` / `Bitmap` | ✓ | ✓ | Plain owned pixel buffers. |
| `RenderOptions`, `TextLayer`, `Annotation`, metadata types | ✓ | ✓ | Plain data. |
| `LazyDocument<R>` (async) | ✓/✓ **iff** `R: Send`/`Sync` | conditional | Thread-safety is inherited from the caller-supplied reader `R`. |

**Async guidance.** CPU-bound IW44/JB2 decode and render is synchronous and
must run on a blocking pool (`tokio::task::spawn_blocking`), not on a runtime
thread. `Document`/`DjVuDocument` being `Send + Sync` means a single parsed
document can be shared (e.g. behind an `Arc`) across render tasks.

## 6. Panic-free contract for untrusted input

**Contract.** No public parse, decode, or render entry point may panic, abort,
or overflow on *any* byte input, trusted or not. Malformed input must surface as
a typed `Err`, never an unwind. This covers `DjVuDocument::parse` /
`parse_from_dir`, `Document::open` / `from_bytes` / `from_reader`, every
`DjVuPage` accessor (`thumbnail`, `text_layer`, `annotations`, `extract_mask`),
every `djvu_render::*` entry point, and the standalone codec entry points.

**Tests.** The contract is exercised by:

- [`tests/panic_free_corpus.rs`](../tests/panic_free_corpus.rs) — runs every
  public decode/render entry point across the whole corpus and a set of
  adversarial byte patterns on every PR; any unwind fails the run.
- `tests/proptest_codecs.rs` — property tests over the codec surface.
- `fuzz/fuzz_targets/*` (libFuzzer) + OSS-Fuzz — continuous adversarial
  coverage of IFF/BZZ/JB2/IW44/G4, encode, metadata, validate, and the full
  parse→render pipeline (`fuzz_full`).

The FFI/WASM boundaries additionally catch unwinds and convert them to error
codes, so a hostile file can never unwind across the language boundary.

## 7. Resource-limit contract

Public decode/render operations **inherit** documented, bounded resource
ceilings: every allocation or loop whose size is driven by untrusted bytes is
capped against an explicit constant or the input length, so a crafted file
costs only bounded memory and work before erroring out. The authoritative table
of ceilings lives in [`../SECURITY.md`](../SECURITY.md#decode-time-resource-ceilings)
(the #589 inventory) and is summarized here:

| Axis | Bound | Constant |
|------|-------|----------|
| JB2 symbol pixels (per symbol / per stream) | 16 MP / 256 MP | `MAX_SYMBOL_PIXELS` / `MAX_TOTAL_SYMBOL_PIXELS` |
| JB2 page / blit pixels | 16 MP / 256 MP | `MAX_PAGE_SYMBOL_PIXELS` / `MAX_TOTAL_BLIT_PIXELS` |
| JB2 record count | 65 536 | `MAX_RECORDS` |
| IW44 declared image | 64 MP | checked in `decode_chunk` |
| BZZ block / total output | 4 MiB / 256 MB | `MAX_BLOCK_SIZE` / `MAX_OUTPUT_SIZE` |
| IFF / FORM nesting depth | 64 | `MAX_IFF_DEPTH` |
| NAVM bookmark depth | 256 | `MAX_NAVM_DEPTH` |
| S-expression (ANTz) depth | 64 | `MAX_SEXPR_DEPTH` |
| Text-zone (TXTz) depth | 64 | `MAX_ZONE_DEPTH` |
| Pixmap pixels | 64 MP | `MAX_PIXELS` |
| Render output pixels | 512 MP | [`DEFAULT_MAX_RENDER_PIXELS`](../../src/validate.rs) |

**Configurable budgets.** Callers may supply a [`ResourceLimits`](../../src/validate.rs)
budget via [`ParseOptions::limits`](../../src/validate.rs) on
[`DjVuDocument::parse_with_options`](../../src/djvu_document.rs) /
[`Document::from_bytes_with_options`](../../src/lib.rs), or via
[`render_pixmap_with_limits`](../../src/djvu_render.rs) on render entry points.
Unset fields mean “no limit on this axis”. When render calls omit an explicit
override, the document inherits limits stored at parse time. When both are
unset, render output inherits [`DEFAULT_MAX_RENDER_PIXELS`]. Use
[`ResourceLimits::inherited`] for the documented default render ceiling only.
Per-render tightening uses [`render_pixmap_with_limits`](../../src/djvu_render.rs).
The validator and `djvu validate --limits` use the same type.

**Limit failures are typed and identify the operation.** When a ceiling is hit,
the failing entry point returns a typed error naming the axis and operation:

| Limit | Typed error | Wrapped as |
|-------|-------------|------------|
| IFF nesting too deep | `IffError::DepthLimitExceeded { max }` | `DjVuError::Iff` |
| IFF chunk longer than its container | `IffError::ChunkTooLong { id, claimed, available }` | `DjVuError::Iff` |
| JB2 image / symbol / dict too large | `Jb2Error::ImageTooLarge` / `InheritedDictTooLarge` | `DjVuError::Jb2` |
| JB2 too many records | `Jb2Error::TooManyRecords` | `DjVuError::Jb2` |
| IW44 image too large | `Iw44Error::ImageTooLarge` | `DjVuError::Iw44` |
| BZZ block / output too large | `BzzError::BlockSizeTooLarge(_)` / `OutputTooLarge` | `DjVuError::Bzz` |
| Configured document/render budget exceeded | [`ResourceLimitExceeded`](../../src/validate.rs) | [`DocError::ResourceLimit`](../../src/djvu_document.rs) / [`RenderError::ResourceLimit`](../../src/djvu_render.rs) |

The `DjVuError` variant (`Iff` / `Jb2` / `Iw44` / `Bzz`) identifies which
decode stage rejected the input; the inner variant identifies the axis.
[`ResourceLimitExceeded::operation`] names the public entry point (for example
`document.parse` or `render_pixmap`). `IffError::ChunkTooLong` additionally
reports the offending chunk `id`.

## 8. Experimental-surface consistency (README ↔ docs.rs)

The set of experimental/placeholder/deprecated surfaces is stated identically
in three places, and they must not drift:

- this document (§1),
- the **Feature flags** and **Status & limitations** tables in the README, and
- the rustdoc of each item (rendered on docs.rs, built with `--all-features` and
  `--cfg docsrs`).

## Enforcement

| Guarantee (AC) | Gate |
|----------------|------|
| Stable API breakage detection | `cargo-semver-checks` (`.github/workflows/api-stability.yml`) |
| Feature combinations / targets | feature-matrix job (`api-stability.yml`) + `scripts/check_feature_hygiene.sh` (`Lint`) |
| MSRV | `MSRV (1.88)` job (required) |
| `no_std` builds | `Test (stable)` `Build (no_std check)` + `wasm32 build check` (both required) |
| Panic-free contract | `tests/panic_free_corpus.rs`, proptests, fuzz/OSS-Fuzz |
| `Send`/`Sync` contract | `tests/send_sync_contract.rs` |
| Resource limits | codec unit tests + fuzz; ceilings documented here and in `SECURITY.md` |

Run the deterministic subset locally with `make check` before pushing.
