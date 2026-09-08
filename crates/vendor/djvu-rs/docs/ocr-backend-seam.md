# OCR Backend Seam Decision

Issue: #382

Date: 2026-06-17

## Context

`OcrBackend` (`src/ocr.rs`) is a one-method trait — `recognize(&self, pixmap,
&OcrOptions) -> Result<TextLayer, OcrError>` — with three implementors:

- `TesseractBackend` (`src/ocr_tesseract.rs`) — fully supported. Honours both
  `OcrOptions` fields (`languages` → `Tesseract::new`, `dpi` →
  `set_source_resolution`) and emits the full `page → line → word` zone tree.
- `OnnxBackend` (`src/ocr_onnx.rs`) — a real, working library-level CTC
  recognizer built on `tract`, but it ignores `OcrOptions` (a loaded ONNX graph
  has a baked-in input size and vocabulary; `languages`/`dpi` have no effect) and
  is **not** exposed as a CLI backend: no stable model family, preprocessing
  contract, or fixture is guaranteed.
- `CandleBackend` (`src/ocr_neural.rs`) — an explicit tombstone. `load()` and
  `recognize()` both return a clear error. It exists to make "neural OCR is not
  available yet" a typed, discoverable answer rather than a missing symbol.

An architecture-review pass (deepening-opportunities) flagged this as a possible
*hypothetical seam* — "one real adapter ⇒ no seam to maintain" — and asked
whether the trait should be collapsed, deepened, or deliberately retained.

Two facts decide the question:

1. **The seam has real consumers.** `src/bin/djvu.rs` exposes a CLI
   `--backend tesseract|onnx|candle` selector (`OcrBackendChoice`), and
   `build_ocr_backend` returns a `Box<dyn OcrBackend>` that `cmd_ocr` drives
   polymorphically (`ocr_backend.recognize(&pixmap, &options)`). The trait is the
   runtime dispatch point for that flag, not an unused abstraction. Collapsing it
   would delete a user-facing selector and its dispatch.
2. **The shape is intentional.** Every module carries docs distinguishing
   "supported" (Tesseract) from "experimental library-only" (ONNX) from
   "placeholder" (Candle); the CLI arms return individually-worded errors for the
   two unfinished backends; the README documents all three states. This is
   deliberate forward-shaping for backends in flight, not accidental generality.

## Decision

**Retain the `OcrBackend` trait.** It is the runtime seam behind the CLI
`--backend` selector and the `Box<dyn OcrBackend>` dispatch in `cmd_ocr`. It is
deliberate forward-shaping, not a speculative abstraction, and should not be
collapsed.

**Keep `OcrOptions { languages, dpi }` as advisory hints, documented as
best-effort.** A backend may honour or ignore any field; Tesseract honours both,
ONNX honours neither. The fields are not "Tesseract vocabulary" so much as the
two parameters any image-OCR run plausibly needs — input resolution and target
language(s) — and `languages` is meaningful to multilingual neural models too. A
model-neutral recast (e.g. splitting engine-neutral fields from an
engine-specific `params` bag) is deferred until a **second CLI-live backend**
actually needs to be configured through the trait; doing it now would ripple the
public `OcrOptions` API for a consumer that does not yet exist.

## Alternatives considered

### Collapse the trait, ship concrete `Tesseract`

Rejected. The trait is not vestigial — it backs a shipped CLI flag and a boxed
dispatch. Collapsing would remove `--backend`, fold ONNX/Candle handling into
ad-hoc `cmd_ocr` branches, and re-open the same design the next time a second
backend is wired. "One real adapter" is true of the *fully supported* set, but
the seam's job is to make adding the second adapter a local change.

### Deepen now: model-neutral `OcrOptions`, finish or delete Candle

Rejected as premature. Recasting `OcrOptions` before a second configurable
backend exists is speculative API churn (YAGNI). Finishing Candle is a feature
(model family + preprocessing + decoder + fixtures), out of scope for a seam
decision. Deleting Candle would discard a deliberately-placed, typed "not yet"
signal that the CLI and README already lean on.

## Consequences

- Future architecture-review passes should treat the `OcrBackend` seam as
  intentional and not re-suggest collapsing it; this document is the record.
- `src/ocr.rs` documents the advisory-hint contract inline so the source and this
  ADR agree: callers may rely only on a populated top-level `TextLayer::text`
  plus at least one page-level zone, and backends may ignore `OcrOptions` fields.
- Revisit `OcrOptions` when a second backend becomes CLI-live (most likely a
  neural one): at that point split the engine-neutral fields from
  engine-specific configuration, driven by the concrete second consumer rather
  than by anticipation.
