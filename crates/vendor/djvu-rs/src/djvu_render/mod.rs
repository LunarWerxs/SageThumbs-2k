//! Rendering pipeline for the new DjVuPage model (phase 5).
//!
//! This module provides the high-level rendering API for [`DjVuPage`] using the
//! clean-room decoders (IW44, JB2, BZZ) introduced in phases 2–3.
//!
//! ## Key public types
//!
//! - `RenderOptions` — render parameters (size, scale, bold, AA)
//! - `RenderError` — typed errors from the render pipeline
//!
//! ## Compositing model
//!
//! Three layers are composited in this order:
//!
//! 1. **Background** — IW44 wavelet-coded YCbCr image (BG44 chunks).
//!    YCbCr → RGB conversion happens HERE, and nowhere else.
//! 2. **Mask** — JB2 bilevel image (Sjbz chunk). Black pixels mark foreground.
//! 3. **Foreground palette** — FGbz-encoded color palette (FGbz chunk).
//!    Each foreground pixel is colored according to the palette.
//!
//! ## Gamma correction
//!
//! A `gamma_lut[256]` is precomputed from the INFO chunk `gamma` value using
//! `lut[i] = (i/255)^(doc_gamma/2.2) * 255`.  For the vast majority of DjVu
//! files (gamma = 2.2) the exponent is 1.0 → identity, no correction applied.
//!
//! ## Scaling
//!
//! Bilinear scaling uses 4-bit fixed-point fractional coordinates (FRACBITS=4).
//! Anti-aliasing downscale averages a 2×2 neighbourhood before outputting.
//!
//! ## Progressive rendering
//!
//! `render_coarse()` decodes only the first BG44 chunk; subsequent calls to
//! `render_progressive(chunk_n)` decode one additional chunk, yielding
//! progressively higher-quality images.

#[cfg(not(feature = "std"))]
use alloc::{string::String, sync::Arc, vec, vec::Vec};
#[cfg(feature = "std")]
use std::sync::Arc;

use crate::djvu_document::DjVuPage;
use crate::iw44::Iw44Image;
use crate::pixmap::{GrayPixmap, Pixmap};
use crate::render_size::RenderSize;

// Test-only counter of BG44 `decode_chunk` calls made through this module's
// two progressive call sites (the naive per-frame `decode_background_chunks`
// loop and `ProgressiveDecoder::push_bg44_chunk`).
//
// Exists to back the B5 structural claim — O(N²) chunk decodes for a
// per-frame `render_progressive_step` session vs O(N) for the stateful
// decoder — with an exact call count instead of noisy wall-clock timing.
// `#[cfg(test)]`-gated so it costs nothing (not even the branch) outside
// test builds. Thread-local (not a shared global) so parallel test runners
// (`cargo test`'s default multi-threaded harness) can't have unrelated
// tests on other threads pollute the count; the decode call sites this
// counts are not dispatched to other threads under the `cli`/default
// feature set `make check` tests with (no `parallel` feature).
#[cfg(test)]
thread_local! {
    pub(crate) static BG44_CHUNK_DECODES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_bg44_chunk_decode() {
    BG44_CHUNK_DECODES.with(|c| c.set(c.get() + 1));
}

// Structural counter for full JB2 mask decodes (test-only), mirroring
// `BG44_CHUNK_DECODES`. Lets the #607 retained-sub4 test prove that a warm
// downgraded page's sub≥4 re-render never re-runs the JB2 arithmetic decode.
#[cfg(test)]
thread_local! {
    pub(crate) static JB2_MASK_DECODES: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

#[cfg(test)]
fn count_jb2_mask_decode() {
    JB2_MASK_DECODES.with(|c| c.set(c.get() + 1));
}

// ── FGbz palette parsing ──────────────────────────────────────────────────────

// FGbz chunk parsing lives in `crate::fgbz` so this module receives already-decoded
// palette data and never calls `bzz_decode` itself. `parse_fgbz` returns a
// `BzzError`; callers in the submodules propagate it through `RenderError`'s `From<BzzError>`.
use crate::fgbz::{FgbzPalette, PaletteColor, parse_fgbz};

// ── Submodules ────────────────────────────────────────────────────────────────
//
// One file per concern (#889). Every item a sibling needs is `pub(super)`:
// the same reach a private item had when this module was one file.

// The render caches need `std` (Mutex, atomics); the module is gated whole.
#[cfg(feature = "std")]
mod cache;
mod composite;
mod layers;
mod options;
mod pipeline;
mod progressive;
mod request;
mod rotate;
mod sampling;

#[cfg(feature = "std")]
pub(crate) use cache::*;
pub use composite::*;
pub(crate) use layers::*;
pub use options::*;
use pipeline::*;
pub use progressive::*;
pub use request::*;
pub(crate) use rotate::*;
use sampling::*;

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
// The deprecated entry points keep their tests until they are removed.
#[allow(deprecated)]
mod tests;
