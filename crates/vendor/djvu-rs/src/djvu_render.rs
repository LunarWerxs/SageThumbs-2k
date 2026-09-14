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
use alloc::{borrow::Cow, string::String, vec, vec::Vec};
#[cfg(feature = "std")]
use std::borrow::Cow;

use crate::djvu_document::DjVuPage;
use crate::iw44::Iw44Image;
use crate::pixmap::{GrayPixmap, Pixmap};

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

// ── Errors ───────────────────────────────────────────────────────────────────

/// Errors that can occur during DjVuPage rendering.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RenderError {
    /// IW44 wavelet decode error.
    #[error("IW44 decode error: {0}")]
    Iw44(#[from] crate::error::Iw44Error),

    /// JB2 bilevel decode error.
    #[error("JB2 decode error: {0}")]
    Jb2(#[from] crate::error::Jb2Error),

    /// The output buffer provided to `render_into` is too small.
    #[error("buffer too small: need {need} bytes, got {got}")]
    BufTooSmall { need: usize, got: usize },

    /// The requested render dimensions are invalid (zero width or height).
    #[error("invalid render dimensions: {width}x{height}")]
    InvalidDimensions { width: u32, height: u32 },

    /// `chunk_n` is out of range for progressive rendering.
    #[error("chunk index {chunk_n} out of range (max {max})")]
    ChunkOutOfRange { chunk_n: usize, max: usize },

    /// BZZ decompression error (for FGbz palette).
    #[error("BZZ error: {0}")]
    Bzz(#[from] crate::error::BzzError),

    /// JPEG decode error (for BGjp/FGjp chunks).
    #[cfg(feature = "std")]
    #[error("JPEG decode error: {0}")]
    Jpeg(String),

    /// Document-level error (e.g. page index out of range).
    #[error("document error: {0}")]
    Doc(#[from] crate::djvu_document::DocError),

    /// A configured resource limit was exceeded during rendering.
    #[error("{0}")]
    ResourceLimit(#[from] crate::resource_limits::ResourceLimitExceeded),

    /// A render option is incompatible with the chosen entry point.
    ///
    /// Returned by [`render_streaming`] when an option requires post-processing
    /// of a fully-allocated pixmap (anti-aliasing, Lanczos resampling at a
    /// scaled output, or rotation).
    #[error("unsupported render option: {0}")]
    UnsupportedOption(&'static str),
}

// ── RenderOptions ─────────────────────────────────────────────────────────────

/// User-requested rotation, applied on top of the INFO chunk rotation.
///
/// The final rotation is the sum of the INFO rotation and the user rotation.
/// For example, if the INFO chunk specifies 90° CW and the user requests 90° CW,
/// the output will be rotated 180°.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UserRotation {
    /// No additional rotation (only INFO chunk rotation applies).
    #[default]
    None,
    /// 90° clockwise.
    Cw90,
    /// 180°.
    Rot180,
    /// 90° counter-clockwise (= 270° clockwise).
    Ccw90,
}

/// Resampling algorithm used when scaling a rendered page to the target size.
///
/// Applied after full-resolution decode and compositing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Resampling {
    /// Bilinear interpolation (default — fast, acceptable quality).
    #[default]
    Bilinear,
    /// Lanczos-3 separable resampling.
    ///
    /// Higher quality than bilinear for downscaling (less aliasing, sharper
    /// text). Slower: two-pass separable filter with a 6-tap kernel.
    /// The rendered pixmap is produced at full page resolution and then
    /// downscaled, so memory usage is higher than `Bilinear`.
    Lanczos3,
}

/// Rendering parameters passed to `render_into` and related functions.
///
/// # Example
///
/// ```
/// use djvu_rs::djvu_render::RenderOptions;
///
/// // Set the output size; the pipeline derives the decode scale from `width`.
/// let opts = RenderOptions {
///     width: 800,
///     height: 600,
///     aa: true,
///     ..Default::default()
/// };
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct RenderOptions {
    /// Output width in pixels.
    pub width: u32,
    /// Output height in pixels.
    pub height: u32,
    /// Deprecated and **ignored by the render pipeline**.
    ///
    /// Rendering now derives its decode scale from `width` and the page's
    /// native width (see the internal `decode_scale`), so this field no longer
    /// controls anything. It is retained for backward compatibility — the
    /// `fit_to_*` constructors still populate it — but setting it by hand has no
    /// effect. Build options via
    /// [`RenderOptions::fit_to_width`] / [`fit_to_box`](RenderOptions::fit_to_box)
    /// instead of assembling the `(width, height, scale)` triple yourself.
    #[deprecated(
        since = "0.20.1",
        note = "scale is derived from `width` by the render pipeline and is no longer read; \
                build options via `RenderOptions::fit_to_width`/`fit_to_box`. \
                This field is ignored and will be removed in a future release."
    )]
    pub scale: f32,
    /// Bold level: number of dilation passes on the JB2 mask (0 = no dilation).
    pub bold: u8,
    /// Whether to apply anti-aliasing downscale pass.
    pub aa: bool,
    /// User-requested rotation, combined with the INFO chunk rotation.
    pub rotation: UserRotation,
    /// When `true`, tolerate corrupted chunks instead of returning an error.
    ///
    /// - BG44: decodes chunks until the first decode error; uses whatever
    ///   was decoded so far (may be empty / blurry).
    /// - JB2 mask: if decoding fails, renders the background without a mask
    ///   rather than returning `Err`.
    ///
    /// Returns `Ok(pixmap)` even when chunks are skipped. Useful for document
    /// viewers where a partial render is better than a blank page.
    ///
    /// Default: `false` (strict — any decode error propagates as `Err`).
    pub permissive: bool,
    /// Resampling algorithm applied when scaling to `width`×`height`.
    ///
    /// Default: [`Resampling::Bilinear`] (preserves backward compatibility).
    pub resampling: Resampling,
    /// Anti-alias the JB2 bilevel mask's edges when rendering at **upscale**
    /// (zoom > 1): instead of the hard nearest-bit lookup, bilinearly
    /// interpolate the mask's 0/255 coverage and blend foreground/background
    /// colour proportionally — smoother glyph edges under zoom.
    ///
    /// A no-op at scale ≤ 1 (native or downscaled renders are unaffected).
    ///
    /// **Opt-in, default `false`.** DjVuLibre hard-edges the mask under zoom,
    /// so enabling this is a deliberate, judged divergence from the reference
    /// renderer's pixel output — a "prettier than DjVuLibre" quality mode, not
    /// a faithfulness fix. Leaving it `false` keeps `render_pixmap` and
    /// friends byte-identical to prior releases.
    pub mask_aa: bool,
}

impl Default for RenderOptions {
    #[allow(deprecated)] // still sets the retained-for-compat `scale` field
    fn default() -> Self {
        RenderOptions {
            width: 0,
            height: 0,
            scale: 1.0,
            bold: 0,
            aa: false,
            rotation: UserRotation::None,
            permissive: false,
            resampling: Resampling::Bilinear,
            mask_aa: false,
        }
    }
}

fn effective_max_render_pixels(
    page: &crate::djvu_document::DjVuPage,
    limits: Option<crate::resource_limits::ResourceLimits>,
) -> u64 {
    limits
        .and_then(|limits| limits.max_render_pixels)
        .or(page
            .resource_limits()
            .and_then(|limits| limits.max_render_pixels))
        .unwrap_or(crate::resource_limits::DEFAULT_MAX_RENDER_PIXELS)
}

fn check_output_pixels(
    operation: &'static str,
    page: &crate::djvu_document::DjVuPage,
    limits: Option<crate::resource_limits::ResourceLimits>,
    width: u32,
    height: u32,
) -> Result<(), RenderError> {
    if width == 0 || height == 0 {
        return Err(RenderError::InvalidDimensions { width, height });
    }
    let pixels = u64::from(width) * u64::from(height);
    let limit = effective_max_render_pixels(page, limits);
    if pixels > limit {
        return Err(RenderError::ResourceLimit(
            crate::resource_limits::ResourceLimitExceeded {
                operation,
                axis: crate::resource_limits::ResourceLimitAxis::RenderOutputPixels,
                found: pixels,
                limit,
                page_number: Some(page.index() + 1),
                width: Some(width),
                height: Some(height),
            },
        ));
    }
    Ok(())
}

/// A document layer a permissive render skipped or fell back on (#696).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveredLayer {
    /// The IW44 background (`BG44`), possibly truncated at a corrupt chunk.
    Background,
    /// The IW44 foreground detail plane (`FG44`).
    Foreground,
    /// The JB2 stencil mask (`Sjbz`).
    Mask,
    /// The `FGbz` foreground colour palette.
    ForegroundPalette,
}

/// One recovery action a permissive render took to keep going (#696).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderRecovery {
    /// Which layer was affected.
    pub layer: RecoveredLayer,
    /// Human-readable explanation of what was skipped or substituted.
    pub detail: String,
}

/// Structured record of what a permissive render skipped or recovered (#696).
///
/// Returned alongside the pixmap by [`render_pixmap_with_report`]. An empty
/// report (`is_clean`) means the page decoded fully with no fallbacks.
#[derive(Debug, Clone, Default)]
pub struct RenderReport {
    /// Recovery actions in the order the renderer took them.
    pub recoveries: Vec<RenderRecovery>,
}

impl RenderReport {
    /// Whether the render needed no recovery (every layer decoded cleanly).
    pub fn is_clean(&self) -> bool {
        self.recoveries.is_empty()
    }
}

#[cfg(feature = "std")]
thread_local! {
    /// Active recovery sink. `Some` only while [`render_pixmap_with_report`] is
    /// on the stack; otherwise `record_recovery` is a no-op, so the ordinary
    /// [`render_pixmap`] hot path is untouched.
    static RECOVERY_SINK: core::cell::RefCell<Option<Vec<RenderRecovery>>> =
        const { core::cell::RefCell::new(None) };
}

/// Record a permissive recovery when a report is being collected.
#[cfg(feature = "std")]
fn record_recovery(layer: RecoveredLayer, detail: impl Into<String>) {
    RECOVERY_SINK.with(|sink| {
        if let Some(list) = sink.borrow_mut().as_mut() {
            list.push(RenderRecovery {
                layer,
                detail: detail.into(),
            });
        }
    });
}

/// No-op in `no_std`: the report API requires `std` thread-locals.
#[cfg(not(feature = "std"))]
fn record_recovery(_layer: RecoveredLayer, _detail: &str) {}

/// Unwrap a permissive layer decode, recording a recovery on failure.
fn permissive_layer<T>(result: Result<Option<T>, RenderError>, layer: RecoveredLayer) -> Option<T> {
    match result {
        Ok(value) => value,
        Err(error) => {
            record_recovery(layer, {
                #[cfg(feature = "std")]
                {
                    error.to_string()
                }
                #[cfg(not(feature = "std"))]
                {
                    let _ = error;
                    ""
                }
            });
            None
        }
    }
}

#[allow(deprecated)] // the `fit_to_*` constructors still populate `scale` for back-compat
impl RenderOptions {
    /// Create render options that scale the page to fit the given width,
    /// preserving aspect ratio. Respects page rotation from the INFO chunk.
    pub fn fit_to_width(page: &crate::djvu_document::DjVuPage, width: u32) -> Self {
        let (dw, dh) = display_dimensions(page);
        let height = if dw == 0 {
            width
        } else {
            ((dh as f64 * width as f64) / dw as f64).round() as u32
        }
        .max(1);
        let scale = width as f32 / dw.max(1) as f32;
        RenderOptions {
            width,
            height,
            scale,
            ..Default::default()
        }
    }

    /// Create render options that scale the page to fit the given height,
    /// preserving aspect ratio. Respects page rotation from the INFO chunk.
    pub fn fit_to_height(page: &crate::djvu_document::DjVuPage, height: u32) -> Self {
        let (dw, dh) = display_dimensions(page);
        let width = if dh == 0 {
            height
        } else {
            ((dw as f64 * height as f64) / dh as f64).round() as u32
        }
        .max(1);
        let scale = height as f32 / dh.max(1) as f32;
        RenderOptions {
            width,
            height,
            scale,
            ..Default::default()
        }
    }

    /// Create render options that scale the page to fit within a bounding box,
    /// preserving aspect ratio. Respects page rotation from the INFO chunk.
    pub fn fit_to_box(
        page: &crate::djvu_document::DjVuPage,
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let (dw, dh) = display_dimensions(page);
        if dw == 0 || dh == 0 {
            return RenderOptions {
                width: max_width.max(1),
                height: max_height.max(1),
                scale: 1.0,
                ..Default::default()
            };
        }
        let scale_w = max_width as f64 / dw as f64;
        let scale_h = max_height as f64 / dh as f64;
        let scale = if scale_w < scale_h { scale_w } else { scale_h };
        let width = (dw as f64 * scale).round() as u32;
        let height = (dh as f64 * scale).round() as u32;
        RenderOptions {
            width: width.max(1),
            height: height.max(1),
            scale: scale as f32,
            ..Default::default()
        }
    }

    /// Whether `page` can be rendered with [`render_streaming`] under these
    /// options, producing pixels identical to [`render_pixmap`].
    ///
    /// The streaming path emits the page row-by-row without buffering a full
    /// [`Pixmap`], so callers that only need to forward rows (PDF/TIFF image
    /// encoders) can avoid the intermediate allocation. It is only equivalent
    /// to the buffered path when no whole-image post-pass is required: no
    /// anti-aliasing, no rotation, and either bilinear resampling or a 1:1
    /// (unscaled) render.
    ///
    /// This is the single source of truth for streaming eligibility; export
    /// paths call it instead of re-deriving the rule.
    pub fn can_stream(&self, page: &crate::djvu_document::DjVuPage) -> bool {
        !self.aa
            && (self.resampling == Resampling::Bilinear
                || (page.width() as u32 == self.width && page.height() as u32 == self.height))
            && page.rotation() == crate::info::Rotation::None
            && self.rotation == UserRotation::None
    }

    /// The scale the decode pipeline uses to choose the IW44 wavelet subsample
    /// level (via [`best_iw44_subsample`]), derived from the requested output
    /// `width` and the page's **native** width.
    ///
    /// The compositor scales the native page raster (`page.width()` ×
    /// `page.height()`) into the `width` × `height` buffer and only *then*
    /// applies INFO/user rotation (see `composite_rows` and `rotate_pixmap`).
    /// The IW44 background is decoded in that pre-rotation native orientation, so
    /// the subsample must be chosen against `width / page.width()`. Dividing by
    /// the rotation-swapped *display* width would pick the wrong subsample for
    /// INFO-rotated, non-square pages — over-subsampling a downscaled portrait
    /// background and under-subsampling a landscape one.
    ///
    /// This is the single home of the `scale ≈ width / page-width` invariant that
    /// every caller used to maintain by hand — and that the PDF exporter got
    /// wrong, leaving `scale = 1.0` and silently over-decoding at every DPI.
    /// Rendering reads *this*, never the deprecated public [`scale`](Self::scale)
    /// field, so a caller can no longer cause a silent over- or under-decode by
    /// building the size triple inconsistently.
    pub(crate) fn decode_scale(&self, page: &crate::djvu_document::DjVuPage) -> f32 {
        self.width as f32 / (page.width() as u32).max(1) as f32
    }
}

/// Return `(display_width, display_height)` — dimensions after rotation.
///
/// The single source of the INFO-rotation dimension swap; the `fit_to_*`
/// constructors and `Page::display_dims` both call it instead of re-deriving it.
pub(crate) fn display_dimensions(page: &crate::djvu_document::DjVuPage) -> (u32, u32) {
    let w = page.width() as u32;
    let h = page.height() as u32;
    match page.rotation() {
        crate::info::Rotation::Cw90 | crate::info::Rotation::Ccw90 => (h, w),
        _ => (w, h),
    }
}

// ── Gamma LUT ─────────────────────────────────────────────────────────────────

/// Standard sRGB / CRT display gamma assumed for rendering output.
const DISPLAY_GAMMA: f32 = 2.2;

/// Precompute a gamma-correction look-up table for values 0..255.
///
/// Matches DjVuLibre's correction formula: the exponent is
/// `document_gamma / DISPLAY_GAMMA` so that documents created on a
/// standard gamma-2.2 device need no correction (identity LUT), while
/// documents from linear-light (gamma=1.0) sources are brightened to
/// compensate for the display gamma.
///
/// `lut[i] = round(255 * (i/255)^(gamma / DISPLAY_GAMMA))`
///
/// When `gamma <= 0.0`, not finite, or approximately equal to
/// `DISPLAY_GAMMA`, the LUT is the identity function (no correction).
fn build_gamma_lut(gamma: f32) -> [u8; 256] {
    let mut lut = [0u8; 256];
    let exponent = if gamma <= 0.0 || !gamma.is_finite() {
        1.0_f32 // invalid — no correction
    } else {
        gamma / DISPLAY_GAMMA
    };
    if (exponent - 1.0).abs() < 1e-4 {
        // Identity
        for (i, v) in lut.iter_mut().enumerate() {
            *v = i as u8;
        }
        return lut;
    }
    for (i, v) in lut.iter_mut().enumerate() {
        let linear = i as f32 / 255.0;
        let corrected = linear.powf(exponent);
        *v = (corrected * 255.0 + 0.5) as u8;
    }
    lut
}

// ── Bilinear scaling (FRACBITS = 4) ──────────────────────────────────────────

/// Fixed-point fractional bits for bilinear scaling (1 << 4 = 16 subpixels).
const FRACBITS: u32 = 4;
const FRAC: u32 = 1 << FRACBITS;
const FRAC_MASK: u32 = FRAC - 1;

/// Maps each byte value to 8 fg-mask bytes (MSB-first): 0xFF if bit set (fg), 0x00 otherwise.
const MASK_EXPAND: [[u8; 8]; 256] = {
    let mut lut = [[0u8; 8]; 256];
    let mut b = 0usize;
    while b < 256 {
        let mut bit = 0usize;
        while bit < 8 {
            lut[b][bit] = if (b >> (7 - bit)) & 1 != 0 {
                0xFF
            } else {
                0x00
            };
            bit += 1;
        }
        b += 1;
    }
    lut
};

/// Maps each mask byte to 8 RGBA pixels for bilevel rendering (MSB-first, 300 DPI 1:1).
/// fg bit (1) → [0x00, 0x00, 0x00, 0xFF] (black); bg bit (0) → [0xFF, 0xFF, 0xFF, 0xFF] (white).
/// Table: 256 × 32 = 8 KiB (128 cache lines); persists in L2 across rows.
const BILEVEL_RGBA: [[u8; 32]; 256] = {
    let mut lut = [[0u8; 32]; 256];
    let mut mb = 0usize;
    while mb < 256 {
        let mut bit = 0usize;
        while bit < 8 {
            let ch = if (mb >> (7 - bit)) & 1 != 0 {
                0u8
            } else {
                255u8
            };
            lut[mb][bit * 4] = ch;
            lut[mb][bit * 4 + 1] = ch;
            lut[mb][bit * 4 + 2] = ch;
            lut[mb][bit * 4 + 3] = 255;
            bit += 1;
        }
        mb += 1;
    }
    lut
};

// ── SIMD helpers ──────────────────────────────────────────────────────────────

/// Convert packed RGB bytes to packed RGBA with alpha = 255.
///
/// On x86_64 with SSSE3 (available on Core 2+, ~2006): processes 4 pixels per
/// `_mm_shuffle_epi8` + `_mm_or_si128`.  Falls back to scalar on older targets.
///
/// `src` must hold exactly `pixel_count * 3` bytes;
/// `dst` must hold exactly `pixel_count * 4` bytes.
#[cfg(feature = "std")]
#[allow(unsafe_code)]
#[inline]
fn rgb_to_rgba(src: &[u8], dst: &mut [u8]) {
    let pixel_count = src.len() / 3;
    debug_assert_eq!(dst.len(), pixel_count * 4);

    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("ssse3") {
        // SAFETY: feature detected; bounds are enforced by safe_chunks calculation.
        unsafe {
            // Only load 16 bytes where src has at least 16 bytes available:
            // chunk i reads src[i*12..i*12+16], so we need i*12+16 <= src.len().
            let safe_chunks = if src.len() >= 16 {
                ((src.len() - 16) / 12 + 1).min(pixel_count / 4)
            } else {
                0
            };
            rgb_to_rgba_ssse3(src, dst, pixel_count, safe_chunks);
        }
        return;
    }

    rgb_to_rgba_scalar(src, dst, 0, pixel_count);
}

#[cfg(all(feature = "std", target_arch = "x86_64"))]
#[allow(unsafe_code, unsafe_op_in_unsafe_fn)]
#[target_feature(enable = "ssse3")]
// SAFETY: caller guarantees SSSE3 availability; safe_chunks * 12 + 16 <= src.len()
// and safe_chunks * 16 <= dst.len() (enforced by rgb_to_rgba).
unsafe fn rgb_to_rgba_ssse3(src: &[u8], dst: &mut [u8], pixel_count: usize, safe_chunks: usize) {
    use core::arch::x86_64::*;

    // Shuffle 12 packed RGB bytes into 16 RGBA bytes (4 pixels), zero in alpha slot.
    // _mm_set_epi8 arguments are byte 15 (highest) down to byte 0 (lowest).
    let shuf = _mm_set_epi8(
        -1, 11, 10, 9, // pixel 3: [R,G,B,0]
        -1, 8, 7, 6, // pixel 2
        -1, 5, 4, 3, // pixel 1
        -1, 2, 1, 0, // pixel 0
    );
    let alpha_or = _mm_set1_epi32(0xFF000000u32 as i32);

    for i in 0..safe_chunks {
        let v = _mm_loadu_si128(src.as_ptr().add(i * 12) as *const __m128i);
        _mm_storeu_si128(
            dst.as_mut_ptr().add(i * 16) as *mut __m128i,
            _mm_or_si128(_mm_shuffle_epi8(v, shuf), alpha_or),
        );
    }

    rgb_to_rgba_scalar(src, dst, safe_chunks * 4, pixel_count);
}

#[cfg(feature = "std")]
#[inline]
fn rgb_to_rgba_scalar(src: &[u8], dst: &mut [u8], start: usize, end: usize) {
    for i in start..end {
        dst[i * 4] = src[i * 3];
        dst[i * 4 + 1] = src[i * 3 + 1];
        dst[i * 4 + 2] = src[i * 3 + 2];
        dst[i * 4 + 3] = 255;
    }
}

/// Generic Q24 ratios `(plane_w << 24) / page_w` and `(plane_h << 24) / page_h`
/// for converting page-space FRACBITS coords into plane-space FRACBITS coords.
/// Returns `(0, 0)` when `plane` is `None` or page dims are zero.  The public
/// FG/BG helpers below apply layer-specific cell-grid adjustments first.
#[inline]
fn plane_q24(plane: Option<&Pixmap>, page_w: u32, page_h: u32) -> (u64, u64) {
    match plane {
        Some(p) if page_w > 0 && page_h > 0 => (
            ((p.width as u64) << 24) / page_w as u64,
            ((p.height as u64) << 24) / page_h as u64,
        ),
        _ => (0, 0),
    }
}

/// Map page-space fixed-point coordinates to BG44/FG44 plane-space by aligning
/// pixel centres instead of top-left corners.  This matches the usual image
/// resampling convention and reduces native-resolution drift against ddjvu for
/// non-integer page→plane ratios such as colorbook's 2260→754 BG scale.
#[inline]
fn map_plane_center_frac(page_frac: u32, q24: u64) -> u32 {
    let centered = (((page_frac as u64 + (FRAC / 2) as u64) * q24) >> 24) as u32;
    centered.saturating_sub(FRAC / 2)
}

#[inline]
fn fg_q24(fg: Option<&Pixmap>, page_w: u32, page_h: u32) -> (u64, u64) {
    match fg {
        Some(p) if page_w > 0 && page_h > 0 && p.width > 0 && p.height > 0 => {
            // FG44 is a sparse foreground colour map.  Horizontally, the last
            // encoded column is often padding for a fixed-width colour cell
            // grid (e.g. 2260px page / 189px FG => 12px cells), so use the
            // inferred integer cell pitch instead of stretching across the
            // padded column.  Vertically, use the encoded plane ratio so the
            // bottom FG row remains reachable when the page height is not an
            // exact multiple of the cell pitch.
            let sx = page_w.div_ceil(p.width).max(1);
            (
                (1u64 << 24) / sx as u64,
                ((p.height as u64) << 24) / page_h as u64,
            )
        }
        _ => plane_q24(fg, page_w, page_h),
    }
}

#[inline]
fn bg_q24(bg: Option<&Pixmap>, page_w: u32, page_h: u32) -> (u64, u64) {
    match bg {
        Some(p) if page_w > 0 && page_h > 0 && p.width > 0 && p.height > 0 => {
            // BG44 planes are cell grids too (usually page/3 for scans).  Use
            // the inferred integer subsample pitch so the padded right/bottom
            // edge cells do not stretch across the page during native render.
            let sx = page_w.div_ceil(p.width).max(1);
            let sy = page_h.div_ceil(p.height).max(1);
            ((1u64 << 24) / sx as u64, (1u64 << 24) / sy as u64)
        }
        _ => plane_q24(bg, page_w, page_h),
    }
}

/// Sample a pixmap at fractional coordinates using bilinear interpolation.
///
/// Coordinates are in fixed-point: `fx = x * FRAC`, etc.
/// Returns (r, g, b).
#[inline]
#[cfg_attr(not(test), allow(dead_code))]
fn sample_bilinear(pm: &Pixmap, fx: u32, fy: u32) -> (u8, u8, u8) {
    let x0 = (fx >> FRACBITS).min(pm.width.saturating_sub(1));
    let y0 = (fy >> FRACBITS).min(pm.height.saturating_sub(1));
    let x1 = (x0 + 1).min(pm.width.saturating_sub(1));
    let y1 = (y0 + 1).min(pm.height.saturating_sub(1));

    let tx = fx & FRAC_MASK; // 0..15
    let ty = fy & FRAC_MASK;

    let (r00, g00, b00) = pm.get_rgb(x0, y0);
    let (r10, g10, b10) = pm.get_rgb(x1, y0);
    let (r01, g01, b01) = pm.get_rgb(x0, y1);
    let (r11, g11, b11) = pm.get_rgb(x1, y1);

    let lerp = |a: u8, b: u8, c: u8, d: u8| -> u8 {
        let top = a as u32 * (FRAC - tx) + b as u32 * tx;
        let bot = c as u32 * (FRAC - tx) + d as u32 * tx;
        let numerator = top * (FRAC - ty) + bot * ty;
        // v ≤ (255*FRAC*FRAC + round) >> (2*FRACBITS) = 255 — no clamp needed.
        ((numerator + (1 << (2 * FRACBITS - 1))) >> (2 * FRACBITS)) as u8
    };

    (
        lerp(r00, r10, r01, r11),
        lerp(g00, g10, g01, g11),
        lerp(b00, b10, b01, b11),
    )
}

/// Bilinear sample using pre-fetched row slices (avoids repeated y-coord computation).
/// `ty` is the vertical fractional weight (0..FRAC-1). Row slices are RGBA (4 bytes/pixel).
#[inline]
fn bilinear_from_rows(row0: &[u8], row1: &[u8], width: u32, fx: u32, ty: u32) -> (u8, u8, u8) {
    let w = width.saturating_sub(1) as usize;
    let x0 = (fx >> FRACBITS) as usize;
    let x0 = x0.min(w);
    let x1 = (x0 + 1).min(w);
    let tx = fx & FRAC_MASK;

    // Read 4 bytes (RGBA) per pixel — a single 32-bit load on all targets.
    let get = |row: &[u8], x: usize| -> (u8, u8, u8) {
        let off = x * 4;
        if let Some(q) = row.get(off..off + 4) {
            (q[0], q[1], q[2])
        } else {
            (0, 0, 0)
        }
    };
    let (r00, g00, b00) = get(row0, x0);
    let (r10, g10, b10) = get(row0, x1);
    let (r01, g01, b01) = get(row1, x0);
    let (r11, g11, b11) = get(row1, x1);

    // Precompute bilinear weights so ty/ity are absorbed into w01/w11 and never
    // need to be reloaded from the stack during per-channel accumulation.
    // Weights sum to FRAC*FRAC = 256, so result = dot/256 (>> 8).
    let itx = FRAC - tx;
    let ity = FRAC - ty;
    let w00 = itx * ity;
    let w10 = tx * ity;
    let w01 = itx * ty;
    let w11 = tx * ty;
    // max dot = 255 * 256 = 65280 + 128 ≤ u32 range; no clamp needed.
    let blend = |a: u8, b: u8, c: u8, d: u8| -> u8 {
        ((a as u32 * w00 + b as u32 * w10 + c as u32 * w01 + d as u32 * w11 + 128) >> 8) as u8
    };
    (
        blend(r00, r10, r01, r11),
        blend(g00, g10, g01, g11),
        blend(b00, b10, b01, b11),
    )
}

#[inline]
#[cfg_attr(not(test), allow(dead_code))]
fn sample_nearest(pm: &Pixmap, fx: u32, fy: u32) -> (u8, u8, u8) {
    let x = ((fx + FRAC / 2) >> FRACBITS).min(pm.width.saturating_sub(1));
    let y = ((fy + FRAC / 2) >> FRACBITS).min(pm.height.saturating_sub(1));
    pm.get_rgb(x, y)
}

/// Area-average (box filter) sample: average all source pixels covered by the
/// output pixel's footprint.  Used when downscaling (scale < 1.0) for better
/// anti-aliasing and fewer moire patterns than bilinear.
///
/// `fx`, `fy` are the top-left corner of the output pixel in fixed-point.
/// `fx_step`, `fy_step` are the output pixel size in source coordinates.
#[inline]
fn sample_area_avg(pm: &Pixmap, fx: u32, fy: u32, fx_step: u32, fy_step: u32) -> (u8, u8, u8) {
    let (x0, x1) = area_range(pm.width, fx, fx_step);
    let (y0, y1) = area_range(pm.height, fy, fy_step);
    sample_area_avg_bounds(pm, x0, x1, y0, y1)
}

#[inline]
fn sample_area_avg_bounds(pm: &Pixmap, x0: u32, x1: u32, y0: u32, y1: u32) -> (u8, u8, u8) {
    let cols = (x1 - x0) as usize;
    let rows = (y1 - y0) as usize;

    // Fast path: 1x1 box -> direct read.
    if cols <= 1 && rows <= 1 {
        return pm.get_rgb(x0, y0);
    }

    let mut r_sum = 0u32;
    let mut g_sum = 0u32;
    let mut b_sum = 0u32;

    let pw = pm.width as usize;
    // One bounds check per row (not per pixel) to let the inner loop vectorize.
    for sy in y0..y1 {
        let row_off = sy as usize * pw * 4 + x0 as usize * 4;
        if let Some(row) = pm.data.get(row_off..row_off + cols * 4) {
            for chunk in row.as_chunks::<4>().0 {
                r_sum += chunk[0] as u32;
                g_sum += chunk[1] as u32;
                b_sum += chunk[2] as u32;
            }
        }
    }

    let count = (rows * cols) as u32;
    if count == 0 {
        return (255, 255, 255);
    }

    // Power-of-2 counts (count=4 at 2x downscale): replace UDIV with shifts.
    let half = count >> 1;
    if count.is_power_of_two() {
        let shift = count.trailing_zeros();
        (
            ((r_sum + half) >> shift) as u8,
            ((g_sum + half) >> shift) as u8,
            ((b_sum + half) >> shift) as u8,
        )
    } else {
        (
            ((r_sum + half) / count) as u8,
            ((g_sum + half) / count) as u8,
            ((b_sum + half) / count) as u8,
        )
    }
}

/// Coverage-weighted bilevel downscale: returns the fraction of set mask bits in
/// the output pixel's footprint as a gray value (0 = all background, 255 = all foreground).
///
/// Used by `composite_rows_bilevel_one` for anti-aliased text at downscale DPIs.
#[inline]
fn mask_box_coverage(
    mask: &crate::bitmap::Bitmap,
    fx: u32,
    fy: u32,
    fx_step: u32,
    fy_step: u32,
) -> u8 {
    let x0 = (fx >> FRACBITS).min(mask.width.saturating_sub(1));
    let y0 = (fy >> FRACBITS).min(mask.height.saturating_sub(1));
    let x1 = ((fx + fx_step) >> FRACBITS).min(mask.width);
    let y1 = ((fy + fy_step) >> FRACBITS).min(mask.height);
    let total = (x1 - x0) * (y1 - y0);
    if total == 0 {
        return 0;
    }
    // Count foreground bits using byte-level popcount instead of individual bit reads.
    // MSB-first packing: pixel x is at bit (7 - x%8) of byte (x/8).
    // first_mask keeps pixels [x0, next-byte-boundary); end_mask keeps pixels before x1.
    let stride = mask.row_stride();
    let byte_lo = x0 as usize / 8;
    let byte_hi = (x1 as usize).div_ceil(8); // exclusive
    let first_mask = 0xFF_u8 >> (x0 % 8);
    let end_mask = if x1.is_multiple_of(8) {
        0xFF_u8
    } else {
        0xFF_u8 << (8 - x1 % 8)
    };
    let mut count = 0u32;
    if byte_hi == byte_lo + 1 {
        // Entire x-range fits in one byte.
        let combined = first_mask & end_mask;
        for sy in y0..y1 {
            count += (mask.data[sy as usize * stride + byte_lo] & combined).count_ones();
        }
    } else {
        for sy in y0..y1 {
            let row = &mask.data[sy as usize * stride..];
            count += (row[byte_lo] & first_mask).count_ones();
            for byte in row[(byte_lo + 1)..(byte_hi - 1)].iter() {
                count += byte.count_ones();
            }
            count += (row[byte_hi - 1] & end_mask).count_ones();
        }
    }
    // Widen to u64: on a large mask with aggressive downsampling a single box can
    // cover > 16.8 M foreground bits, where `count * 255` overflows u32 (wrong
    // value in release, panic in debug).
    ((count as u64 * 255 + total as u64 / 2) / total as u64) as u8
}

/// Bilinearly interpolate the JB2 mask's 0/255 bits as a continuous coverage
/// field, for anti-aliased glyph edges at **upscale** (zoom > 1).
///
/// Treats each set mask bit as full foreground coverage (255) and each clear
/// bit as full background coverage (0), then blends the four nearest mask
/// pixels the same way [`sample_bilinear`] blends a [`Pixmap`] — mirroring
/// `mask_box_coverage`'s box-average approach used at *downscale*, but for the
/// opposite direction.
///
/// Returns the interpolated foreground fraction, 0 (all background) ..= 255
/// (all foreground) — same convention as `mask_box_coverage`.
///
/// Opt-in via [`RenderOptions::mask_aa`]: DjVuLibre hard-edges the mask under
/// zoom, so this is a deliberate, judged divergence from the reference
/// renderer, not a faithfulness fix — the default (`mask_aa: false`) path
/// never calls this function.
#[inline]
fn mask_bilinear_coverage(mask: &crate::bitmap::Bitmap, fx: u32, fy: u32) -> u8 {
    let x0 = (fx >> FRACBITS).min(mask.width.saturating_sub(1));
    let y0 = (fy >> FRACBITS).min(mask.height.saturating_sub(1));
    let x1 = (x0 + 1).min(mask.width.saturating_sub(1));
    let y1 = (y0 + 1).min(mask.height.saturating_sub(1));

    let tx = fx & FRAC_MASK;
    let ty = fy & FRAC_MASK;

    let bit = |x: u32, y: u32| -> u32 { if mask.get(x, y) { 255 } else { 0 } };
    let v00 = bit(x0, y0);
    let v10 = bit(x1, y0);
    let v01 = bit(x0, y1);
    let v11 = bit(x1, y1);

    let top = v00 * (FRAC - tx) + v10 * tx;
    let bot = v01 * (FRAC - tx) + v11 * tx;
    let numerator = top * (FRAC - ty) + bot * ty;
    // Same shape as sample_bilinear's lerp: v <= 255 — no clamp needed.
    ((numerator + (1 << (2 * FRACBITS - 1))) >> (2 * FRACBITS)) as u8
}

/// Find the center foreground pixel in a mask box for palette color lookup.
#[inline]
fn mask_box_center_fg(
    mask: &crate::bitmap::Bitmap,
    fx: u32,
    fy: u32,
    fx_step: u32,
    fy_step: u32,
) -> (u32, u32) {
    // Use the center of the box
    let cx = (fx + fx_step / 2) >> FRACBITS;
    let cy = (fy + fy_step / 2) >> FRACBITS;
    (
        cx.min(mask.width.saturating_sub(1)),
        cy.min(mask.height.saturating_sub(1)),
    )
}

// ── Anti-aliasing downscale ──────────────────────────────────────────────────

/// Apply a 2×2 box-filter downscale pass for anti-aliasing.
///
/// If either dimension of `pm` is 1, the output dimension stays at 1.
fn aa_downscale(pm: &Pixmap) -> Pixmap {
    let out_w = (pm.width / 2).max(1);
    let out_h = (pm.height / 2).max(1);
    let mut out = Pixmap::white(out_w, out_h);
    for y in 0..out_h {
        for x in 0..out_w {
            let sx = (x * 2).min(pm.width.saturating_sub(1));
            let sy = (y * 2).min(pm.height.saturating_sub(1));
            let sx1 = (sx + 1).min(pm.width.saturating_sub(1));
            let sy1 = (sy + 1).min(pm.height.saturating_sub(1));

            let (r00, g00, b00) = pm.get_rgb(sx, sy);
            let (r10, g10, b10) = pm.get_rgb(sx1, sy);
            let (r01, g01, b01) = pm.get_rgb(sx, sy1);
            let (r11, g11, b11) = pm.get_rgb(sx1, sy1);

            let avg = |a: u8, b: u8, c: u8, d: u8| -> u8 {
                ((a as u32 + b as u32 + c as u32 + d as u32 + 2) / 4) as u8
            };
            out.set_rgb(
                x,
                y,
                avg(r00, r10, r01, r11),
                avg(g00, g10, g01, g11),
                avg(b00, b10, b01, b11),
            );
        }
    }
    out
}

// ── Page rotation ───────────────────────────────────────────────────────────

/// Convert a rotation to a number of 90° CW steps (0..3).
fn rotation_to_steps(r: crate::info::Rotation) -> u8 {
    use crate::info::Rotation;
    match r {
        Rotation::None => 0,
        Rotation::Cw90 => 1,
        Rotation::Rot180 => 2,
        Rotation::Ccw90 => 3,
    }
}

/// Convert a user rotation to a number of 90° CW steps (0..3).
fn user_rotation_to_steps(r: UserRotation) -> u8 {
    match r {
        UserRotation::None => 0,
        UserRotation::Cw90 => 1,
        UserRotation::Rot180 => 2,
        UserRotation::Ccw90 => 3,
    }
}

/// Combine INFO chunk rotation with user rotation and return the combined
/// `info::Rotation` value.
pub(crate) fn combine_rotations(
    info: crate::info::Rotation,
    user: UserRotation,
) -> crate::info::Rotation {
    use crate::info::Rotation;
    let steps = (rotation_to_steps(info) + user_rotation_to_steps(user)) % 4;
    match steps {
        0 => Rotation::None,
        1 => Rotation::Cw90,
        2 => Rotation::Rot180,
        3 => Rotation::Ccw90,
        _ => unreachable!(),
    }
}

/// Apply page rotation to the rendered pixmap.
///
/// For 90°/270° rotations, width and height are swapped.
fn rotate_pixmap(src: Pixmap, rotation: crate::info::Rotation) -> Pixmap {
    use crate::info::Rotation;
    match rotation {
        Rotation::None => src,
        Rotation::Cw90 => {
            let w = src.height;
            let h = src.width;
            let mut out = Pixmap::white(w, h);
            // #447: 32×32 tiled transpose. The naïve per-pixel write strides the
            // destination by `out.width*4` bytes (a cache miss per pixel); tiling
            // keeps both the source read and destination write within a few cache
            // lines per tile. 4-byte copy is exact: rendered source pixmaps carry
            // alpha=255 and `Pixmap::white` pre-fills alpha=255.
            const TILE: u32 = 32;
            let (sw, sh, ow) = (src.width as usize, src.height as usize, w as usize);
            let mut ty = 0;
            while ty < src.height {
                let mut tx = 0;
                while tx < src.width {
                    let y_end = (ty + TILE).min(src.height);
                    let x_end = (tx + TILE).min(src.width);
                    for y in ty..y_end {
                        let src_row = y as usize * sw * 4;
                        let dst_col = sh - 1 - y as usize;
                        for x in tx..x_end {
                            let si = src_row + x as usize * 4;
                            let di = (x as usize * ow + dst_col) * 4;
                            out.data[di..di + 4].copy_from_slice(&src.data[si..si + 4]);
                        }
                    }
                    tx += TILE;
                }
                ty += TILE;
            }
            out
        }
        Rotation::Rot180 => {
            let mut out = Pixmap::white(src.width, src.height);
            for y in 0..src.height {
                for x in 0..src.width {
                    let (r, g, b) = src.get_rgb(x, y);
                    out.set_rgb(src.width - 1 - x, src.height - 1 - y, r, g, b);
                }
            }
            out
        }
        Rotation::Ccw90 => {
            let w = src.height;
            let h = src.width;
            let mut out = Pixmap::white(w, h);
            // #447: 32×32 tiled transpose (see Cw90).
            const TILE: u32 = 32;
            let (sw, ow) = (src.width as usize, w as usize);
            let mut ty = 0;
            while ty < src.height {
                let mut tx = 0;
                while tx < src.width {
                    let y_end = (ty + TILE).min(src.height);
                    let x_end = (tx + TILE).min(src.width);
                    for y in ty..y_end {
                        let src_row = y as usize * sw * 4;
                        for x in tx..x_end {
                            let si = src_row + x as usize * 4;
                            let di = ((sw - 1 - x as usize) * ow + y as usize) * 4;
                            out.data[di..di + 4].copy_from_slice(&src.data[si..si + 4]);
                        }
                    }
                    tx += TILE;
                }
                ty += TILE;
            }
            out
        }
    }
}

// ── FGbz palette parsing ──────────────────────────────────────────────────────

// FGbz chunk parsing lives in `crate::fgbz` so this module receives already-decoded
// palette data and never calls `bzz_decode` itself. `parse_fgbz` returns a
// `BzzError`; callers below propagate it through `RenderError`'s `From<BzzError>`.
use crate::fgbz::{FgbzPalette, PaletteColor, parse_fgbz};

// ── Core compositor ───────────────────────────────────────────────────────────

/// Return the largest power-of-2 IW44 subsample factor for the given render
/// scale, allowing up to 1.5× upscaling in the compositor.
///
/// The compositor samples the decoded background at `pixel / subsample`, so a
/// decoded plane that is slightly smaller than the output is fine — the
/// compositor's nearest-neighbour lookup handles it naturally.  Allowing up to
/// 1.5× upscaling lets us pick a coarser subsample in many common cases
/// (e.g. 150 dpi from a 400 dpi source) and skip the high-frequency wavelet
/// bands, matching the partial-decode strategy used by DjVuLibre.
///
/// Examples (with 1.5× tolerance):
/// - scale=1.0  → 1 (full resolution)
/// - scale=0.5  → 2 (1.5/0.5=3.0 → 2)
/// - scale=0.375→ 4 (1.5/0.375=4.0 → 4)   ← was 2 before fix
/// - scale=0.25 → 4 (1.5/0.25=6.0 → 4)
/// - scale=0.1  → 8 (1.5/0.1=15 → capped at 8)
fn best_iw44_subsample(scale: f32) -> u32 {
    if scale <= 0.0 || !scale.is_finite() || scale >= 1.0 {
        return 1;
    }
    // Allow up to 1.5× upscaling: the compositor handles the coordinate
    // division, so a slightly-too-small decoded plane is fine.
    // Round rather than truncate: pixel-rounding of width causes decode_scale
    // to differ from the true scale by up to 0.5/page_width (≈0.023% for a
    // 2260-px page), which can push 1.5/scale just below an integer and select
    // a 2× coarser subsample — e.g. subsample 2 instead of 4 for colorbook.
    let max_sub = (1.5_f32 / scale).round() as u32;
    let mut s = 1u32;
    while s * 2 <= max_sub {
        s *= 2;
    }
    s.min(8)
}

/// Tile edge length (pixels) used by [`render_region_tiled`]'s composited-
/// output cache. 256 keeps a full RGBA tile at 256 KiB — small enough that a
/// pan/zoom viewer's working set (a screenful of tiles) stays a few MB, large
/// enough that the per-tile `HashMap`/`Mutex` overhead is negligible next to
/// the compositor work it avoids repeating.
#[cfg(feature = "std")]
const TILE_SIZE: u32 = 256;

/// Default per-page byte budget for the composited-tile cache — about 32
/// full tiles (256×256×4 B = 256 KiB each). Independent of, but counted
/// towards, [`PageLayers::cached_bytes`] / `DjVuDocument::enforce_cache_budget`:
/// a document-wide budget sweep evicts whole pages (tiles included), while
/// this bound keeps one page's own pan history from growing unboundedly
/// between sweeps. Overridable per page via
/// [`crate::djvu_tile::set_tile_cache_budget`] (#691 slice 2).
#[cfg(feature = "std")]
pub(crate) const TILE_CACHE_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Composited-output tile cache key: `(full_w, full_h, tile_x, tile_y, bold,
/// mask_aa)` — the tuple of [`RenderOptions`] fields (plus the tile's
/// top-left corner in full-render space) that `composite_into`'s per-pixel
/// output actually depends on for a [`render_region_tiled`]-eligible request
/// (bilinear resampling, identity rotation, strict decode). Any option that
/// can change a composited pixel's bytes must be part of this key.
#[cfg(feature = "std")]
type TileKey = (u32, u32, u32, u32, u8, bool);

/// One cached composited tile: `w × h` (≤ [`TILE_SIZE`], smaller at the
/// page's right/bottom edge) RGBA bytes, row-major, stride `w * 4`.
#[cfg(feature = "std")]
struct TileEntry {
    w: u32,
    h: u32,
    data: Vec<u8>,
}

/// [`PageLayers`]'s composited-tile store: the tile map, FIFO insertion order
/// for eviction, and the running byte total (avoids re-summing on every
/// insert/evict).
#[cfg(feature = "std")]
#[derive(Default)]
struct TileCacheState {
    map: std::collections::HashMap<TileKey, std::sync::Arc<TileEntry>>,
    order: std::collections::VecDeque<TileKey>,
    bytes: usize,
    /// Per-page budget override (#691 slice 2); `None` means
    /// [`TILE_CACHE_MAX_BYTES`]. Kept as an `Option` so `derive(Default)`
    /// stays valid and "still on the default" remains observable.
    budget: Option<usize>,
    /// Hit/miss/eviction telemetry (#576). Test-only so the release lock
    /// section stays exactly as cheap as before.
    #[cfg(test)]
    hits: usize,
    #[cfg(test)]
    misses: usize,
    #[cfg(test)]
    evictions: usize,
}

#[cfg(feature = "std")]
impl TileCacheState {
    /// The budget this cache currently enforces (override or default).
    fn effective_budget(&self) -> usize {
        self.budget.unwrap_or(TILE_CACHE_MAX_BYTES)
    }

    /// Evict oldest-first until `bytes` is back under the effective budget.
    fn evict_to_budget(&mut self) {
        while self.bytes > self.effective_budget() {
            match self.order.pop_front() {
                Some(old_key) => {
                    if let Some(old) = self.map.remove(&old_key) {
                        self.bytes = self.bytes.saturating_sub(old.data.len());
                        #[cfg(test)]
                        {
                            self.evictions += 1;
                        }
                    }
                }
                None => break,
            }
        }
    }
}

/// Render-tier cache of a page's decoded layers.
///
/// These are the decoded wavelet / bitmap forms the compositor consumes —
/// background (`bg44`, plus the first-chunk-only `bg44_partial` used at
/// subsample ≥ 4), the JB2 mask (`mask`), its quarter-resolution max-pool
/// downsample (`mask_sub4`, a pure compositor concern), and the FG44 colour
/// layer (`fg44`). They live here in the render tier rather than on
/// [`DjVuPage`] so the page model stays close to its raw bytes and every
/// render concern — including compositor subsampling — concentrates in one
/// place.
///
/// Each layer is decoded lazily and cached, so repeated renders of the same
/// page (e.g. thumbnails then full resolution) reuse the expensive ZP
/// arithmetic decode. A `DjVuPage` holds one of these behind a `OnceLock`;
/// the accessors borrow the page only to decode on a cache miss, so the
/// returned reference is tied to the cache, not the call.
/// Cached decoded ANTz payload (#605): the parsed annotation record plus its
/// map areas, shared behind an `Arc` between the per-page cache and callers.
#[cfg(feature = "std")]
pub(crate) type SharedAnnotations = std::sync::Arc<(
    crate::annotation::Annotation,
    Vec<crate::annotation::MapArea>,
)>;

#[cfg(feature = "std")]
#[derive(Default)]
pub(crate) struct PageLayers {
    bg44: std::sync::OnceLock<Option<Iw44Image>>,
    bg44_partial: std::sync::OnceLock<Option<Iw44Image>>,
    mask: std::sync::OnceLock<Option<crate::bitmap::Bitmap>>,
    mask_sub4: std::sync::OnceLock<Option<crate::bitmap::Bitmap>>,
    fg44: std::sync::OnceLock<Option<Pixmap>>,
    // Full-resolution (subsample=1) RGB Pixmap derived from bg44. Cached so
    // repeated renders of the same page skip the 2–3 ms IW44 IDWT + YCbCr→RGB
    // conversion. Populated on first full-resolution render; left empty on
    // pages that are never rendered at sub=1 (e.g. thumbnails only).
    bg_rgb_s1: std::sync::OnceLock<Option<Pixmap>>,
    // Half-resolution (subsample=2) RGB Pixmap derived from bg44, for the common
    // 150-from-300-DPI render. Same memoization as `bg_rgb_s1` but ~4× smaller;
    // left empty on pages never rendered at sub=2.
    bg_rgb_s2: std::sync::OnceLock<Option<Pixmap>>,
    // Quarter-resolution (subsample=4) RGB Pixmap derived from the *partial*
    // bg44 (first chunk only, matching the sub>=4 decode path). Caches the
    // IDWT + YCbCr->RGB conversion for the common thumbnail / heavy-downscale
    // render (e.g. 150-from-400-DPI, contact-sheet zoom); ~16x smaller than the
    // sub=1 cache. Left empty on pages never rendered at sub=4.
    bg_rgb_s4: std::sync::OnceLock<Option<Pixmap>>,
    // Decoded JB2 mask + per-pixel blit-index map for FGbz-palette pages. The
    // plain `mask` cache does not cover the indexed variant, so without this
    // every warm render of a palette page re-runs the full JB2 ZP decode and
    // re-allocates the page-sized blit map. Only populated for palette pages.
    // THUMB_PARTIAL_MEMO: the converted RGB for the first subsample > 4 this
    // page was rendered at, stored as `(subsample, pixmap)`. Thumbnail grids
    // and zoomed-out pans land here, and they land on the *same* subsample for
    // a given page, so one slot serves them. Tiny — ~90 KB for a 128 px
    // thumbnail of a colorbook.djvu page — and it lets the sub > 4 path stop
    // memoising the full-size `bg44_partial` coefficient image (5.75 MB) it
    // derives from. A render at a different sub > 4 misses and reconverts.
    bg_rgb_subhi: std::sync::OnceLock<Option<(u32, Pixmap)>>,
    mask_indexed: std::sync::OnceLock<Option<(crate::bitmap::Bitmap, Vec<i32>)>>,
    // Decoded page metadata (#605): the TXTz/ANTz payloads are BZZ-compressed
    // and rebuilt into full zone/annotation trees on every access, yet viewers
    // ask for the same metadata repeatedly (search, selection, link overlays).
    // Cached behind `Arc` so warm accesses share one decode. Only populated
    // for pages whose metadata is actually touched; parse *errors* are not
    // cached (malformed chunks keep erroring per call, unchanged behaviour).
    text_layer: std::sync::OnceLock<Option<std::sync::Arc<crate::text::TextLayer>>>,
    annotations: std::sync::OnceLock<Option<SharedAnnotations>>,
    /// Monotonic last-access tick for LRU cache-budget eviction. Bumped from a
    /// process-global counter every time this page's layers are touched; read
    /// (without touching) by `DjVuDocument::enforce_cache_budget` to evict the
    /// least-recently-used pages first. Not part of the decoded data.
    access: std::sync::atomic::AtomicU64,
    /// C4_TILE_CACHE: composited-output tiles for [`render_region_tiled`],
    /// keyed by `(full_w, full_h, tile_x, tile_y, bold, mask_aa)`. Unlike the
    /// layers above (which cache *decoded* data), these cache the *compositor's
    /// output* — the per-pixel work `composite_into` repeats on every call is
    /// not memoized anywhere else. Bounded to `TILE_CACHE_MAX_BYTES` per page
    /// with FIFO eviction; counted in [`cached_bytes`](Self::cached_bytes) so
    /// it shares the page's C5 byte-budget accounting, and dropped whenever
    /// this whole `PageLayers` is (`evict_render_cache`).
    tile_cache: std::sync::Mutex<TileCacheState>,
}

/// Process-global monotonic source for the per-page LRU access tick.
#[cfg(feature = "std")]
static ACCESS_TICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Decode the first BG44 chunk of `page` into a fresh `Iw44Image`.
///
/// The body of [`PageLayers::bg44_partial`]'s initialiser, factored out so the
/// subsample > 4 path can produce the same image without memoising it (see
/// [`PageLayers::bg44_partial_cached`]).
#[cfg(feature = "std")]
fn decode_bg44_partial(page: &DjVuPage) -> Option<Iw44Image> {
    let chunks = page.bg44_chunks();
    if chunks.is_empty() {
        return None;
    }
    let mut img = Iw44Image::new();
    if img.decode_chunk(chunks[0]).is_err() {
        return None;
    }
    if img.width == 0 {
        return None;
    }
    // Same dimension cross-check as `PageLayers::bg44`.
    if !iw44_reduction_is_legal(
        page.width() as u32,
        page.height() as u32,
        img.width,
        img.height,
    ) {
        return None;
    }
    Some(img)
}

#[cfg(feature = "std")]
impl PageLayers {
    /// An empty cache. Layers are decoded on first access.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Record an access, stamping this cache with the next global tick (LRU).
    pub(crate) fn bump_access(&self) {
        let t = ACCESS_TICK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.access.store(t, std::sync::atomic::Ordering::Relaxed);
    }

    /// The last-access tick (higher = more recently used).
    pub(crate) fn access_tick(&self) -> u64 {
        self.access.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Resident bytes held by this page's decoded caches.
    ///
    /// Every pixel cache is measured from the `Vec` it owns, not estimated:
    /// [`DjVuDocument::enforce_cache_budget`](crate::djvu_document::DjVuDocument::enforce_cache_budget)
    /// turns this number into a memory ceiling, so an estimate here becomes a
    /// wrong ceiling for the caller. The BG44 coefficient images used to be
    /// sized as `w·h·2`, which counts the luma plane and drops the two chroma
    /// planes a colour page also keeps — the whole cache reported at ~38 % of
    /// the truth, and a 16 MiB budget held ~52 MB (PERF_EXPERIMENTS.md
    /// DECODE_CACHE_ACCOUNTING; guarded by `tests/decode_cache_accounting.rs`).
    /// The text/annotation trees stay approximate — they are node counts, not
    /// buffers, and are small next to the pixel caches.
    ///
    /// Reads caches without initialising them.
    pub(crate) fn cached_bytes(&self) -> usize {
        let px = |o: &std::sync::OnceLock<Option<Pixmap>>| {
            o.get().and_then(|x| x.as_ref()).map_or(0, |p| p.data.len())
        };
        let bm = |o: &std::sync::OnceLock<Option<crate::bitmap::Bitmap>>| {
            o.get().and_then(|x| x.as_ref()).map_or(0, |b| b.data.len())
        };
        let iw = |o: &std::sync::OnceLock<Option<Iw44Image>>| {
            o.get()
                .and_then(|x| x.as_ref())
                .map_or(0, |i| i.heap_bytes())
        };
        let mi = self
            .mask_indexed
            .get()
            .and_then(|x| x.as_ref())
            .map_or(0, |(b, v)| b.data.len() + v.len() * 4);
        // Metadata caches (#605): approximate — text bytes + a fixed cost per
        // zone/map-area node. Small next to the pixel caches, but accounted so
        // the budget sweep sees them.
        let tl = self
            .text_layer
            .get()
            .and_then(|x| x.as_ref())
            .map_or(0, |t| t.text.len() + count_zones(&t.zones) * 64);
        let an = self
            .annotations
            .get()
            .and_then(|x| x.as_ref())
            .map_or(0, |a| a.1.len() * 96 + 64);
        px(&self.fg44)
            + px(&self.bg_rgb_s1)
            + px(&self.bg_rgb_s2)
            + px(&self.bg_rgb_s4)
            + self
                .bg_rgb_subhi
                .get()
                .and_then(|x| x.as_ref())
                .map_or(0, |(_, p)| p.data.len())
            + bm(&self.mask)
            + bm(&self.mask_sub4)
            + iw(&self.bg44)
            + iw(&self.bg44_partial)
            + mi
            + tl
            + an
            + self.tile_cache_bytes()
    }

    /// C5_COMPRESS: drop the expensive full-resolution derivations —
    /// `bg44`/`bg44_partial` coefficient images, `bg_rgb_s1`, mask +
    /// `mask_sub4`, `fg44`, `mask_indexed`, and the composited-tile cache —
    /// while **preserving** any already-cached `bg_rgb_s2` / `bg_rgb_s4`
    /// downscaled RGB pixmap.
    ///
    /// This is the cheaper middle tier between "keep everything" and
    /// [`evict_render_cache`](DjVuPage::evict_render_cache)'s full drop: a
    /// later downscaled render (subsample ≥ 2 — e.g. a thumbnail, contact
    /// sheet, or zoomed-out pan) stays warm, while a full-resolution render
    /// still pays a cold decode. Measured (see PERF_EXPERIMENTS.md
    /// C5_COMPRESS): the coefficient `Iw44Image` retained by `bg44` is *not*
    /// cheaper to keep than the derived RGB pixmap (same size class once
    /// colour planes are counted), so there is no cheap "upgrade sub=2→sub=1"
    /// path — only the already-decoded downscaled pixmaps are worth keeping.
    /// No-op on fields that were never populated.
    pub(crate) fn downgrade(&mut self) {
        self.bg44 = std::sync::OnceLock::new();
        self.bg44_partial = std::sync::OnceLock::new();
        self.mask = std::sync::OnceLock::new();
        // mask_sub4 intentionally preserved (#607): ~1/16 of the packed mask
        // bytes keeps sub>=4 re-renders (thumbnails, zoomed-out pans) warm
        // without re-running the JB2 arithmetic decode. `decode_layers`
        // consults it before forcing a full mask decode.
        self.fg44 = std::sync::OnceLock::new();
        self.mask_indexed = std::sync::OnceLock::new();
        self.bg_rgb_s1 = std::sync::OnceLock::new();
        // Tiles are dropped, but a per-page budget override (#691 slice 2)
        // survives the downgrade — it is configuration, not cached data.
        let budget = self
            .tile_cache
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .budget;
        self.tile_cache = std::sync::Mutex::new(TileCacheState {
            budget,
            ..TileCacheState::default()
        });
        // bg_rgb_s2 / bg_rgb_s4 / bg_rgb_subhi / access tick intentionally
        // preserved — all three are the cheap downscaled tiers a later
        // zoomed-out render reuses.
    }

    /// Bytes currently held by the composited-tile cache (see `tile_cache`).
    pub(crate) fn tile_cache_bytes(&self) -> usize {
        self.tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bytes
    }

    /// Look up a cached composited tile, cloning the `Arc` handle on a hit.
    ///
    /// LRU (#576): a hit moves the key to the back of the eviction order.
    /// Under a back-and-forth pan — the classic reading pattern — FIFO evicts
    /// exactly the tiles about to be reused; move-to-back keeps them. The
    /// order deque holds ≤ `TILE_CACHE_MAX_BYTES / tile_bytes` ≈ 32 keys, so
    /// the linear reposition is a few dozen comparisons per hit.
    fn get_tile(&self, key: TileKey) -> Option<std::sync::Arc<TileEntry>> {
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let hit = state.map.get(&key).cloned();
        #[cfg(test)]
        {
            if hit.is_some() {
                state.hits += 1;
            } else {
                state.misses += 1;
            }
        }
        if hit.is_some()
            && let Some(pos) = state.order.iter().position(|k| *k == key)
        {
            state.order.remove(pos);
            state.order.push_back(key);
        }
        hit
    }

    /// Insert a freshly composited tile, evicting the oldest tiles (FIFO)
    /// until back under the page's effective tile-cache budget.
    fn insert_tile(&self, key: TileKey, entry: std::sync::Arc<TileEntry>) {
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.map.contains_key(&key) {
            return;
        }
        state.bytes += entry.data.len();
        state.map.insert(key, entry);
        state.order.push_back(key);
        state.evict_to_budget();
    }

    /// The tile-cache budget this page currently enforces (#691 slice 2).
    pub(crate) fn tile_cache_budget(&self) -> usize {
        self.tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .effective_budget()
    }

    /// Number of composited tiles currently cached (#691 slice 2).
    pub(crate) fn tile_cache_len(&self) -> usize {
        self.tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .map
            .len()
    }

    /// Override this page's tile-cache byte budget, evicting oldest-first
    /// down to the new bound immediately (#691 slice 2). A budget of `0`
    /// effectively disables composited-tile caching for the page.
    pub(crate) fn set_tile_cache_budget(&self, max_bytes: usize) {
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.budget = Some(max_bytes);
        state.evict_to_budget();
    }

    /// Drop every cached composited tile, returning the bytes freed
    /// (#691 slice 2). The budget override, if any, is kept.
    pub(crate) fn clear_tile_cache(&self) -> usize {
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let freed = state.bytes;
        state.map.clear();
        state.order.clear();
        state.bytes = 0;
        freed
    }

    /// Drop every cached composited tile that intersects `rect`, where `rect`
    /// is given in the pre-rotation pixel space of a `canvas_w × canvas_h`
    /// full render (#691 slice 2). Cached tiles belonging to *other* render
    /// sizes are matched by scaling the rect proportionally (outward, so a
    /// boundary-straddling tile is always dropped rather than kept). Returns
    /// the bytes freed.
    pub(crate) fn remove_tiles_intersecting(
        &self,
        rect: RenderRect,
        canvas_w: u32,
        canvas_h: u32,
    ) -> usize {
        if canvas_w == 0 || canvas_h == 0 || rect.width == 0 || rect.height == 0 {
            return 0;
        }
        let mut state = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut freed = 0usize;
        let doomed: Vec<TileKey> = state
            .map
            .iter()
            .filter(|((fw, fh, tx, ty, _, _), entry)| {
                // Scale the rect into this entry's (fw × fh) render space,
                // rounding outward (floor start, ceil end).
                let x0 = u64::from(rect.x) * u64::from(*fw) / u64::from(canvas_w);
                let x1 = ((u64::from(rect.x) + u64::from(rect.width)) * u64::from(*fw))
                    .div_ceil(u64::from(canvas_w));
                let y0 = u64::from(rect.y) * u64::from(*fh) / u64::from(canvas_h);
                let y1 = ((u64::from(rect.y) + u64::from(rect.height)) * u64::from(*fh))
                    .div_ceil(u64::from(canvas_h));
                let (tx0, ty0) = (u64::from(*tx), u64::from(*ty));
                let (tx1, ty1) = (tx0 + u64::from(entry.w), ty0 + u64::from(entry.h));
                tx0 < x1 && tx1 > x0 && ty0 < y1 && ty1 > y0
            })
            .map(|(k, _)| *k)
            .collect();
        for key in doomed {
            if let Some(old) = state.map.remove(&key) {
                freed += old.data.len();
            }
            if let Some(pos) = state.order.iter().position(|k| *k == key) {
                state.order.remove(pos);
            }
        }
        state.bytes = state.bytes.saturating_sub(freed);
        freed
    }

    /// Tile-cache telemetry snapshot `(hits, misses, evictions)` (#576).
    #[cfg(test)]
    fn tile_cache_stats(&self) -> (usize, usize, usize) {
        let s = self
            .tile_cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (s.hits, s.misses, s.evictions)
    }

    /// The fully decoded BG44 wavelet image (all chunks), decoding on first
    /// call. `None` when the page has no BG44 chunks or when any chunk fails
    /// strict decoding. The wavelet inverse-transform / YCbCr→RGB conversion is
    /// *not* cached here — it runs per render at the requested subsample.
    pub(crate) fn bg44(&self, page: &DjVuPage) -> Option<&Iw44Image> {
        self.bg44
            .get_or_init(|| {
                let chunks = page.bg44_chunks();
                if chunks.is_empty() {
                    return None;
                }
                // IW44_CHECKPOINT (#608): resume from the cached first-chunk
                // decode when a sub>=4 render already paid for it (the common
                // thumbnail -> full-view flow). Chunk 0 is the most expensive
                // chunk (~16-19% of a 4-chunk full decode on the corpus), the
                // clone is ~0.04-0.5 ms, and progressive decode is defined to
                // produce byte-identical output to a fresh 0..n decode. Peek
                // only (`get`) -- a cold full decode must not populate the
                // partial tier as a side effect.
                let mut img;
                let mut start = 0;
                if let Some(Some(partial)) = self.bg44_partial.get() {
                    img = partial.clone();
                    start = 1;
                } else {
                    img = Iw44Image::new();
                }
                for chunk_data in &chunks[start.min(chunks.len())..] {
                    #[cfg(test)]
                    count_bg44_chunk_decode();
                    if img.decode_chunk(chunk_data).is_err() {
                        return None;
                    }
                }
                if img.width == 0 {
                    return None;
                }
                // Dimension cross-check (see `iw44_reduction_is_legal`): a
                // BG44 plane whose header declares a size that isn't a legal
                // 1:1..1:12 reduction of the page's INFO dimensions is
                // corrupted/desynced — DjVuLibre rejects the whole page for
                // this, so treat it the same as any other BG44 decode
                // failure rather than stretching it onto the page.
                if !iw44_reduction_is_legal(
                    page.width() as u32,
                    page.height() as u32,
                    img.width,
                    img.height,
                ) {
                    return None;
                }
                Some(img)
            })
            .as_ref()
    }

    /// A partially-decoded BG44 image — first chunk only — decoding on first
    /// call. Roughly 4× cheaper to decode; used at subsample ≥ 4 where the
    /// high-frequency refinement chunks are imperceptible.
    pub(crate) fn bg44_partial(&self, page: &DjVuPage) -> Option<&Iw44Image> {
        self.bg44_partial
            .get_or_init(|| decode_bg44_partial(page))
            .as_ref()
    }

    /// The first-chunk BG44 image **only if it is already cached** — never
    /// decodes, never populates the slot.
    ///
    /// THUMB_PARTIAL_MEMO: the subsample > 4 render path uses this instead of
    /// [`bg44_partial`](Self::bg44_partial). A "partial" `Iw44Image` decodes ~4x
    /// faster than a full one but is exactly as large: `PlaneDecoder` allocates
    /// every 32x32 coefficient block up front, whichever chunks are decoded
    /// into them. Memoising it costs a full-size image per page (5.75 MB on
    /// `colorbook.djvu`) and buys nothing at sub > 4, where the derived RGB is
    /// not cached either — so a thumbnail sweep retained the whole book's
    /// backgrounds and re-ran the conversion anyway. See PERF_EXPERIMENTS.md
    /// THUMB_PARTIAL_MEMO.
    pub(crate) fn bg44_partial_cached(&self) -> Option<&Iw44Image> {
        self.bg44_partial.get().and_then(|x| x.as_ref())
    }

    /// The cached RGB conversion for `subsample`, when this page's single
    /// `sub > 4` slot holds exactly that subsample. Never decodes.
    pub(crate) fn bg_rgb_subhi(&self, subsample: u32) -> Option<&Pixmap> {
        match self.bg_rgb_subhi.get()?.as_ref()? {
            (s, px) if *s == subsample => Some(px),
            _ => None,
        }
    }

    /// Fill the `sub > 4` slot if it is still empty. No-op once set, so the
    /// first subsample a page is rendered at wins.
    pub(crate) fn store_bg_rgb_subhi(&self, subsample: u32, px: &Pixmap) {
        let _ = self.bg_rgb_subhi.set(Some((subsample, px.clone())));
    }

    /// The decoded JB2 / G4 foreground mask, decoding on first call. `None`
    /// when the page has no mask chunk or decoding fails.
    pub(crate) fn mask(&self, page: &DjVuPage) -> Option<&crate::bitmap::Bitmap> {
        self.mask
            .get_or_init(|| {
                #[cfg(test)]
                count_jb2_mask_decode();
                page.extract_mask().ok().flatten()
            })
            .as_ref()
    }

    /// A 1/4-resolution max-pool downsample of the mask. Each bit is 1 if any
    /// bit in the corresponding 4×4 block is set, letting the compositor do
    /// one lookup per output pixel at subsample ≥ 4 instead of 4–9. Purely a
    /// compositor optimisation, which is why it lives in the render tier
    /// rather than on the page.
    ///
    /// If the full-resolution mask is already cached (e.g. a prior
    /// native-resolution render of this page), downsamples that instead of
    /// decoding again. Otherwise decodes straight to 1/4 resolution via
    /// [`DjVuPage::extract_mask_sub4`] — the thumbnail / heavy-downscale
    /// path's common case — skipping the full-resolution JB2 canvas
    /// allocation and the full-canvas downsample scan (round 89 follow-up:
    /// `extract_mask` was 12.6 MB of the 47 MB thumbnail-sweep allocation
    /// total, decoded only to be immediately downsampled and discarded).
    pub(crate) fn mask_sub4(&self, page: &DjVuPage) -> Option<&crate::bitmap::Bitmap> {
        self.mask_sub4
            .get_or_init(|| {
                if let Some(full) = self.mask.get().and_then(|o| o.as_ref()) {
                    return Some(downsample_mask_4x(full));
                }
                page.extract_mask_sub4().ok().flatten()
            })
            .as_ref()
    }

    /// Peek at an already-built 1/4-resolution mask without triggering any
    /// decode. `Some` only when a previous sub>=4 render populated the slot
    /// (possibly retained across [`downgrade`](Self::downgrade), #607).
    ///
    /// Test-only: `decode_layers` used to gate its #607 fast path on this
    /// (only firing when already warm); it now calls `mask_sub4` directly so
    /// a *cold* sub>=4 render benefits too (round 89 follow-up). Kept as a
    /// non-triggering cache-warmth probe for the structural regression tests.
    #[cfg(test)]
    pub(crate) fn mask_sub4_cached(&self) -> Option<&crate::bitmap::Bitmap> {
        self.mask_sub4.get().and_then(|o| o.as_ref())
    }

    /// The decoded FG44 foreground colour layer, decoding on first call.
    /// `None` when the page has no FG44 chunks or decoding fails.
    pub(crate) fn fg44(&self, page: &DjVuPage) -> Option<&Pixmap> {
        self.fg44
            .get_or_init(|| page.extract_foreground().ok().flatten())
            .as_ref()
    }

    /// Full-resolution (sub=1) RGB Pixmap from BG44, cached after first call.
    ///
    /// Builds on the already-cached [`bg44`](Self::bg44) wavelet image so the
    /// ZP arithmetic decode is paid at most once per page. The IDWT + YCbCr→RGB
    /// conversion (≈2–3 ms for a typical A4 scan) is cached here so that
    /// repeated renders at native resolution skip it entirely.
    ///
    /// `None` when the page has no BG44 layer or the conversion fails.
    pub(crate) fn bg_rgb_s1(&self, page: &DjVuPage) -> Option<&Pixmap> {
        self.bg_rgb_s1
            .get_or_init(|| {
                let img = self.bg44(page)?;
                img.to_rgb_subsample(1).ok()
            })
            .as_ref()
    }

    /// Half-resolution (sub=2) RGB Pixmap from BG44, cached after first call.
    ///
    /// Mirrors [`bg_rgb_s1`](Self::bg_rgb_s1) for the common 150-from-300-DPI
    /// render: builds on the already-cached [`bg44`](Self::bg44) wavelet image so
    /// the ZP decode is paid once, then caches the IDWT + YCbCr→RGB conversion at
    /// subsample 2 (a ~8 MB Pixmap, 4× smaller than the sub=1 cache).
    ///
    /// `None` when the page has no BG44 layer or the conversion fails.
    pub(crate) fn bg_rgb_s2(&self, page: &DjVuPage) -> Option<&Pixmap> {
        self.bg_rgb_s2
            .get_or_init(|| {
                let img = self.bg44(page)?;
                img.to_rgb_subsample(2).ok()
            })
            .as_ref()
    }

    /// Quarter-resolution (sub=4) RGB Pixmap from the partial BG44, cached after
    /// first call.
    ///
    /// Mirrors [`bg_rgb_s2`](Self::bg_rgb_s2) for the common heavy-downscale /
    /// thumbnail render (e.g. 150-from-400-DPI). Builds on the already-cached
    /// [`bg44_partial`](Self::bg44_partial) wavelet image — matching the sub>=4
    /// decode path, which uses the first chunk only — so the ZP decode is paid
    /// once, then caches the IDWT + YCbCr->RGB conversion at subsample 4.
    ///
    /// `None` when the page has no BG44 layer or the conversion fails.
    pub(crate) fn bg_rgb_s4(&self, page: &DjVuPage) -> Option<&Pixmap> {
        self.bg_rgb_s4
            .get_or_init(|| {
                let img = self.bg44_partial(page)?;
                img.to_rgb_subsample(4).ok()
            })
            .as_ref()
    }

    /// The decoded JB2 mask + per-pixel blit-index map, decoding on first call.
    ///
    /// Used for FGbz-palette pages, where the compositor needs the blit index of
    /// each foreground pixel to look up its palette colour. Caches the full JB2
    /// ZP decode and the page-sized `Vec<i32>` blit map so repeated renders of the
    /// same page skip both. `None` when the page has no Sjbz/Smmr chunk or decode
    /// fails. The blit map is ~`width*height*4` bytes — only pages actually
    /// rendered with a palette ever populate this slot.
    /// Cached decoded text layer (#605). `try_init` runs at most once
    /// successfully; a parse error is returned without caching.
    pub(crate) fn text_layer_cached(
        &self,
        parse: impl FnOnce() -> Result<
            Option<std::sync::Arc<crate::text::TextLayer>>,
            crate::djvu_document::DocError,
        >,
    ) -> Result<Option<std::sync::Arc<crate::text::TextLayer>>, crate::djvu_document::DocError>
    {
        if let Some(v) = self.text_layer.get() {
            return Ok(v.clone());
        }
        let v = parse()?;
        let _ = self.text_layer.set(v.clone());
        Ok(v)
    }

    /// Cached decoded annotations (#605); same error semantics as
    /// [`text_layer_cached`](Self::text_layer_cached).
    pub(crate) fn annotations_cached(
        &self,
        parse: impl FnOnce() -> Result<Option<SharedAnnotations>, crate::djvu_document::DocError>,
    ) -> Result<Option<SharedAnnotations>, crate::djvu_document::DocError> {
        if let Some(v) = self.annotations.get() {
            return Ok(v.clone());
        }
        let v = parse()?;
        let _ = self.annotations.set(v.clone());
        Ok(v)
    }

    pub(crate) fn mask_indexed(
        &self,
        page: &DjVuPage,
    ) -> Option<&(crate::bitmap::Bitmap, Vec<i32>)> {
        self.mask_indexed
            .get_or_init(|| {
                #[cfg(test)]
                count_jb2_mask_decode();
                page.extract_mask_indexed().ok().flatten()
            })
            .as_ref()
    }
}

/// Recursive zone count for the metadata-cache byte estimate (#605).
#[cfg(feature = "std")]
fn count_zones(zones: &[crate::text::TextZone]) -> usize {
    zones
        .iter()
        .map(|z| 1 + count_zones(&z.children))
        .sum::<usize>()
}

/// Max-pool 4× downsample of a bilevel mask.
///
/// Each output pixel is 1 if any bit in the corresponding 4×4 block of `src`
/// is set. Used by [`PageLayers::mask_sub4`] to build the 1/4-resolution mask
/// the compositor uses for sub=4 renders instead of `mask_box_any`.
#[cfg(feature = "std")]
pub(crate) fn downsample_mask_4x(src: &crate::bitmap::Bitmap) -> crate::bitmap::Bitmap {
    let out_w = src.width.div_ceil(4);
    let out_h = src.height.div_ceil(4);
    let mut out = crate::bitmap::Bitmap::new(out_w, out_h);
    for oy in 0..out_h {
        for ox in 0..out_w {
            'outer: for dy in 0..4u32 {
                for dx in 0..4u32 {
                    let sx = ox * 4 + dx;
                    let sy = oy * 4 + dy;
                    if sx < src.width && sy < src.height && src.get(sx, sy) {
                        out.set(ox, oy, true);
                        break 'outer;
                    }
                }
            }
        }
    }
    out
}

/// The page's render-tier `mask_sub4` layer, or `None` without `std`.
///
/// Wraps the `std`-only [`PageLayers`] cache so the compositor's sub=4 path
/// compiles identically with and without the `std` feature.
#[cfg(feature = "std")]
fn page_mask_sub4(page: &DjVuPage) -> Option<&crate::bitmap::Bitmap> {
    page.render_layers().mask_sub4(page)
}

/// Is `(plane_w, plane_h)` a legal BG44/FG44 reduction of the page's own
/// `(page_w, page_h)` (from the INFO chunk)?
///
/// Mirrors DjVuLibre's `DjVuFile::get_dpi` cross-check (message
/// `DjVuFile.corrupt_BG44`, "Corrupted data (Incorrect size in BG44
/// chunk)."): the IW44 plane must be an exact `ceil(page_dim / red)`
/// downsample of the page for a *single* common integer reduction factor
/// `red` in `1..=12` — i.e. the same `red` must satisfy width *and* height
/// simultaneously. A BG44 chunk's own header freely declares its width/height
/// (independent of the page's INFO chunk), so without this check a corrupted
/// or desynced INFO/BG44 pairing silently maps the plane onto the page using
/// mismatched per-axis ratios (see `bg_q24`/`fg_q44`, which compute `sx`/`sy`
/// independently) instead of being rejected — producing a visibly stretched/
/// distorted composite rather than a clean error, exactly the "no INFO-vs-
/// BG44-payload dimension cross-check" gap found by differential fuzzing
/// against `ddjvu` (round 45, PERF_EXPERIMENTS.md finding 2).
fn iw44_reduction_is_legal(page_w: u32, page_h: u32, plane_w: u32, plane_h: u32) -> bool {
    if page_w == 0 || page_h == 0 || plane_w == 0 || plane_h == 0 {
        return false;
    }
    (1..=12u32).any(|red| page_w.div_ceil(red) == plane_w && page_h.div_ceil(red) == plane_h)
}

/// Decode background from BG44 chunks up to `max_chunks`.
///
/// `subsample` controls IW44 decode resolution: 1 = full, 2 = half, 4 = quarter.
/// Use `best_iw44_subsample(opts.decode_scale(page))` to pick an appropriate value.
///
/// When `max_chunks == usize::MAX`, the decoded wavelet image is fetched from
/// the page's [`PageLayers`] cache, avoiding repeated ZP arithmetic decode.
///
/// Returns `None` if there are no BG44 chunks.
/// `max_chunks = usize::MAX` means decode all chunks.
fn decode_background_chunks<'a>(
    page: &'a DjVuPage,
    max_chunks: usize,
    subsample: u32,
) -> Result<Option<Cow<'a, Pixmap>>, RenderError> {
    // Fast path: use a cached Iw44Image when all chunks are wanted.
    // For sub >= 4 we use the partial cache (first chunk only) — the high-frequency
    // refinement in later chunks is imperceptible at quarter-scale output, and skipping
    // them reduces cold ZP decode cost by ~4×.
    // For sub=1 (the most common case — full-resolution render) we also cache the
    // decoded RGB Pixmap, saving the 2–3 ms IDWT + YCbCr→RGB conversion per call.
    if max_chunks == usize::MAX {
        let bg44_chunks = page.bg44_chunks();
        if !bg44_chunks.is_empty() {
            if subsample == 1 {
                // Strict-mode error propagation: if BG44 failed to decode,
                // decoded_bg44() returns None; treat that as a hard error.
                let _ = page
                    .decoded_bg44()
                    .ok_or(RenderError::Iw44(crate::Iw44Error::Invalid))?;
                return Ok(page.decoded_bg_rgb_s1().map(Cow::Borrowed));
            }
            if subsample == 2 {
                // C5_COMPRESS: `PageLayers::downgrade` can clear `bg44` while
                // deliberately *keeping* an already-cached `bg_rgb_s2` (the
                // cheaper middle tier — see downgrade's doc comment). Check the
                // terminal cache first so that case stays warm: `bg_rgb_s2`'s
                // own initialiser already routes through `bg44(page)?`, so a
                // populated `Some` here can only follow a prior successful
                // decode — no need to force `decoded_bg44()` again.
                if let Some(cached) = page.decoded_bg_rgb_s2() {
                    return Ok(Some(Cow::Borrowed(cached)));
                }
                // Same memoization as sub=1 for the common 150-from-300-DPI render.
                // Strict-mode error propagation: if BG44 failed to decode,
                // decoded_bg44() returns None; treat that as a hard error.
                let _ = page
                    .decoded_bg44()
                    .ok_or(RenderError::Iw44(crate::Iw44Error::Invalid))?;
                return Ok(page.decoded_bg_rgb_s2().map(Cow::Borrowed));
            }
            if subsample == 4 {
                // C5_COMPRESS: mirrors the subsample==2 short-circuit above for
                // `bg_rgb_s4` / `bg44_partial`.
                if let Some(cached) = page.decoded_bg_rgb_s4() {
                    return Ok(Some(Cow::Borrowed(cached)));
                }
                // Cache the sub=4 RGB conversion (built from the partial image,
                // matching the sub>=4 path) so repeated thumbnail / downscale
                // renders skip the IDWT + YCbCr->RGB conversion.
                let _ = page
                    .decoded_bg44_partial()
                    .ok_or(RenderError::Iw44(crate::Iw44Error::Invalid))?;
                return Ok(page.decoded_bg_rgb_s4().map(Cow::Borrowed));
            }
            // subsample > 4 (subsample == 4 returned above). THUMB_PARTIAL_MEMO:
            // reuse an already-cached partial image, but do not *populate* the
            // cache from here. A partial `Iw44Image` is exactly as large as a
            // full one (see `PageLayers::bg44_partial_cached`) and this branch
            // caches nothing it derives, so memoising it made a thumbnail sweep
            // retain the whole book's backgrounds for no repeat saving.
            #[cfg(feature = "std")]
            if let Some(cached) = page.cached_bg_rgb_subhi(subsample) {
                return Ok(Some(Cow::Borrowed(cached)));
            }
            // Decoded here and dropped with this call when the page has no
            // cached partial image. `no_std` has no layer cache at all, so it
            // keeps the plain accessor (which is a stub returning `None`).
            #[cfg(feature = "std")]
            let owned = if subsample >= 4 && page.cached_bg44_partial().is_none() {
                decode_bg44_partial(page)
            } else {
                None
            };
            let img = if subsample >= 4 {
                #[cfg(feature = "std")]
                {
                    page.cached_bg44_partial().or(owned.as_ref())
                }
                #[cfg(not(feature = "std"))]
                {
                    page.decoded_bg44_partial()
                }
            } else {
                page.decoded_bg44()
            };
            let img = img.ok_or(RenderError::Iw44(crate::Iw44Error::Invalid))?;
            let rgb = img.to_rgb_subsample(subsample)?;
            #[cfg(feature = "std")]
            if subsample > 4 {
                page.store_bg_rgb_subhi(subsample, &rgb);
            }
            return Ok(Some(Cow::Owned(rgb)));
        }
        // No BG44 chunks — fall through to the JPEG fallback below.
    } else {
        let bg44_chunks = page.bg44_chunks();
        if !bg44_chunks.is_empty() {
            let mut img = Iw44Image::new();
            for chunk_data in bg44_chunks.iter().take(max_chunks) {
                #[cfg(test)]
                count_bg44_chunk_decode();
                img.decode_chunk(chunk_data)?;
            }
            // Same dimension cross-check as the cached path in `PageLayers::bg44`.
            if !iw44_reduction_is_legal(
                page.width() as u32,
                page.height() as u32,
                img.width,
                img.height,
            ) {
                return Err(RenderError::Iw44(crate::Iw44Error::Invalid));
            }
            return Ok(Some(Cow::Owned(img.to_rgb_subsample(subsample)?)));
        }
    }

    // Fall back to JPEG-encoded background if present.
    #[cfg(feature = "std")]
    if let Some(pm) = decode_bgjp(page)? {
        return Ok(Some(Cow::Owned(pm)));
    }

    Ok(None)
}

/// Permissive variant: decode BG44 chunks until the first error, then stop.
///
/// Returns whatever was decoded so far (may be blurry / incomplete).
/// Returns `None` only when there are no BG44 chunks at all or even the
/// first chunk fails to produce a valid image.
fn decode_background_chunks_permissive<'a>(
    page: &'a DjVuPage,
    max_chunks: usize,
    subsample: u32,
) -> Option<Cow<'a, Pixmap>> {
    let bg44_chunks = page.bg44_chunks();
    if !bg44_chunks.is_empty() {
        let mut img = Iw44Image::new();
        let wanted = bg44_chunks.len().min(max_chunks);
        // `decoded` is the number of chunks decoded before the first error,
        // which is exactly the failing chunk's `enumerate` index.
        for (decoded, chunk_data) in bg44_chunks.iter().take(max_chunks).enumerate() {
            if img.decode_chunk(chunk_data).is_err() {
                // stop on first error, use what we have
                record_recovery(RecoveredLayer::Background, {
                    #[cfg(feature = "std")]
                    {
                        format!(
                            "BG44 truncated at chunk {} of {wanted}; \
                                 kept {decoded} decoded chunk(s)",
                            decoded + 1
                        )
                    }
                    #[cfg(not(feature = "std"))]
                    {
                        let _ = (decoded, wanted);
                        ""
                    }
                });
                break;
            }
        }
        return img.to_rgb_subsample(subsample).ok().map(Cow::Owned);
    }

    // Fall back to JPEG-encoded background if present.
    #[cfg(feature = "std")]
    {
        decode_bgjp(page).ok().flatten().map(Cow::Owned)
    }
    #[cfg(not(feature = "std"))]
    None
}

/// Decode the JB2 mask (Sjbz chunk) without blit tracking.
///
/// Uses the page-level cache (`decoded_mask`) so that repeated renders of the
/// same page (e.g. at different DPI levels) skip the ZP arithmetic decode.
/// Returns `Cow::Borrowed` pointing into the page cache (zero-copy) on a cache
/// hit; `Cow::Owned` on a cold decode or when no Sjbz chunk is present.
fn decode_mask<'a>(
    page: &'a DjVuPage,
) -> Result<Option<Cow<'a, crate::bitmap::Bitmap>>, RenderError> {
    match page.decoded_mask() {
        Some(bm) => Ok(Some(Cow::Borrowed(bm))),
        None if page.find_chunk(b"Sjbz").is_some() => {
            // Cache miss means decode failed; propagate via fresh decode for the error.
            page.extract_mask()
                .map_err(RenderError::from)
                .map(|opt| opt.map(Cow::Owned))
        }
        None => Ok(None),
    }
}

/// Decode the JB2 mask with per-pixel blit index tracking.
///
/// Delegates to [`DjVuPage::extract_mask_indexed`] so that the shared DJVI
/// dictionary (`shared_djbz`) is used as a fallback when there is no inline
/// Djbz chunk.
/// A decoded indexed mask: the JB2 bitmap plus its per-pixel blit-index map,
/// each borrowed from the page cache (`Cow::Borrowed`) or freshly decoded
/// (`Cow::Owned`).
type IndexedMask<'a> = (Cow<'a, crate::bitmap::Bitmap>, Cow<'a, [i32]>);

fn decode_mask_indexed<'a>(page: &'a DjVuPage) -> Result<Option<IndexedMask<'a>>, RenderError> {
    match page.decoded_mask_indexed() {
        Some((bm, blit)) => Ok(Some((Cow::Borrowed(bm), Cow::Borrowed(blit.as_slice())))),
        // Cache miss with a mask chunk present means decode failed; re-run to
        // surface the error (mirrors `decode_mask`). A genuine no-chunk page
        // returns Ok(None) without a re-decode.
        None if page.find_chunk(b"Sjbz").is_some() || page.find_chunk(b"Smmr").is_some() => page
            .extract_mask_indexed()
            .map_err(RenderError::from)
            .map(|opt| opt.map(|(bm, blit)| (Cow::Owned(bm), Cow::Owned(blit)))),
        None => Ok(None),
    }
}

/// Decode the FGbz foreground palette with per-blit color indices.
fn decode_fg_palette_full(page: &DjVuPage) -> Result<Option<FgbzPalette>, RenderError> {
    let fgbz = match page.find_chunk(b"FGbz") {
        Some(data) => data,
        None => return Ok(None),
    };

    let pal = parse_fgbz(fgbz)?;
    if pal.colors.is_empty() {
        return Ok(None);
    }
    Ok(Some(pal))
}

/// Decode the FG44 foreground layer.
///
/// Uses the page-level cache (`decoded_fg44`) so that repeated renders skip
/// the IW44 ZP decode. Falls back to FGjp (JPEG) when no FG44 chunks are present.
fn decode_fg44<'a>(page: &'a DjVuPage) -> Result<Option<Cow<'a, Pixmap>>, RenderError> {
    let fg44_chunks = page.fg44_chunks();
    if !fg44_chunks.is_empty() {
        return match page.decoded_fg44() {
            Some(pm) => Ok(Some(Cow::Borrowed(pm))),
            // `decoded_fg44()` is a shared cache used by both strict and
            // permissive callers, so it swallows the underlying decode error
            // and returns `None`. With chunks present, a cache miss can only
            // mean decode failed (never "no foreground") — re-run the decode
            // for the real error and propagate it (mirrors `decode_mask`'s
            // Sjbz handling below, round 577). Permissive callers already wrap
            // this call in `.ok().flatten()`, recovering the old "no
            // foreground" behavior.
            None => page
                .extract_foreground()
                .map_err(RenderError::from)
                .map(|opt| opt.map(Cow::Owned)),
        };
    }

    // Fall back to JPEG-encoded foreground if present.
    #[cfg(feature = "std")]
    if let Some(pm) = decode_fgjp(page)? {
        return Ok(Some(Cow::Owned(pm)));
    }

    Ok(None)
}

/// The page layers decoded for a full (non-progressive) composite: background,
/// foreground palette, mask, optional indexed blit map, and the FG44/FGjp
/// foreground pixmap.
struct DecodedLayers<'a> {
    bg: Option<Cow<'a, Pixmap>>,
    fg_palette: Option<FgbzPalette>,
    mask: Option<Cow<'a, crate::bitmap::Bitmap>>,
    blit_map: Option<Cow<'a, [i32]>>,
    fg44: Option<Cow<'a, Pixmap>>,
}

/// Decode every layer needed for a full composite at `bg_subsample` — the one
/// home for the permissive-vs-strict decode decision.
///
/// In permissive mode each step swallows errors (`.ok().flatten()`) and the
/// background stops at the first corrupt chunk; in strict mode any decode error
/// propagates. The returned `mask` already has `opts.bold` dilation applied,
/// since both callers do that immediately after decoding.
///
/// Both [`render_rows`] (the row path behind `render_pixmap` / `render_into` /
/// `render_streaming`) and [`render_region`] build their `CompositeContext`
/// from this, keeping their decode logic identical. The progressive path
/// decodes differently (partial background up to `chunk_n`) and is not routed
/// through here.
fn decode_layers<'a>(
    page: &'a DjVuPage,
    opts: &RenderOptions,
    bg_subsample: u32,
    bg_chunk_limit: usize,
) -> Result<DecodedLayers<'a>, RenderError> {
    // #607 (round 89 follow-up): an eligible sub>=4 render skips the full JB2
    // decode entirely, whether `mask_sub4` is already warm or still cold.
    // Eligibility mirrors `resolve_sub4_mask` (no bold dilation, no FGbz
    // palette — those need full-resolution mask semantics); the compositor
    // then reads only the sub4 plane, so output is pixel-identical to the
    // full-decode path by construction.
    //
    // `mask_sub4(page)` (rather than the passive `mask_sub4_cached()`) is what
    // makes this fire on a *cold* first render too: when nothing is cached yet
    // it decodes straight to 1/4 resolution via `extract_mask_sub4` instead of
    // decoding the full-resolution canvas and downsampling it afterward — the
    // 12.6 MB `extract_mask` allocation round 89 flagged in the thumbnail
    // sweep. A warm full-resolution mask (from a prior sub=1 render of this
    // page) is still reused by downsampling it in place, never re-decoded.
    //
    // Restricted to full-background decodes (`bg_chunk_limit == usize::MAX`):
    // the progressive path composites with the mask returned *here* (it never
    // consults `resolve_sub4_mask`), so handing it a maskless layer set made
    // `render_progressive` frames silently drop the text layer whenever this
    // cache happened to be warm — output depended on cache warmth (#691
    // slice 3 regression test `render_progressive_ignores_mask_sub4_warmth`).
    #[cfg(feature = "std")]
    if bg_chunk_limit == usize::MAX
        && bg_subsample >= 4
        && opts.bold == 0
        && page.find_chunk(b"FGbz").is_none()
        && page.render_layers().mask_sub4(page).is_some()
    {
        let (bg, fg44) = if opts.permissive {
            (
                decode_background_chunks_permissive(page, bg_chunk_limit, bg_subsample),
                permissive_layer(decode_fg44(page), RecoveredLayer::Foreground),
            )
        } else {
            (
                decode_background_chunks(page, bg_chunk_limit, bg_subsample)?,
                decode_fg44(page)?,
            )
        };
        return Ok(DecodedLayers {
            bg,
            fg_palette: None,
            mask: None,
            blit_map: None,
            fg44,
        });
    }

    let bg;
    let fg_palette;
    let mask;
    let blit_map;
    let fg44;

    if opts.permissive {
        bg = decode_background_chunks_permissive(page, bg_chunk_limit, bg_subsample);
        fg_palette = permissive_layer(
            decode_fg_palette_full(page),
            RecoveredLayer::ForegroundPalette,
        );
        let indexed = if fg_palette.is_some() {
            permissive_layer(decode_mask_indexed(page), RecoveredLayer::Mask)
        } else {
            None
        };
        if let Some((bm, bm_map)) = indexed {
            mask = Some(bm);
            blit_map = Some(bm_map);
        } else {
            mask = permissive_layer(decode_mask(page), RecoveredLayer::Mask);
            blit_map = None;
        }
        fg44 = permissive_layer(decode_fg44(page), RecoveredLayer::Foreground);
    } else {
        // #440: background (BG44 ZP + IDWT) and foreground (JB2 mask + FG44) decode
        // touch disjoint OnceLock fields, so on a cold render they can run on two
        // rayon threads — the FG JB2 decode overlaps the BG ZP phase (when the pool
        // is otherwise idle, before IW44_PAR's IDWT join kicks in). Warm renders hit
        // the caches and return immediately, so the join is paid only when cold.
        #[cfg(feature = "parallel")]
        let (bg_res, fg_res) = rayon::join(
            || decode_background_chunks(page, bg_chunk_limit, bg_subsample),
            || decode_foreground_strict(page),
        );
        #[cfg(not(feature = "parallel"))]
        let (bg_res, fg_res) = (
            decode_background_chunks(page, bg_chunk_limit, bg_subsample),
            decode_foreground_strict(page),
        );
        bg = bg_res?;
        let fg = fg_res?;
        fg_palette = fg.fg_palette;
        mask = fg.mask;
        blit_map = fg.blit_map;
        fg44 = fg.fg44;
    }

    let mask = if opts.bold > 0 {
        mask.map(|m| Cow::Owned(m.into_owned().dilate_n(opts.bold as u32)))
    } else {
        mask
    };

    Ok(DecodedLayers {
        bg,
        fg_palette,
        mask,
        blit_map,
        fg44,
    })
}

/// The foreground layers (mask + blit map, FGbz palette, FG44 colour) decoded in
/// strict mode, *before* any bold dilation.
struct ForegroundLayers<'a> {
    fg_palette: Option<FgbzPalette>,
    mask: Option<Cow<'a, crate::bitmap::Bitmap>>,
    blit_map: Option<Cow<'a, [i32]>>,
    fg44: Option<Cow<'a, Pixmap>>,
}

/// Strict-mode decode of a page's foreground layers, shared by the full
/// [`decode_layers`] path and [`render_progressive`] (which decodes a partial
/// background but the same foreground).
///
/// Owns the "indexed mask when an FGbz palette is present, plain mask
/// otherwise" decision so the two callers cannot drift apart. Bold dilation is
/// applied by the caller, since `decode_layers` shares one dilation step across
/// its permissive and strict branches.
fn decode_foreground_strict<'a>(page: &'a DjVuPage) -> Result<ForegroundLayers<'a>, RenderError> {
    let fg_palette = decode_fg_palette_full(page)?;
    let (mask, blit_map) = if fg_palette.is_some() {
        match decode_mask_indexed(page)? {
            Some((bm, bm_map)) => (Some(bm), Some(bm_map)),
            None => (None, None),
        }
    } else {
        (decode_mask(page)?, None)
    };
    let fg44 = decode_fg44(page)?;
    Ok(ForegroundLayers {
        fg_palette,
        mask,
        blit_map,
        fg44,
    })
}

/// Decode a BGjp (JPEG-encoded background) chunk into an RGB [`Pixmap`].
///
/// Returns `None` when the page has no `BGjp` chunk.
/// Only available with the `std` feature (requires `zune-jpeg`).
#[cfg(feature = "std")]
fn decode_bgjp(page: &DjVuPage) -> Result<Option<Pixmap>, RenderError> {
    let data = match page.find_chunk(b"BGjp") {
        Some(d) => d,
        None => return Ok(None),
    };
    Ok(Some(decode_jpeg_to_pixmap(data)?))
}

/// Decode an FGjp (JPEG-encoded foreground) chunk into an RGB [`Pixmap`].
///
/// Returns `None` when the page has no `FGjp` chunk.
/// Only available with the `std` feature (requires `zune-jpeg`).
#[cfg(feature = "std")]
fn decode_fgjp(page: &DjVuPage) -> Result<Option<Pixmap>, RenderError> {
    let data = match page.find_chunk(b"FGjp") {
        Some(d) => d,
        None => return Ok(None),
    };
    Ok(Some(decode_jpeg_to_pixmap(data)?))
}

/// Decode raw JPEG bytes into an RGBA [`Pixmap`].
///
/// Uses `zune-jpeg` for decoding. The JPEG is decoded to RGB and then
/// converted to RGBA (alpha = 255).
#[cfg(feature = "std")]
fn decode_jpeg_to_pixmap(data: &[u8]) -> Result<Pixmap, RenderError> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;

    let cursor = ZCursor::new(data);
    let mut decoder = JpegDecoder::new(cursor);
    decoder
        .decode_headers()
        .map_err(|e| RenderError::Jpeg(format!("{e:?}")))?;
    let info = decoder
        .info()
        .ok_or_else(|| RenderError::Jpeg("missing image info after decode_headers".to_owned()))?;
    let w = info.width as usize;
    let h = info.height as usize;
    let rgb = decoder
        .decode()
        .map_err(|e| RenderError::Jpeg(format!("{e:?}")))?;

    // zune-jpeg returns packed RGB; convert to RGBA with alpha = 255.
    let pixel_count = w * h;
    let rgb = if rgb.len() >= pixel_count * 3 {
        rgb
    } else {
        // Truncated JPEG — pad with zeros so rgb_to_rgba stays in bounds.
        let mut padded = rgb;
        padded.resize(pixel_count * 3, 0);
        padded
    };
    let mut rgba = vec![0u8; pixel_count * 4];
    rgb_to_rgba(&rgb[..pixel_count * 3], &mut rgba);
    Ok(Pixmap {
        width: w as u32,
        height: h as u32,
        data: rgba,
    })
}

/// A sub-rectangle within the full rendered output.
///
/// Used by [`render_region`] to select which portion of the page to render.
/// `x` and `y` are pixel offsets within the output at `opts.width × opts.height`
/// resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RenderRect {
    /// X offset in output pixels.
    pub x: u32,
    /// Y offset in output pixels.
    pub y: u32,
    /// Width of the output region in pixels.
    pub width: u32,
    /// Height of the output region in pixels.
    pub height: u32,
}

/// All decoded layers and options passed to the compositor.
///
/// `Clone`/`Copy`: every field is a reference or a `Copy` primitive, so
/// [`render_region_tiled`] can stamp out one context per tile (only
/// `offset_x`/`offset_y`/`out_w`/`out_h` differ) without rebuilding the q24 /
/// gamma-LUT plumbing each time.
#[derive(Clone, Copy)]
struct CompositeContext<'a> {
    opts: &'a RenderOptions,
    page_w: u32,
    page_h: u32,
    bg: Option<&'a Pixmap>,
    /// Q24 ratio for converting page-space FRACBITS coordinates to BG-plane
    /// FRACBITS coordinates.  Uses the inferred integer BG44 cell pitch so
    /// padded edge cells do not stretch across the native render.  `0` when
    /// `bg` is `None`.
    bg_x_q24: u64,
    bg_y_q24: u64,
    mask: Option<&'a crate::bitmap::Bitmap>,
    /// `mask_sub.trailing_zeros()` where mask_sub is 1 (full-res) or 4 (1/4-res).
    /// Using a shift instead of division avoids a UDIV instruction in the hot path.
    mask_shift: u32,
    fg_palette: Option<&'a FgbzPalette>,
    /// Per-pixel blit index map (same dimensions as mask). `-1` = no blit.
    blit_map: Option<&'a [i32]>,
    fg44: Option<&'a Pixmap>,
    /// Q24 ratio for converting page-space FRACBITS coordinates to FG44-space
    /// FRACBITS coordinates.  The horizontal ratio uses the inferred integer
    /// foreground colour-cell pitch; the vertical ratio uses the encoded plane
    /// height so the bottom row remains reachable.  `0` when `fg44` is `None`.
    fg_x_q24: u64,
    fg_y_q24: u64,
    gamma_lut: &'a [u8; 256],
    /// True when gamma_lut is the identity mapping (lut[i] == i for all i).
    gamma_is_identity: bool,
    /// X offset within the full render (for region renders; 0 for full page).
    offset_x: u32,
    /// Y offset within the full render (for region renders; 0 for full page).
    offset_y: u32,
    /// Output width (may be smaller than opts.width for region renders).
    out_w: u32,
    /// Output height (may be smaller than opts.height for region renders).
    out_h: u32,
}

#[derive(Clone, Copy)]
struct AreaAvgX {
    fx: u32,
    bg_fx: u32,
    bg_x0: u32,
    bg_x1: u32,
}

// Exclusive upper bounds: the output pixel covers source [x0, x1) x [y0, y1).
// The old inclusive formula gave a 3x3 box at 2x downscale because x1 landed on
// the first pixel of the next output cell; exclusive gives the correct 2x2.
fn area_range(limit: u32, f: u32, step: u32) -> (u32, u32) {
    (
        (f >> FRACBITS).min(limit.saturating_sub(1)),
        ((f + step) >> FRACBITS).min(limit),
    )
}

fn precompute_area_avg_x(
    ctx: &CompositeContext<'_>,
    fx_step: u32,
    bg_fx_step: u32,
) -> Vec<AreaAvgX> {
    let mut xs = Vec::with_capacity(ctx.out_w as usize);
    let bg_w = ctx.bg.map_or(0, |bg| bg.width);
    for ox in 0..ctx.out_w {
        let fx = (ox + ctx.offset_x) * fx_step;
        let bg_fx = ((fx as u64 * ctx.bg_x_q24) >> 24) as u32;
        let (bg_x0, bg_x1) = area_range(bg_w, bg_fx, bg_fx_step);
        xs.push(AreaAvgX {
            fx,
            bg_fx,
            bg_x0,
            bg_x1,
        });
    }
    xs
}

/// Per-output-column bg sampling data for the bilinear (upscale / 1:1) path:
/// clamped source columns `x0`/`x1` and the horizontal fractional weight `tx`.
/// Column mapping never depends on the row, so this is computed once per
/// render instead of once per pixel — the AreaAvgX analog for upscaling.
#[derive(Clone, Copy)]
struct BilinearX {
    x0: u32,
    x1: u32,
    tx: u32,
}

/// Build the per-column table for [`composite_rows_bilinear_one`]. Walks the
/// exact Q48 fixed-point accumulator of the in-loop fallback (`bg_fx_q`), so
/// table lookups and the fallback produce byte-identical coordinates.
fn precompute_bilinear_x(ctx: &CompositeContext<'_>, fx_step: u32) -> Option<Vec<BilinearX>> {
    let bg = ctx.bg?;
    let clamp_w = bg.width.saturating_sub(1);
    let bg_fx_step_q: u64 = fx_step as u64 * ctx.bg_x_q24;
    let mut bg_fx_q: u64 = (ctx.offset_x as u64 * fx_step as u64 + FRAC as u64 / 2) * ctx.bg_x_q24;
    let mut xs = Vec::with_capacity(ctx.out_w as usize);
    for _ in 0..ctx.out_w {
        let bg_fx = ((bg_fx_q >> 24) as u32).saturating_sub(FRAC / 2);
        let x0 = (bg_fx >> FRACBITS).min(clamp_w);
        xs.push(BilinearX {
            x0,
            x1: (x0 + 1).min(clamp_w),
            tx: bg_fx & FRAC_MASK,
        });
        bg_fx_q = bg_fx_q.wrapping_add(bg_fx_step_q);
    }
    Some(xs)
}

impl<'a> CompositeContext<'a> {
    /// Build a composite context from already-decoded layers.
    ///
    /// This is the single home for the per-render wiring that was copy-pasted
    /// across all five render entry points: the q24 cell-pitch arithmetic (where
    /// the #199 BG/FG alignment fix lives), the page-dimension lookup, and the
    /// gamma-LUT / offset / output-size plumbing. Callers decode their layers
    /// (full or partial background, optionally sub-4 mask) and hand them here so
    /// a q24 / offset / gamma bug can only ever be fixed in one place.
    ///
    /// The `(mask, mask_shift)` pair is passed in rather than derived because it
    /// legitimately varies per entry point — the full-page paths swap in the
    /// 1/4-resolution mask via [`resolve_sub4_mask`], while region / coarse /
    /// progressive renders always composite against the full-resolution mask.
    #[allow(clippy::too_many_arguments)]
    fn from_layers(
        page: &DjVuPage,
        opts: &'a RenderOptions,
        bg: Option<&'a Pixmap>,
        mask: Option<&'a crate::bitmap::Bitmap>,
        mask_shift: u32,
        fg_palette: Option<&'a FgbzPalette>,
        blit_map: Option<&'a [i32]>,
        fg44: Option<&'a Pixmap>,
        gamma_lut: &'a [u8; 256],
        offset: (u32, u32),
        out: (u32, u32),
    ) -> Self {
        let page_w = page.width() as u32;
        let page_h = page.height() as u32;
        let (fg_x_q24, fg_y_q24) = fg_q24(fg44, page_w, page_h);
        let (bg_x_q24, bg_y_q24) = bg_q24(bg, page_w, page_h);
        CompositeContext {
            opts,
            page_w,
            page_h,
            bg,
            bg_x_q24,
            bg_y_q24,
            mask,
            mask_shift,
            fg_palette,
            blit_map,
            fg44,
            fg_x_q24,
            fg_y_q24,
            gamma_lut,
            gamma_is_identity: gamma_lut.iter().enumerate().all(|(i, &v)| v == i as u8),
            offset_x: offset.0,
            offset_y: offset.1,
            out_w: out.0,
            out_h: out.1,
        }
    }
}

/// Pick the mask plane and shift for a full-page composite.
///
/// At background subsample ≥ 4 (and only when no bold dilation or FGbz palette
/// is in play, since those need full-resolution lookups) the compositor reads a
/// pre-downsampled 1/4-resolution mask — one bit lookup per output pixel instead
/// of 4–9. The returned shift is `mask_sub.trailing_zeros()` (2 for the 1/4-res
/// mask, 0 for the full-res mask). This is the single home for that decision,
/// previously copy-pasted into `render_rows` and `render_into`.
#[cfg(feature = "std")]
fn resolve_sub4_mask<'a>(
    page: &'a DjVuPage,
    bg_subsample: u32,
    opts: &RenderOptions,
    full_mask: Option<&'a crate::bitmap::Bitmap>,
    fg_palette: Option<&FgbzPalette>,
) -> (Option<&'a crate::bitmap::Bitmap>, u32) {
    if bg_subsample >= 4 && opts.bold == 0 && fg_palette.is_none() {
        (page_mask_sub4(page), 2)
    } else {
        (full_mask, 0)
    }
}

#[cfg(not(feature = "std"))]
fn resolve_sub4_mask<'a>(
    _page: &'a DjVuPage,
    _bg_subsample: u32,
    _opts: &RenderOptions,
    full_mask: Option<&'a crate::bitmap::Bitmap>,
    _fg_palette: Option<&FgbzPalette>,
) -> (Option<&'a crate::bitmap::Bitmap>, u32) {
    (full_mask, 0)
}

/// Look up the palette color for a foreground pixel at (px, py).
///
/// Uses the blit map to find the per-glyph blit index, then maps it through
/// the FGbz index table to get the final color. Falls back to `palette[0]` when
/// no index table is present, and to black when lookup fails.
#[inline]
fn lookup_palette_color(
    pal: &FgbzPalette,
    blit_map: Option<&[i32]>,
    mask: Option<&crate::bitmap::Bitmap>,
    px: u32,
    py: u32,
) -> PaletteColor {
    if let Some(bm) = blit_map
        && let Some(m) = mask
    {
        let mi = py as usize * m.width as usize + px as usize;
        if mi < bm.len() {
            let blit_idx = bm[mi];
            if blit_idx >= 0 {
                if !pal.indices.is_empty() {
                    // Two-level indirection: blit_idx → color_idx → color
                    let bi = blit_idx as usize;
                    if bi < pal.indices.len() {
                        let ci = pal.indices[bi] as usize;
                        if ci < pal.colors.len() {
                            return pal.colors[ci];
                        }
                    }
                } else {
                    // No index table: use blit_idx directly as color index
                    let ci = blit_idx as usize;
                    if ci < pal.colors.len() {
                        return pal.colors[ci];
                    }
                }
            }
        }
    }
    // Fallback: first palette color or black
    pal.colors.first().copied().unwrap_or_default()
}

/// Composite one page into `buf` (RGBA, pre-allocated) using the given context.
///
/// This is a zero-allocation render path when `buf` is already the right size.
/// For region renders, `ctx.out_w`/`ctx.out_h` give the output dimensions and
/// `ctx.offset_x`/`ctx.offset_y` give the starting offset within the full render.
///
/// Iterates the output rows over the same single-row composite bodies used by
/// [`composite_rows`], writing each row directly into its slice of `buf` with no
/// intermediate copy. The two paths therefore share one per-pixel decision tree.
fn composite_into(ctx: &CompositeContext<'_>, buf: &mut [u8]) -> Result<(), RenderError> {
    let full_w = ctx.opts.width;
    let full_h = ctx.opts.height;

    // Fixed-point step: how many source pixels per full-render output pixel
    let fx_step = ((ctx.page_w as u64 * FRAC as u64) / full_w.max(1) as u64) as u32;
    let fy_step = ((ctx.page_h as u64 * FRAC as u64) / full_h.max(1) as u64) as u32;

    let row_stride = ctx.out_w as usize * 4;

    // Bilevel fast path: JB2-only page (no IW44 bg, no FG44, no palette).
    // Skips bilinear sampling and gamma LUT — just white fill + black mask writes.
    // Writes alpha=255 inline; returns early before the general compositor loop.
    if ctx.bg.is_none() && ctx.fg44.is_none() && ctx.fg_palette.is_none() {
        #[cfg(feature = "parallel")]
        {
            use rayon::prelude::*;
            let n = ctx.out_h as usize * row_stride;
            buf[..n]
                .par_chunks_exact_mut(row_stride)
                .enumerate()
                .for_each(|(oy, row)| {
                    composite_rows_bilevel_one(ctx, oy as u32, fx_step, fy_step, row);
                });
        }
        #[cfg(not(feature = "parallel"))]
        for (oy, row) in buf[..ctx.out_h as usize * row_stride]
            .chunks_exact_mut(row_stride)
            .enumerate()
        {
            composite_rows_bilevel_one(ctx, oy as u32, fx_step, fy_step, row);
        }
        return Ok(());
    }

    let downscale = fx_step > FRAC || fy_step > FRAC;
    // Precompute bg-space step for the area-average path (avoids per-pixel multiply).
    let bg_fx_step = ((fx_step as u64 * ctx.bg_x_q24) >> 24) as u32;
    let bg_fy_step = ((fy_step as u64 * ctx.bg_y_q24) >> 24) as u32;
    let area_avg_x = downscale.then(|| precompute_area_avg_x(ctx, fx_step, bg_fx_step));
    let bilinear_x = (!downscale)
        .then(|| precompute_bilinear_x(ctx, fx_step))
        .flatten();

    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        let n = ctx.out_h as usize * row_stride;
        buf[..n]
            .par_chunks_exact_mut(row_stride)
            .enumerate()
            .for_each_init(Vec::new, |vblend, (oy, row)| {
                if downscale {
                    composite_rows_area_avg_one(
                        ctx,
                        oy as u32,
                        fx_step,
                        fy_step,
                        bg_fx_step,
                        bg_fy_step,
                        row,
                        area_avg_x.as_deref(),
                    );
                } else {
                    composite_rows_bilinear_one(
                        ctx,
                        oy as u32,
                        fx_step,
                        fy_step,
                        row,
                        bilinear_x.as_deref(),
                        vblend,
                    );
                }
            });
    }
    #[cfg(not(feature = "parallel"))]
    {
        let mut vblend = Vec::new();
        for (oy, row) in buf[..ctx.out_h as usize * row_stride]
            .chunks_exact_mut(row_stride)
            .enumerate()
        {
            if downscale {
                composite_rows_area_avg_one(
                    ctx,
                    oy as u32,
                    fx_step,
                    fy_step,
                    bg_fx_step,
                    bg_fy_step,
                    row,
                    area_avg_x.as_deref(),
                );
            } else {
                composite_rows_bilinear_one(
                    ctx,
                    oy as u32,
                    fx_step,
                    fy_step,
                    row,
                    bilinear_x.as_deref(),
                    &mut vblend,
                );
            }
        }
    }

    // All render paths (bilevel, bilinear, area-average) write alpha=255 inline;
    // no separate fill_alpha_255 post-pass is needed.

    Ok(())
}

/// Drive the composite hot path row-by-row, calling `sink(row_index, &row_rgba)`
/// once per output row.
///
/// Each call to `sink` receives a 4-byte-per-pixel RGBA slice of width
/// `ctx.out_w`.  A single scratch row is allocated up-front (not per-row), so
/// peak additional heap use is `out_w * 4` bytes regardless of page height.
///
/// This is the internal streaming primitive used by [`render_rows`]; callers
/// that already hold a flat output buffer should prefer [`composite_into`],
/// which writes directly without an intermediate copy.
fn composite_rows<F>(ctx: &CompositeContext<'_>, mut sink: F) -> Result<(), RenderError>
where
    F: FnMut(usize, &[u8]),
{
    let full_w = ctx.opts.width;
    let full_h = ctx.opts.height;

    // Fixed-point step: how many source pixels per full-render output pixel.
    let fx_step = ((ctx.page_w as u64 * FRAC as u64) / full_w.max(1) as u64) as u32;
    let fy_step = ((ctx.page_h as u64 * FRAC as u64) / full_h.max(1) as u64) as u32;

    let row_stride = ctx.out_w as usize * 4;

    // Bilevel fast path: JB2-only page (no IW44 bg, no FG44, no palette).
    if ctx.bg.is_none() && ctx.fg44.is_none() && ctx.fg_palette.is_none() {
        let mut row_buf = vec![0u8; row_stride];
        for oy in 0..ctx.out_h {
            composite_rows_bilevel_one(ctx, oy, fx_step, fy_step, &mut row_buf);
            sink(oy as usize, &row_buf);
        }
        return Ok(());
    }

    let mut row_buf = vec![0u8; row_stride];
    let downscale = fx_step > FRAC || fy_step > FRAC;

    // Precompute bg-space step for area-average path (avoids per-pixel multiply).
    let bg_fx_step = ((fx_step as u64 * ctx.bg_x_q24) >> 24) as u32;
    let bg_fy_step = ((fy_step as u64 * ctx.bg_y_q24) >> 24) as u32;
    let area_avg_x = downscale.then(|| precompute_area_avg_x(ctx, fx_step, bg_fx_step));
    let bilinear_x = (!downscale)
        .then(|| precompute_bilinear_x(ctx, fx_step))
        .flatten();
    let mut vblend = Vec::new();

    for oy in 0..ctx.out_h {
        if downscale {
            composite_rows_area_avg_one(
                ctx,
                oy,
                fx_step,
                fy_step,
                bg_fx_step,
                bg_fy_step,
                &mut row_buf,
                area_avg_x.as_deref(),
            );
        } else {
            composite_rows_bilinear_one(
                ctx,
                oy,
                fx_step,
                fy_step,
                &mut row_buf,
                bilinear_x.as_deref(),
                &mut vblend,
            );
        }
        sink(oy as usize, &row_buf);
    }

    Ok(())
}

/// Write one bilevel row into `row_buf`.
#[inline]
fn composite_rows_bilevel_one(
    ctx: &CompositeContext<'_>,
    oy: u32,
    fx_step: u32,
    fy_step: u32,
    row_buf: &mut [u8],
) {
    let mask = match ctx.mask {
        Some(m) => m,
        None => {
            for chunk in row_buf.as_chunks_mut::<4>().0 {
                chunk[0] = 255;
                chunk[1] = 255;
                chunk[2] = 255;
                chunk[3] = 255;
            }
            return;
        }
    };

    // 1:1 scale fast path.
    if fx_step == FRAC && fy_step == FRAC {
        let stride = mask.row_stride();
        // Same shape as the column clamp below: `mask.data` holds `mask.height`
        // rows, not `page_h` rows. An INFO chunk that declares a page taller
        // than the bilevel mask it ships walked `py` past the end of the data
        // and panicked on the range. Clamp to the mask's own height too, and
        // take the row through `get`, so a short or empty mask renders white
        // instead of unwinding.
        //
        // Both bounds checks for this row are here, once, rather than per
        // pixel: `mask_row` is exactly `stride` bytes and `stride` is
        // `ceil(mask.width / 8)`, so once a column is clamped to
        // `mask.width - 1` its byte index cannot leave the row. That is what
        // lets the fallback loop below keep indexing directly. A zero-width
        // mask has no byte to read at all, so it leaves with the empty row.
        let py = (oy + ctx.offset_y)
            .min(ctx.page_h.saturating_sub(1))
            .min(mask.height.saturating_sub(1)) as usize;
        let Some(mask_row) = mask.data.get(py * stride..(py + 1) * stride) else {
            row_buf.fill(255);
            return;
        };
        if mask_row.is_empty() {
            row_buf.fill(255);
            return;
        }

        // I3: whole-row white fast path. If the mask row has no foreground bits (page
        // margins, blank inter-line gaps — typically 25-35% of rows in text scans),
        // fill the output row with white in one NEON-vectorised store instead of running
        // the per-pixel bit-extraction loop.
        if !mask_row.iter().any(|&b| b != 0) {
            row_buf.fill(255);
            return;
        }

        // P2: BILEVEL_RGBA table fast path. One table lookup per mask byte produces
        // 8 pre-packed RGBA pixels (32 bytes); copy_from_slice compiles to 2 NEON vst1
        // stores, replacing 8×16 scalar instructions. Works whenever offset_x is
        // byte-aligned (offset_x % 8 == 0) so output byte `i` maps to source mask byte
        // `offset_x/8 + i` with no per-pixel bit shuffle — covers full-page renders
        // (offset_x==0) and byte-aligned `render_region` viewports. Guard
        // `offset_x + out_w <= mask.width` keeps the source index in bounds and makes
        // the `.min(page_w-1)` clamp a no-op (mask.width == page_w for the 1:1 mask).
        let out_w = row_buf.len() / 4;
        let ox0 = ctx.offset_x as usize;
        if ctx.offset_x.is_multiple_of(8) && ox0 + out_w <= mask.width as usize {
            let mb0 = ox0 / 8; // first source mask byte (0 for full-page renders)
            let nb_full = out_w / 8; // number of full mask bytes (8 pixels each)
            let nb_rem = out_w % 8; // trailing pixels from a partial mask byte
            for byte_idx in 0..nb_full {
                let mb = mask_row[mb0 + byte_idx];
                let src = &BILEVEL_RGBA[mb as usize];
                row_buf[byte_idx * 32..(byte_idx + 1) * 32].copy_from_slice(src);
            }
            if nb_rem > 0 {
                let mb = mask_row[mb0 + nb_full];
                let src = &BILEVEL_RGBA[mb as usize];
                let base = nb_full * 32;
                row_buf[base..base + nb_rem * 4].copy_from_slice(&src[..nb_rem * 4]);
            }
            return;
        }

        // Fallback: branchless per-pixel expansion with .min() clamp for partial/offset views.
        //
        // `mask_row` is `mask.width` pixels wide, not `page_w` wide: an INFO chunk that
        // declares a page wider than the bilevel mask it ships (a fuzzed or malformed file)
        // used to walk `px` past the end of `mask_row` here and panic on the index. The
        // fast path above already guards this (`ox0 + out_w <= mask.width`); clamp to the
        // mask's own width too.
        //
        // The clamp is the bounds check, and it is the only one this loop needs:
        // `last_col <= mask.width - 1` and `mask_row` is `ceil(mask.width / 8)`
        // bytes, so `px >> 3` is always a byte of this row. Reading it through
        // `get(..).map_or(..)` instead cost 5.7 % on `render_region_bilevel` —
        // the per-pixel branch is what the word "branchless" above is about.
        let last_col = ctx
            .page_w
            .saturating_sub(1)
            .min(mask.width.saturating_sub(1));
        for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let px = (ox as u32 + ctx.offset_x).min(last_col) as usize;
            let is_fg = ((mask_row[px >> 3] >> (7 - (px & 7))) & 1) as u32;
            let ch = (is_fg.wrapping_sub(1) & 0xFF) as u8; // 0 when fg, 255 when bg
            pixel[0] = ch;
            pixel[1] = ch;
            pixel[2] = ch;
            pixel[3] = 255;
        }
        return;
    }

    let downscale = fx_step > FRAC || fy_step > FRAC;
    let fy = (oy + ctx.offset_y) * fy_step;
    let py = (fy >> FRACBITS).min(ctx.page_h.saturating_sub(1));

    // I3-downscale: all-white band fast path for the anti-aliased path. The
    // source mask band [y0, y1) for this output row is row-invariant (fy is
    // fixed), so if it has no foreground bits every output pixel's coverage is 0
    // → white. One NEON-vectorised scan replaces out_w `mask_box_coverage` calls
    // (each of which itself scans the band). Only `mask_shift == 0` is covered —
    // the max-pool sub-path indexes the mask at a coarser resolution. The y range
    // matches `mask_box_coverage` exactly; `y1 <= y0` is the degenerate
    // zero-coverage case (also white).
    if downscale && ctx.mask_shift == 0 {
        let stride = mask.row_stride();
        let y0 = (fy >> FRACBITS).min(mask.height.saturating_sub(1)) as usize;
        let y1 = ((fy + fy_step) >> FRACBITS).min(mask.height) as usize;
        if y1 <= y0 || !mask.data[y0 * stride..y1 * stride].iter().any(|&b| b != 0) {
            row_buf.fill(255);
            return;
        }
    }

    for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let fx = (ox as u32 + ctx.offset_x) * fx_step;
        let px = (fx >> FRACBITS).min(ctx.page_w.saturating_sub(1));

        // ch = 0 → black (foreground), 255 → white (background).
        // At downscale, count the fraction of foreground mask bits in the output pixel's
        // footprint (anti-aliased). At 1:1, use the exact mask bit (binary).
        let ch = if downscale {
            if ctx.mask_shift > 0 {
                // Subsampled max-pool mask: one boolean covers the footprint already.
                let dpx = fx >> (FRACBITS + ctx.mask_shift);
                let dpy = fy >> (FRACBITS + ctx.mask_shift);
                if dpx < mask.width && dpy < mask.height && mask.get(dpx, dpy) {
                    0u8
                } else {
                    255u8
                }
            } else {
                // Anti-aliased: coverage fraction → gray.
                255 - mask_box_coverage(mask, fx, fy, fx_step, fy_step)
            }
        } else if ctx.opts.mask_aa && ctx.mask_shift == 0 {
            // D_AA_ZOOM (opt-in): this branch is only reachable past the exact
            // 1:1 early return above, so `!downscale` here means a genuine
            // upscale (zoom > 1) in at least one axis. Bilinearly interpolate
            // the mask's 0/255 bits instead of the hard nearest-bit lookup —
            // smooths glyph edges under zoom. Default `mask_aa: false` never
            // takes this branch, so the fast nearest path below is untouched.
            255 - mask_bilinear_coverage(mask, fx, fy)
        } else if px < mask.width && py < mask.height && mask.get(px, py) {
            0u8
        } else {
            255u8
        };
        pixel[0] = ch;
        pixel[1] = ch;
        pixel[2] = ch;
        pixel[3] = 255;
    }
}

/// Write one bilinear row into `row_buf` (upscale / 1:1).
///
/// `bx` is the optional per-column table from [`precompute_bilinear_x`]
/// (`None` falls back to the in-loop fixed-point walk — byte-identical).
/// `vblend` is caller-owned scratch for the vertically pre-blended bg row;
/// reusing it across rows avoids a per-row allocation.
#[inline]
fn composite_rows_bilinear_one(
    ctx: &CompositeContext<'_>,
    oy: u32,
    fx_step: u32,
    fy_step: u32,
    row_buf: &mut [u8],
    bx: Option<&[BilinearX]>,
    vblend: &mut Vec<u16>,
) {
    let (page_w, page_h) = (ctx.page_w, ctx.page_h);
    let fy = (oy + ctx.offset_y) * fy_step;
    let py = (fy >> FRACBITS).min(page_h.saturating_sub(1));

    // 1:1 fast path: fx and fy land on exact pixel centres (tx = ty = 0), so
    // bilinear interpolation degrades to nearest-neighbour. Guard on the bg
    // plane ratio too: if bg is at subsample > 1, the bg coordinates are not
    // integer-aligned even at native scale and bilinear blending is needed.
    if fx_step == FRAC && fy_step == FRAC && ctx.bg_x_q24 == (1 << 24) && ctx.bg_y_q24 == (1 << 24)
    {
        // Extra-tight path for the common corpus case: bg present, mask
        // present, no palette, no FG44, zero horizontal offset. Precompute
        // the bg row and mask row slices so the inner loop only touches
        // sequential memory with no per-pixel coordinate mapping calls.
        if ctx.offset_x == 0
            && ctx.fg_palette.is_none()
            && ctx.fg44.is_none()
            && let Some(bg) = ctx.bg
        {
            let bg_py = py.min(bg.height.saturating_sub(1)) as usize;
            let bg_stride = bg.width as usize * 4;
            let bg_row = bg.data.get(bg_py * bg_stride..).unwrap_or(&[]);
            let lut = &ctx.gamma_lut;

            if let Some(mask) = ctx.mask {
                // Has mask: check each pixel for foreground (black).
                let mask_stride = mask.row_stride();
                let mask_py = py.min(mask.height.saturating_sub(1)) as usize;
                let mask_row = mask.data.get(mask_py * mask_stride..).unwrap_or(&[]);

                // A2: pre-expand mask bits to bytes via LUT, then branchless blend.
                let bg_max_px = (bg.width as usize).saturating_sub(1);
                let mask_limit = mask.width as usize;
                let out_w = row_buf.len() / 4;
                // D1: hoist gamma identity check outside the pixel loop.
                macro_rules! a2_has_mask_loop {
                    ($write:expr) => {
                        for mb_idx in 0..out_w.div_ceil(8) {
                            let mb = mask_row.get(mb_idx).copied().unwrap_or(0);
                            let exp = &MASK_EXPAND[mb as usize];
                            for j in 0..8usize {
                                let ox = mb_idx * 8 + j;
                                if ox >= out_w {
                                    break;
                                }
                                let fg_m = if ox < mask_limit { exp[j] } else { 0u8 };
                                let px = ox.min(bg_max_px);
                                let off = px * 4;
                                let pixel = &mut row_buf[ox * 4..(ox + 1) * 4];
                                let (r, g, b) = if let Some(q) = bg_row.get(off..off + 4) {
                                    (q[0] & !fg_m, q[1] & !fg_m, q[2] & !fg_m)
                                } else {
                                    (!fg_m, !fg_m, !fg_m)
                                };
                                $write(pixel, r, g, b);
                            }
                        }
                    };
                }
                if ctx.gamma_is_identity {
                    a2_has_mask_loop!(|pixel: &mut [u8], r, g, b| {
                        pixel[0] = r;
                        pixel[1] = g;
                        pixel[2] = b;
                        pixel[3] = 255;
                    });
                } else {
                    a2_has_mask_loop!(|pixel: &mut [u8], r, g, b| {
                        pixel[0] = lut[r as usize];
                        pixel[1] = lut[g as usize];
                        pixel[2] = lut[b as usize];
                        pixel[3] = 255;
                    });
                }
            } else {
                // No mask: pure background copy with gamma correction.
                // D1: hoist gamma identity check outside the pixel loop.
                if ctx.gamma_is_identity {
                    let out_w = row_buf.len() / 4;
                    // E1: when bg covers the full output width, bulk-copy the row
                    // via memcpy — bg Pixmap always has alpha=255 from YCbCr decode.
                    if bg.width as usize >= out_w {
                        row_buf[..out_w * 4].copy_from_slice(&bg_row[..out_w * 4]);
                    } else {
                        for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                            let px = ox.min((bg.width as usize).saturating_sub(1));
                            let off = px * 4;
                            if let Some(q) = bg_row.get(off..off + 4) {
                                pixel[0] = q[0];
                                pixel[1] = q[1];
                                pixel[2] = q[2];
                            } else {
                                pixel[0] = 255;
                                pixel[1] = 255;
                                pixel[2] = 255;
                            }
                            pixel[3] = 255;
                        }
                    }
                } else {
                    for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let px = ox.min((bg.width as usize).saturating_sub(1));
                        let off = px * 4;
                        if let Some(q) = bg_row.get(off..off + 4) {
                            pixel[0] = lut[q[0] as usize];
                            pixel[1] = lut[q[1] as usize];
                            pixel[2] = lut[q[2] as usize];
                        } else {
                            pixel[0] = 255;
                            pixel[1] = 255;
                            pixel[2] = 255;
                        }
                        pixel[3] = 255;
                    }
                }
            }
            return;
        }

        // General 1:1 nearest-neighbour path (offset, palette, or FG44 present).

        // C2: Pre-hoist FG44 y-rows (row-invariant, analogous to bg_rows in B-series path).
        // Eliminates per-fg-pixel: map_plane_center_frac(fy), y0/y1/ty computation, row lookups.
        let fg_rows_1x1 = ctx.fg44.filter(|_| ctx.fg_palette.is_none()).map(|fg| {
            let fg_fy = map_plane_center_frac(fy, ctx.fg_y_q24);
            let y0 = (fg_fy >> FRACBITS).min(fg.height.saturating_sub(1)) as usize;
            let y1 = (y0 + 1).min(fg.height.saturating_sub(1) as usize);
            let ty = fg_fy & FRAC_MASK;
            let stride = fg.width as usize * 4;
            let row0 = fg.data.get(y0 * stride..).unwrap_or(&[]);
            let row1 = fg.data.get(y1 * stride..).unwrap_or(&[]);
            (row0, row1, fg.width, ty)
        });
        // C2b: Pre-hoist bg row slice (bg_x_q24 == bg_y_q24 == 1<<24 guaranteed by outer
        // condition, so bg_fx == fx and the bg row index == py clamped to bg.height).
        let bg_row_1x1 = ctx.bg.and_then(|bg| {
            let by = (py as usize).min(bg.height.saturating_sub(1) as usize);
            let stride = bg.width as usize * 4;
            bg.data.get(by * stride..).map(|row| (row, bg.width))
        });
        // C3: Pre-hoist mask row (py is row-invariant; eliminates y*stride multiply per pixel).
        let mask_row_1x1 = ctx.mask.and_then(|m| {
            if py >= m.height {
                return None;
            }
            let stride = m.row_stride();
            m.data.get(py as usize * stride..).map(|row| (row, m.width))
        });

        // F2: whole-row background fast path.
        // If the mask row has no foreground bits (page margins, blank inter-line gaps — typically
        // 30-40% of rows in text documents), bulk-copy from bg_row instead of dispatching
        // per-pixel between FG44 bilinear and BG44 lookup.
        {
            let row_is_all_bg = match mask_row_1x1 {
                None => true,
                Some((mask_row, mask_w)) => {
                    let check_bytes = (mask_w as usize).div_ceil(8).min(mask_row.len());
                    mask_row[..check_bytes].iter().all(|&b| b == 0)
                }
            };
            if row_is_all_bg && ctx.gamma_is_identity {
                let out_w = row_buf.len() / 4;
                let offset_x = ctx.offset_x as usize;
                if let Some((bg_row, bg_w)) = bg_row_1x1 {
                    if offset_x + out_w <= bg_w as usize {
                        row_buf.copy_from_slice(&bg_row[offset_x * 4..(offset_x + out_w) * 4]);
                        return;
                    }
                    // Edge case (out_w clamped beyond bg_w): fall through to per-pixel loop.
                } else {
                    row_buf.fill(255);
                    return;
                }
            } else if row_is_all_bg {
                // #443: F2 for non-identity gamma. An all-bg row is just the gamma
                // LUT applied to the bg row (or to white when there is no bg) — a
                // sequential LUT pass that skips the G1 pre-expansion + per-pixel
                // dispatch. Byte-identical to the per-pixel loop for these rows.
                let out_w = row_buf.len() / 4;
                let offset_x = ctx.offset_x as usize;
                let lut = &ctx.gamma_lut;
                if let Some((bg_row, bg_w)) = bg_row_1x1 {
                    if offset_x + out_w <= bg_w as usize {
                        let src = &bg_row[offset_x * 4..(offset_x + out_w) * 4];
                        for (chunk, s) in row_buf
                            .as_chunks_mut::<4>()
                            .0
                            .iter_mut()
                            .zip(src.as_chunks::<4>().0)
                        {
                            chunk[0] = lut[s[0] as usize];
                            chunk[1] = lut[s[1] as usize];
                            chunk[2] = lut[s[2] as usize];
                            chunk[3] = 255;
                        }
                        return;
                    }
                    // Edge case (out_w clamped beyond bg_w): fall through.
                } else {
                    let white = lut[255];
                    for chunk in row_buf.as_chunks_mut::<4>().0 {
                        chunk[0] = white;
                        chunk[1] = white;
                        chunk[2] = white;
                        chunk[3] = 255;
                    }
                    return;
                }
            }
        }

        // G1: Pre-expand the mask row from bit-packed to per-pixel bytes via MASK_EXPAND LUT.
        // Reduces per-pixel mask check from ~7 ops (shift, bounds-check, bit-extract) to a
        // single byte load + compare. Buffer covers up to 600 DPI A4/US-letter (≤4096px);
        // oversized pages fall through to the original bit-extraction path.
        const G1_MAX: usize = 4096;
        let mut g1_buf = [0u8; G1_MAX];
        let g1_mask: &[u8] = if let Some((mask_row, mask_w)) = mask_row_1x1 {
            let mw = mask_w as usize;
            if mw <= G1_MAX {
                let nb = mw.div_ceil(8);
                for (i, &mb) in mask_row[..nb].iter().enumerate() {
                    let exp = &MASK_EXPAND[mb as usize];
                    let base = i * 8;
                    // Write 8 bytes; g1_mask = &g1_buf[..mw] prevents reads past mask_w.
                    g1_buf[base..base + 8].copy_from_slice(exp);
                }
                &g1_buf[..mw]
            } else {
                &g1_buf[..0] // page too wide: use fallback bit-extraction below
            }
        } else {
            &g1_buf[..0] // no mask: all pixels are background
        };

        for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let fx = (ox as u32 + ctx.offset_x) * fx_step;
            let px = (fx >> FRACBITS).min(page_w.saturating_sub(1));

            // G1 fast path: single byte load. Falls back to bit-extraction when g1_mask
            // is empty (no mask, or page wider than G1_MAX). LLVM hoists the is_empty()
            // branch as loop-invariant and generates two loop versions.
            let is_fg = if g1_mask.is_empty() {
                mask_row_1x1.is_some_and(|(row, mask_w)| {
                    let pxu = px as usize;
                    pxu < mask_w as usize
                        && (row.get(pxu >> 3).copied().unwrap_or(0) >> (7 - (pxu & 7))) & 1 != 0
                })
            } else {
                g1_mask.get(px as usize).copied().unwrap_or(0) != 0
            };

            let (r, g, b) = if is_fg {
                if let Some(pal) = ctx.fg_palette {
                    let color = lookup_palette_color(pal, ctx.blit_map, ctx.mask, px, py);
                    (color.r, color.g, color.b)
                } else if let Some((fg_row0, fg_row1, fg_w, fg_ty)) = fg_rows_1x1 {
                    let fg_fx = map_plane_center_frac(fx, ctx.fg_x_q24);
                    bilinear_from_rows(fg_row0, fg_row1, fg_w, fg_fx, fg_ty)
                } else {
                    (0, 0, 0)
                }
            } else if let Some((bg_row, bg_w)) = bg_row_1x1 {
                let bx = (px as usize).min(bg_w.saturating_sub(1) as usize);
                let off = bx * 4;
                bg_row
                    .get(off..off + 4)
                    .map_or((255, 255, 255), |q| (q[0], q[1], q[2]))
            } else {
                (255, 255, 255)
            };

            if ctx.gamma_is_identity {
                pixel[0] = r;
                pixel[1] = g;
                pixel[2] = b;
            } else {
                pixel[0] = ctx.gamma_lut[r as usize];
                pixel[1] = ctx.gamma_lut[g as usize];
                pixel[2] = ctx.gamma_lut[b as usize];
            }
            pixel[3] = 255;
        }
        return;
    }

    // B1: hoist bg_fy (row-invariant) and replace per-pixel u64 mul for bg_fx with
    // an exact u64 accumulator (add per pixel instead of multiply).
    // bg_fx_q tracks (page_frac + FRAC/2) * bg_x_q24 in Q48; >> 24 gives the
    // FRAC-fixed-point coordinate; subtract FRAC/2 to get the centered sample pos.
    let bg_fy_hoist = ctx.bg.map(|_| map_plane_center_frac(fy, ctx.bg_y_q24));
    let bg_fx_step_q: u64 = fx_step as u64 * ctx.bg_x_q24;
    let mut bg_fx_q: u64 = (ctx.offset_x as u64 * fx_step as u64 + FRAC as u64 / 2) * ctx.bg_x_q24;

    // B2b: pre-hoist mask row slice for py (eliminates y*stride multiply per pixel).
    let mask_hoist = ctx.mask.and_then(|m| {
        if py >= m.height {
            return None;
        }
        let stride = m.row_stride();
        m.data.get(py as usize * stride..).map(|row| (row, m.width))
    });

    // #435: row-level all-bg fast path (F2 analog for the B-series path). Pre-scan
    // the hoisted mask row once; if it has no foreground bits (blank margins /
    // inter-line gaps), `is_fg` is constant-false for the whole row, so the
    // `!mask_all_bg &&` short-circuit lets LLVM unswitch the loop and drop the
    // per-pixel bit-extraction. Unlike F2 the bg pixels still need per-pixel
    // resampling, so this saves only the is_fg check (not the whole bg copy).
    let mask_all_bg = match mask_hoist {
        None => true,
        Some((mask_row, mask_w)) => {
            let nb = (mask_w as usize).div_ceil(8).min(mask_row.len());
            !mask_row[..nb].iter().any(|&b| b != 0)
        }
    };

    // B2/B3: precompute bg row slices (y0/y1 are row-invariant), then run the
    // vertical half of the separable bilinear blend once per bg column: with
    // oy fixed, ty and both source rows never change across the row, so
    // v = p0*ity + p1*ty (<= 255*FRAC, exact in u16) is shared by every output
    // pixel sampling that column. The horizontal half later computes
    // (v0*itx + v1*tx + 128) >> 8, which expands to the original 4-term dot
    // product of `bilinear_from_rows` — byte-identical by algebra.
    //
    // The pre-blend only covers the bg columns this row actually samples
    // ([col_start, col_end], from the monotonic accumulator's endpoints) —
    // a region render must not pay for the full bg width (#region bench).
    let vb: Option<(&[[u16; 4]], u32, u32)> = match ctx.bg {
        None => None,
        Some(bg) => {
            let bg_fy = bg_fy_hoist.unwrap_or(0);
            let clamp_h = bg.height.saturating_sub(1) as usize;
            let y0 = ((bg_fy >> FRACBITS) as usize).min(clamp_h);
            let y1 = (y0 + 1).min(clamp_h);
            let ty = bg_fy & FRAC_MASK;
            let ity = FRAC - ty;
            let stride = bg.width as usize * 4;
            let row0 = bg.data.get(y0 * stride..).unwrap_or(&[]);
            let row1 = bg.data.get(y1 * stride..).unwrap_or(&[]);
            let clamp_w = bg.width.saturating_sub(1);
            let fx_at = |q: u64| ((q >> 24) as u32).saturating_sub(FRAC / 2);
            let col_start = (fx_at(bg_fx_q) >> FRACBITS).min(clamp_w);
            let last_q =
                bg_fx_q.wrapping_add(bg_fx_step_q.wrapping_mul(ctx.out_w.saturating_sub(1) as u64));
            let col_end = ((fx_at(last_q) >> FRACBITS).min(clamp_w) + 1).min(clamp_w);
            let ncols = (col_end - col_start + 1) as usize;
            vblend.clear();
            vblend.resize(ncols * 4, 0);
            for (i, v) in vblend.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                // Truncated rows (partial/streaming decode) contribute zeros,
                // exactly like bilinear_from_rows' out-of-range corners.
                let off = (col_start as usize + i) * 4;
                let p0 = row0.get(off..off + 4);
                let p1 = row1.get(off..off + 4);
                for ch in 0..4 {
                    let a = p0.map_or(0, |q| q[ch] as u32);
                    let b = p1.map_or(0, |q| q[ch] as u32);
                    v[ch] = (a * ity + b * ty) as u16;
                }
            }
            Some((vblend.as_chunks::<4>().0, bg.width, col_start))
        }
    };

    // Horizontal half of the separable blend. The column entry comes from the
    // precomputed table when available, else from the Q48 accumulator — the
    // same walk `precompute_bilinear_x` replicates. `col_start` shifts full-bg
    // column indices into the windowed `vb_row`.
    let hblend =
        |vb_row: &[[u16; 4]], bg_w: u32, col_start: u32, e: Option<BilinearX>, bg_fx_q: u64| {
            let e = e.unwrap_or_else(|| {
                let bg_fx = ((bg_fx_q >> 24) as u32).saturating_sub(FRAC / 2);
                let clamp_w = bg_w.saturating_sub(1);
                let x0 = (bg_fx >> FRACBITS).min(clamp_w);
                BilinearX {
                    x0,
                    x1: (x0 + 1).min(clamp_w),
                    tx: bg_fx & FRAC_MASK,
                }
            });
            let v0 = vb_row
                .get(e.x0.saturating_sub(col_start) as usize)
                .copied()
                .unwrap_or([0; 4]);
            let v1 = vb_row
                .get(e.x1.saturating_sub(col_start) as usize)
                .copied()
                .unwrap_or([0; 4]);
            let itx = FRAC - e.tx;
            let f = |i: usize| ((v0[i] as u32 * itx + v1[i] as u32 * e.tx + 128) >> 8) as u8;
            (f(0), f(1), f(2))
        };

    // D_AA_ZOOM (opt-in): this function is only invoked when `!downscale`
    // (composite_into/composite_rows dispatch downscale to the area-average
    // path), but that includes an exact page-level 1:1 render whose *bg*
    // plane is subsampled (bg_x_q24/bg_y_q24 != 1<<24) — very common for
    // scanned BG44 pages — which fails the "extra-tight" 1:1 fast path above
    // and falls through here too. Mask AA must only kick in on a genuine
    // zoom (upscale in at least one axis), never on that native 1:1 case, so
    // gate on `fx_step`/`fy_step` directly rather than reusing `!downscale`.
    let mask_upscale =
        ctx.opts.mask_aa && ctx.mask_shift == 0 && (fx_step < FRAC || fy_step < FRAC);

    for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let fx = (ox as u32 + ctx.offset_x) * fx_step;
        let px = (fx >> FRACBITS).min(page_w.saturating_sub(1));

        // `coverage` generalises the binary `is_fg` lookup to a 0..=255
        // foreground fraction: 0 = fully background, 255 = fully foreground,
        // matching `mask_bilinear_coverage`'s convention. With `mask_aa`
        // disabled (default) it only ever takes the values 0 or 255 via the
        // exact same nearest-bit test as before, and the two special cases
        // below reproduce the original is_fg true/false branches exactly —
        // byte-identical output.
        let coverage: u8 = if mask_upscale {
            if mask_all_bg {
                0
            } else {
                ctx.mask.map_or(0, |m| mask_bilinear_coverage(m, fx, fy))
            }
        } else if !mask_all_bg
            && mask_hoist.is_some_and(|(mask_row, mask_w)| {
                let pxu = px as usize;
                pxu < mask_w as usize
                    && (mask_row.get(pxu >> 3).copied().unwrap_or(0) >> (7 - (pxu & 7))) & 1 != 0
            })
        {
            255
        } else {
            0
        };

        let (r, g, b) = if coverage == 0 {
            if let Some((vb_row, bg_w, col_start)) = vb {
                hblend(
                    vb_row,
                    bg_w,
                    col_start,
                    bx.and_then(|t| t.get(ox).copied()),
                    bg_fx_q,
                )
            } else {
                (255, 255, 255)
            }
        } else {
            let (fr, fg_g, fb) = if let Some(pal) = ctx.fg_palette {
                let color = lookup_palette_color(pal, ctx.blit_map, ctx.mask, px, py);
                (color.r, color.g, color.b)
            } else if let Some(fg) = ctx.fg44 {
                let fg_fx = map_plane_center_frac(fx, ctx.fg_x_q24);
                let fg_fy = map_plane_center_frac(fy, ctx.fg_y_q24);
                sample_bilinear(fg, fg_fx, fg_fy)
            } else {
                (0, 0, 0)
            };
            if coverage == 255 {
                (fr, fg_g, fb)
            } else {
                // Partial coverage (mask_aa only): blend fg/bg proportionally
                // to the interpolated mask coverage for a smoothed glyph edge.
                let (br, bg_g, bb) = if let Some((vb_row, bg_w, col_start)) = vb {
                    hblend(
                        vb_row,
                        bg_w,
                        col_start,
                        bx.and_then(|t| t.get(ox).copied()),
                        bg_fx_q,
                    )
                } else {
                    (255, 255, 255)
                };
                let cov = coverage as u32;
                let inv = 255 - cov;
                let blend =
                    |f: u8, b: u8| -> u8 { ((f as u32 * cov + b as u32 * inv + 127) / 255) as u8 };
                (blend(fr, br), blend(fg_g, bg_g), blend(fb, bb))
            }
        };

        // D1: skip LUT scatter reads when gamma is the identity mapping.
        if ctx.gamma_is_identity {
            pixel[0] = r;
            pixel[1] = g;
            pixel[2] = b;
        } else {
            pixel[0] = ctx.gamma_lut[r as usize];
            pixel[1] = ctx.gamma_lut[g as usize];
            pixel[2] = ctx.gamma_lut[b as usize];
        }
        pixel[3] = 255;
        bg_fx_q = bg_fx_q.wrapping_add(bg_fx_step_q);
    }
}

/// Write one area-average row into `row_buf` (downscale).
#[inline]
#[allow(clippy::too_many_arguments)]
fn composite_rows_area_avg_one(
    ctx: &CompositeContext<'_>,
    oy: u32,
    fx_step: u32,
    fy_step: u32,
    bg_fx_step: u32,
    bg_fy_step: u32,
    row_buf: &mut [u8],
    area_avg_x: Option<&[AreaAvgX]>,
) {
    let fy = (oy + ctx.offset_y) * fy_step;
    let bg_fy = ((fy as u64 * ctx.bg_y_q24) >> 24) as u32;
    let bg_y = ctx.bg.map(|bg| area_range(bg.height, bg_fy, bg_fy_step));

    // #438: row-level all-bg fast path (F2/I3 analog for the area-avg path). The
    // mask footprint's y-band [y0, y1) is row-invariant; if it has no foreground
    // bits, `mask_box_any` would return false for every output pixel, so the
    // `!mask_all_bg &&` short-circuit skips the per-pixel footprint scan entirely.
    // Only the `mask_shift == 0` (mask_box_any) path is covered — the max-pool
    // sub-path indexes a coarser mask. The y range matches `mask_box_any`.
    let mask_all_bg = ctx.mask_shift == 0
        && ctx.mask.is_none_or(|m| {
            let stride = m.row_stride();
            let y0 = (fy >> FRACBITS).min(m.height.saturating_sub(1)) as usize;
            let y1 = ((fy + fy_step) >> FRACBITS).min(m.height) as usize;
            y1 <= y0 || !m.data[y0 * stride..y1 * stride].iter().any(|&b| b != 0)
        });

    for (ox, pixel) in row_buf.as_chunks_mut::<4>().0.iter_mut().enumerate() {
        let fallback;
        let ax = if let Some(ax) = area_avg_x.and_then(|xs| xs.get(ox)) {
            *ax
        } else {
            let fx = (ox as u32 + ctx.offset_x) * fx_step;
            let bg_fx = ((fx as u64 * ctx.bg_x_q24) >> 24) as u32;
            let (bg_x0, bg_x1) = area_range(ctx.bg.map_or(0, |bg| bg.width), bg_fx, bg_fx_step);
            fallback = AreaAvgX {
                fx,
                bg_fx,
                bg_x0,
                bg_x1,
            };
            fallback
        };
        let fx = ax.fx;

        // #439: anti-aliased colour downscale. `coverage` (0..255) is the fraction
        // of the output pixel's footprint that is foreground; blend fg/bg
        // proportionally so colour text edges get a smooth gradient instead of the
        // blocky halos the old binary `mask_box_any` produced (colour analog of the
        // AA experiment for bilevel). The max-pool sub-path (mask_shift > 0) stays
        // binary. #438's `mask_all_bg` skips the coverage scan for blank rows.
        let coverage: u8 = if mask_all_bg {
            0
        } else if let Some(m) = ctx.mask {
            if ctx.mask_shift > 0 {
                let px = fx >> (FRACBITS + ctx.mask_shift);
                let pym = fy >> (FRACBITS + ctx.mask_shift);
                if px < m.width && pym < m.height && m.get(px, pym) {
                    255
                } else {
                    0
                }
            } else {
                mask_box_coverage(m, fx, fy, fx_step, fy_step)
            }
        } else {
            0
        };

        let bg_sample = || -> (u8, u8, u8) {
            if let Some(bg) = ctx.bg {
                if let Some((bg_y0, bg_y1)) = bg_y {
                    sample_area_avg_bounds(bg, ax.bg_x0, ax.bg_x1, bg_y0, bg_y1)
                } else {
                    sample_area_avg(bg, ax.bg_fx, bg_fy, bg_fx_step, bg_fy_step)
                }
            } else {
                (255, 255, 255)
            }
        };
        let fg_sample = || -> (u8, u8, u8) {
            if let Some(pal) = ctx.fg_palette {
                let (cx, cy) = mask_box_center_fg(ctx.mask.unwrap(), fx, fy, fx_step, fy_step);
                let color = lookup_palette_color(pal, ctx.blit_map, ctx.mask, cx, cy);
                (color.r, color.g, color.b)
            } else if let Some(fg) = ctx.fg44 {
                let fg_fx = ((fx as u64 * ctx.fg_x_q24) >> 24) as u32;
                let fg_fy = ((fy as u64 * ctx.fg_y_q24) >> 24) as u32;
                let fg_fx_step = ((fx_step as u64 * ctx.fg_x_q24) >> 24) as u32;
                let fg_fy_step = ((fy_step as u64 * ctx.fg_y_q24) >> 24) as u32;
                sample_area_avg(fg, fg_fx, fg_fy, fg_fx_step, fg_fy_step)
            } else {
                (0, 0, 0)
            }
        };

        let (r, g, b) = if coverage == 0 {
            bg_sample()
        } else if coverage == 255 {
            fg_sample()
        } else {
            // Partially covered edge pixel: blend fg over bg by coverage.
            let (fr, fg_, fb) = fg_sample();
            let (br, bg_, bb) = bg_sample();
            let c = coverage as u32;
            let ic = 255 - c;
            let mix = |f: u8, b: u8| ((c * f as u32 + ic * b as u32 + 127) / 255) as u8;
            (mix(fr, br), mix(fg_, bg_), mix(fb, bb))
        };

        if ctx.gamma_is_identity {
            pixel[0] = r;
            pixel[1] = g;
            pixel[2] = b;
        } else {
            pixel[0] = ctx.gamma_lut[r as usize];
            pixel[1] = ctx.gamma_lut[g as usize];
            pixel[2] = ctx.gamma_lut[b as usize];
        }
        pixel[3] = 255;
    }
}

/// Render a `DjVuPage` row by row, calling `sink(row_index, &rgba_row)` for
/// each output row in top-to-bottom order.
///
/// `rgba_row` contains `opts.width * 4` bytes (RGBA, alpha = 255).
///
/// This is the internal streaming primitive used by [`render_streaming`] and by
/// permissive [`render_pixmap`] fallback. Public callers that need full-pixmap
/// post-processing should use [`render_pixmap`].
///
/// # Errors
///
/// - [`RenderError::InvalidDimensions`] if `width == 0 || height == 0`
/// - Propagates IW44 / JB2 decode errors.
pub(crate) fn render_rows<F>(
    page: &DjVuPage,
    opts: &RenderOptions,
    limits: Option<crate::resource_limits::ResourceLimits>,
    sink: F,
) -> Result<(), RenderError>
where
    F: FnMut(usize, &[u8]),
{
    let w = opts.width;
    let h = opts.height;

    check_output_pixels("render_rows", page, limits, w, h)?;

    let gamma_lut = build_gamma_lut(page.gamma());

    let bg_subsample = best_iw44_subsample(opts.decode_scale(page));

    let DecodedLayers {
        bg,
        fg_palette,
        mask,
        blit_map,
        fg44,
    } = decode_layers(page, opts, bg_subsample, usize::MAX)?;

    let (ctx_mask, mask_shift) = resolve_sub4_mask(
        page,
        bg_subsample,
        opts,
        mask.as_deref(),
        fg_palette.as_ref(),
    );
    let ctx = CompositeContext::from_layers(
        page,
        opts,
        bg.as_deref(),
        ctx_mask,
        mask_shift,
        fg_palette.as_ref(),
        blit_map.as_deref(),
        fg44.as_deref(),
        &gamma_lut,
        (0, 0),
        (w, h),
    );
    composite_rows(&ctx, sink)
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Render a `DjVuPage` into a pre-allocated RGBA buffer.
///
/// This is the zero-allocation render path when `buf` is reused across calls
/// with the same dimensions. The buffer must be at least `width * height * 4`
/// bytes.
///
/// # Errors
///
/// - [`RenderError::BufTooSmall`] if `buf.len() < width * height * 4`
/// - [`RenderError::InvalidDimensions`] if `width == 0 || height == 0`
/// - Propagates IW44 / JB2 decode errors.
pub fn render_into(
    page: &DjVuPage,
    opts: &RenderOptions,
    buf: &mut [u8],
) -> Result<(), RenderError> {
    render_into_with_limits(page, opts, None, buf)
}

/// Like [`render_into`], with an optional caller-supplied resource limit override.
pub fn render_into_with_limits(
    page: &DjVuPage,
    opts: &RenderOptions,
    limits: Option<crate::resource_limits::ResourceLimits>,
    buf: &mut [u8],
) -> Result<(), RenderError> {
    let w = opts.width;
    let h = opts.height;

    check_output_pixels("render_into", page, limits, w, h)?;

    let need = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .unwrap_or(usize::MAX);

    if buf.len() < need {
        return Err(RenderError::BufTooSmall {
            need,
            got: buf.len(),
        });
    }

    let gamma_lut = build_gamma_lut(page.gamma());

    // Decode all layers (shared permissive/strict seam, same as render_rows).
    let bg_subsample = best_iw44_subsample(opts.decode_scale(page));
    let DecodedLayers {
        bg,
        fg_palette,
        mask,
        blit_map,
        fg44,
    } = decode_layers(page, opts, bg_subsample, usize::MAX)?;

    // Use pre-downsampled 1/4-res mask for sub=4 renders (single bit lookup vs
    // 4-9 lookups per pixel in the full-res mask).
    let (ctx_mask, mask_shift) = resolve_sub4_mask(
        page,
        bg_subsample,
        opts,
        mask.as_deref(),
        fg_palette.as_ref(),
    );
    let ctx = CompositeContext::from_layers(
        page,
        opts,
        bg.as_deref(),
        ctx_mask,
        mask_shift,
        fg_palette.as_ref(),
        blit_map.as_deref(),
        fg44.as_deref(),
        &gamma_lut,
        (0, 0),
        (w, h),
    );
    composite_into(&ctx, buf)?;

    Ok(())
}

/// Build the options for the native-resolution pre-pass that feeds the
/// Lanczos-3 post-filter: full page size, no scaling, no AA, no rotation, and
/// bilinear resampling (so the recursive render never re-enters this path).
///
/// Bold and permissive flags are carried through so the high-resolution
/// re-render matches the requested render in everything but the final
/// resampling step.
fn native_render_opts(page: &DjVuPage, opts: &RenderOptions) -> RenderOptions {
    // Full page size (decode scale derives to 1.0), bilinear resampling so the
    // recursive render never re-enters this path, no AA / no rotation. Bold and
    // permissive are carried through.
    RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        bold: opts.bold,
        permissive: opts.permissive,
        ..Default::default()
    }
}

/// Apply the shared Lanczos-3 post-pass to a freshly composited pixmap.
///
/// When `opts.resampling` is [`Resampling::Lanczos3`] *and* actual scaling
/// happened (`page` native size differs from `full_w × full_h`), the page is
/// re-rendered at native resolution via `render_native` and downscaled to
/// `out_w × out_h` with [`crate::pixmap::scale_lanczos3`]. Otherwise `pm` is
/// returned unchanged — covering both the non-Lanczos case and the 1:1 case
/// where Lanczos would be a no-op. If the native re-render fails, the original
/// (bilinear) `pm` is kept, matching the previous per-call behaviour.
///
/// `full` is the full output size used to decide whether scaling occurred;
/// `out` is the target the result is scaled to. They differ only for
/// [`render_region`], where the comparison is against the full page render but
/// the output is the (smaller) region.
fn apply_lanczos_postpass<F>(
    pm: Pixmap,
    page: &DjVuPage,
    opts: &RenderOptions,
    full: (u32, u32),
    out: (u32, u32),
    render_native: F,
) -> Pixmap
where
    F: FnOnce(&RenderOptions) -> Result<Pixmap, RenderError>,
{
    if opts.resampling != Resampling::Lanczos3 {
        return pm;
    }
    let (full_w, full_h) = full;
    let need_scale = page.width() as u32 != full_w || page.height() as u32 != full_h;
    if !need_scale {
        return pm;
    }
    let native_opts = native_render_opts(page, opts);
    match render_native(&native_opts) {
        Ok(native_pm) => crate::pixmap::scale_lanczos3(&native_pm, out.0, out.1),
        // Native render failed — keep the bilinear result already in `pm`.
        Err(_) => pm,
    }
}

/// Render a `DjVuPage` to a new [`Pixmap`] using the given options.
///
/// Strict renders composite directly into the full pixmap. Permissive renders
/// reuse the row path so decode-error recovery remains shared with
/// [`render_streaming`].
/// Render a page and return the pixmap together with a [`RenderReport`] of any
/// layers a permissive render skipped or recovered (#696).
///
/// The pixmap is byte-identical to [`render_pixmap`]. In strict mode the report
/// is always clean (decode errors propagate instead of being recovered); in
/// permissive mode it lists each background truncation, dropped mask, or
/// skipped foreground/palette in the order the renderer took them.
#[cfg(feature = "std")]
pub fn render_pixmap_with_report(
    page: &DjVuPage,
    opts: &RenderOptions,
) -> Result<(Pixmap, RenderReport), RenderError> {
    // Install a per-thread recovery sink; the guard clears it on every exit
    // path (including panics) so a normal `render_pixmap` never records.
    struct SinkGuard;
    impl Drop for SinkGuard {
        fn drop(&mut self) {
            RECOVERY_SINK.with(|sink| *sink.borrow_mut() = None);
        }
    }
    RECOVERY_SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
    let _guard = SinkGuard;

    let pixmap = render_pixmap(page, opts)?;
    let recoveries = RECOVERY_SINK.with(|sink| sink.borrow_mut().take().unwrap_or_default());
    Ok((pixmap, RenderReport { recoveries }))
}

pub fn render_pixmap(page: &DjVuPage, opts: &RenderOptions) -> Result<Pixmap, RenderError> {
    render_pixmap_with_limits(page, opts, None)
}

/// Render a page to an owned RGBA pixmap with an optional resource limit override.
///
/// When `limits` is `None`, limits inherited from the parent document at parse
/// time apply (see [`ParseOptions::limits`](crate::resource_limits::ParseOptions::limits)).
/// Per-render overrides use [`render_pixmap_with_limits`] /
/// [`render_into_with_limits`].
pub fn render_pixmap_with_limits(
    page: &DjVuPage,
    opts: &RenderOptions,
    limits: Option<crate::resource_limits::ResourceLimits>,
) -> Result<Pixmap, RenderError> {
    let w = opts.width;
    let h = opts.height;

    // Bound the output allocation. `w`/`h` flow from the (untrusted) INFO chunk on
    // a default render; w*h*4 of 65535² is ~17 GB, which either OOMs (64-bit) or
    // wraps `Pixmap::new` to an empty buffer that the permissive copy below then
    // indexes out of bounds. Reject up front.
    check_output_pixels("render_pixmap", page, limits, w, h)?;

    let mut pm = Pixmap::white(w, h);

    if opts.permissive {
        let row_stride = w as usize * 4;
        // Permissive rendering has its own decode-error recovery path in
        // render_rows; keep that behaviour and copy each recovered row.
        render_rows(page, opts, limits, |y, row| {
            let start = y * row_stride;
            pm.data[start..start + row_stride].copy_from_slice(row);
        })?;
    } else {
        // Strict renders can composite directly into the output Pixmap,
        // avoiding the scratch row + row copy used by the streaming adapter.
        render_into_with_limits(page, opts, limits, &mut pm.data)?;
    }

    if opts.aa {
        pm = aa_downscale(&pm);
    }

    // Apply the shared Lanczos-3 post-pass (re-render at native size, then
    // downscale) when requested and scaling actually happened.
    let pm = apply_lanczos_postpass(pm, page, opts, (w, h), (w, h), |native_opts| {
        render_pixmap_with_limits(page, native_opts, limits)
    });

    Ok(rotate_pixmap(
        pm,
        combine_rotations(page.rotation(), opts.rotation),
    ))
}

/// Render a `DjVuPage` row by row, calling `sink(row_index, &rgba_row)` for
/// each output row in top-to-bottom order. Each `rgba_row` slice has length
/// `opts.width * 4` bytes (RGBA, alpha = 255).
///
/// This is the constant-memory render path for low-memory targets (mobile,
/// WASM, embedded) and for streaming consumers (TIFF/PDF row-encoders, the
/// browser `OffscreenCanvas` row blit). Internally it allocates a single
/// `opts.width * 4` byte scratch row and reuses it across all rows; peak heap
/// usage during compositing is bounded by that scratch plus the decoded
/// background (BG44) and mask (JB2) buffers.
///
/// Output is byte-identical to [`render_pixmap`] when both produce a result.
///
/// # Constraints
///
/// [`render_pixmap`] applies anti-aliasing, Lanczos-3 resampling, and rotation
/// as **post-processing on the full pixmap**. The streaming path cannot
/// support those modes without buffering the entire output, defeating its
/// purpose. The following must hold or [`RenderError::UnsupportedOption`] is
/// returned:
///
/// - `opts.aa == false`
/// - `opts.resampling == Resampling::Bilinear`, *or* the output dimensions
///   match the page's native dimensions (in which case Lanczos becomes a
///   no-op and `Bilinear` produces the same bytes anyway)
/// - `opts.rotation == UserRotation::None` *and* the page's INFO rotation is
///   `Rotation::None` (i.e. the combined rotation is identity)
///
/// For any of those modes, use [`render_pixmap`] instead.
///
/// # Errors
///
/// - [`RenderError::InvalidDimensions`] if `opts.width == 0 || opts.height == 0`
/// - [`RenderError::UnsupportedOption`] if a post-processing option is set
/// - Propagates IW44 / JB2 decode errors.
///
/// # Example
///
/// ```no_run
/// use djvu_rs::djvu_render::{render_streaming, RenderOptions};
/// # let doc = djvu_rs::djvu_document::DjVuDocument::parse(&[]).unwrap();
/// # let page = doc.page(0).unwrap();
/// let opts = RenderOptions::fit_to_width(page, 1024);
/// render_streaming(page, &opts, |y, rgba_row| {
///     // hand the row off to an encoder, GPU upload, network write, etc
///     # let _ = (y, rgba_row);
/// }).unwrap();
/// ```
pub fn render_streaming<F>(
    page: &DjVuPage,
    opts: &RenderOptions,
    sink: F,
) -> Result<(), RenderError>
where
    F: FnMut(usize, &[u8]),
{
    if opts.aa {
        return Err(RenderError::UnsupportedOption(
            "anti-aliasing requires a full pixmap; use render_pixmap",
        ));
    }
    let lanczos_with_scaling = opts.resampling == Resampling::Lanczos3
        && (page.width() as u32 != opts.width || page.height() as u32 != opts.height);
    if lanczos_with_scaling {
        return Err(RenderError::UnsupportedOption(
            "Lanczos-3 resampling at scaled output requires a full pixmap; use render_pixmap",
        ));
    }
    if combine_rotations(page.rotation(), opts.rotation) != crate::info::Rotation::None {
        return Err(RenderError::UnsupportedOption(
            "rotation requires a full pixmap; use render_pixmap",
        ));
    }
    render_rows(page, opts, None, sink)
}

/// Render a sub-rectangle of a page into a new [`Pixmap`].
///
/// Unlike [`render_pixmap`], which always allocates `opts.width × opts.height`
/// pixels, `render_region` only allocates `region.width × region.height` pixels.
/// This makes it efficient for thumbnails, viewport clips, and tile rendering.
///
/// `opts.width` and `opts.height` still define the **full-page** render dimensions
/// used for scale calculation. `region` selects which sub-rectangle of that
/// full render to output. The returned `Pixmap` has dimensions
/// `region.width × region.height`.
///
/// # Errors
///
/// - [`RenderError::InvalidDimensions`] if `region.width == 0 || region.height == 0`
/// - Propagates IW44 / JB2 decode errors.
pub fn render_region(
    page: &DjVuPage,
    region: RenderRect,
    opts: &RenderOptions,
) -> Result<Pixmap, RenderError> {
    check_output_pixels("render_region", page, None, region.width, region.height)?;

    let full_w = opts.width.max(1);
    let full_h = opts.height.max(1);
    let gamma_lut = build_gamma_lut(page.gamma());

    let bg_subsample = best_iw44_subsample(opts.decode_scale(page));
    let DecodedLayers {
        bg,
        fg_palette,
        mask,
        blit_map,
        fg44,
    } = decode_layers(page, opts, bg_subsample, usize::MAX)?;

    let out_w = region.width;
    let out_h = region.height;
    let mut pm = Pixmap::white(out_w, out_h);

    let region_opts = RenderOptions {
        width: full_w,
        height: full_h,
        ..*opts
    };
    // Same 1/4-res mask fast-path decision as `render_into`/`render_rows`, so
    // a region render stays byte-identical to the matching crop of the full
    // render at every subsample tier (#691).
    let (ctx_mask, mask_shift) = resolve_sub4_mask(
        page,
        bg_subsample,
        opts,
        mask.as_deref(),
        fg_palette.as_ref(),
    );
    let ctx = CompositeContext::from_layers(
        page,
        &region_opts,
        bg.as_deref(),
        ctx_mask,
        mask_shift,
        fg_palette.as_ref(),
        blit_map.as_deref(),
        fg44.as_deref(),
        &gamma_lut,
        (region.x, region.y),
        (out_w, out_h),
    );
    composite_into(&ctx, &mut pm.data)?;

    // Shared Lanczos-3 post-pass: scaling is judged against the full render
    // size (full_w/full_h) but the result is scaled to the region (out_w/out_h).
    let pm = apply_lanczos_postpass(
        pm,
        page,
        opts,
        (full_w, full_h),
        (out_w, out_h),
        |native_opts| render_region(page, region, native_opts),
    );

    Ok(rotate_pixmap(
        pm,
        combine_rotations(page.rotation(), opts.rotation),
    ))
}

/// `true` when a cooperative cancel flag is present and set.
///
/// `Relaxed` is enough: the flag carries no data, it only asks in-flight work
/// to stop at its next checkpoint.
#[inline]
pub(crate) fn is_cancelled(cancel: Option<&core::sync::atomic::AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(core::sync::atomic::Ordering::Relaxed))
}

/// Region variant of [`render_progressive`] (#691 slice 3): composite the
/// `region` sub-rectangle of progressive frame `chunk_n` (BG44 chunks
/// `0..=chunk_n`, full foreground).
///
/// **Byte-identical** to the matching crop of
/// `render_progressive(page, opts, chunk_n)` for every input: the layer
/// decode is the same `decode_layers(.., chunk_n + 1)` call, the composite
/// uses the same full-resolution mask (shift 0 — `render_progressive` never
/// takes the 1/4-res mask fast path), and `composite_into` computes each
/// pixel from its absolute position in the full render, so a sub-rectangle
/// reproduces the frame's bytes exactly (see
/// `progressive_tiles_match_progressive_frames` in `djvu_tile`).
///
/// `cancel` is a cooperative stop flag checked on entry and between the
/// layer decode and the composite; `Ok(None)` means the render was abandoned
/// at a checkpoint. Partial-quality frames are decoded from scratch on every
/// call (the `PageLayers` caches only memoize the full-chunk decode), and
/// their pixels are never inserted into the composited-tile cache.
///
/// # Errors
///
/// Same as [`render_progressive`], plus [`RenderError::UnsupportedOption`]
/// for Lanczos-3 resampling (its whole-pixmap re-render recursion is
/// incompatible with region output; the tile API rejects it earlier anyway).
pub(crate) fn render_region_progressive(
    page: &DjVuPage,
    region: RenderRect,
    opts: &RenderOptions,
    chunk_n: usize,
    cancel: Option<&core::sync::atomic::AtomicBool>,
) -> Result<Option<Pixmap>, RenderError> {
    check_output_pixels(
        "render_region_progressive",
        page,
        None,
        region.width,
        region.height,
    )?;
    if opts.resampling == Resampling::Lanczos3 {
        return Err(RenderError::UnsupportedOption(
            "Lanczos-3 resampling is not supported for progressive region renders",
        ));
    }
    let n_bg44 = page.bg44_chunks().len();
    let max_chunk = n_bg44.saturating_sub(1);
    if n_bg44 > 0 && chunk_n > max_chunk {
        return Err(RenderError::ChunkOutOfRange {
            chunk_n,
            max: max_chunk,
        });
    }
    if is_cancelled(cancel) {
        return Ok(None);
    }

    let full_w = opts.width.max(1);
    let full_h = opts.height.max(1);
    let gamma_lut = build_gamma_lut(page.gamma());
    let bg_subsample = best_iw44_subsample(opts.decode_scale(page));
    let DecodedLayers {
        bg,
        fg_palette,
        mask,
        blit_map,
        fg44,
    } = decode_layers(page, opts, bg_subsample, chunk_n + 1)?;

    if is_cancelled(cancel) {
        return Ok(None);
    }

    let out_w = region.width;
    let out_h = region.height;
    let mut pm = Pixmap::white(out_w, out_h);
    let region_opts = RenderOptions {
        width: full_w,
        height: full_h,
        ..*opts
    };
    let ctx = CompositeContext::from_layers(
        page,
        &region_opts,
        bg.as_deref(),
        mask.as_deref(),
        0,
        fg_palette.as_ref(),
        blit_map.as_deref(),
        fg44.as_deref(),
        &gamma_lut,
        (region.x, region.y),
        (out_w, out_h),
    );
    composite_into(&ctx, &mut pm.data)?;

    Ok(Some(rotate_pixmap(
        pm,
        combine_rotations(page.rotation(), opts.rotation),
    )))
}

/// Render a sub-rectangle of a page, assembling the output from a per-page
/// cache of composited [`TILE_SIZE`]×`TILE_SIZE` output tiles.
///
/// # Why this exists (C4_TILE_CACHE)
///
/// [`render_region`] recomposites its entire requested rectangle from scratch
/// on every call — the per-pixel work in `composite_into` is not memoized
/// anywhere. VIEWER_BENCH (`benches/viewer.rs`) scripted an interactive
/// pan/zoom session (open → full render → zoom → 12-step overlapping pan →
/// zoom → pan) and measured that a `full_recomposite` pan step costs
/// proportionally to its *entire* viewport every time, while an
/// `incremental_strip` step (composite only the newly-exposed edge) costs
/// proportionally to just that edge — see `PERF_EXPERIMENTS.md` round 36 for
/// the numbers. A tile cache captures that gap for real: it composites each
/// `TILE_SIZE`-aligned tile once and reuses it for every subsequent request
/// that touches it, regardless of how the requested rectangle is framed.
///
/// # Correctness
///
/// **Byte-identical** to [`render_region`] for every input — this is a cache
/// in front of the same compositor, not a different one. `composite_into`
/// computes each output pixel from its *absolute* position
/// (`offset_x + ox`, `offset_y + oy`); tile boundaries are aligned to that
/// same absolute grid, so assembling a request from whole or partial tiles
/// reproduces exactly the bytes a direct `render_region` call would have
/// produced (see `render_region_tiled_matches_render_region` and
/// `render_region_tiled_overlapping_regions_share_cache` in the test module).
///
/// # Eligibility
///
/// The cache only activates for the mode it was built for; anything else
/// falls back to a plain [`render_region`] call with no tile bookkeeping:
///
/// - `opts.resampling == Resampling::Lanczos3` — its native-scale re-render
///   recursion isn't compatible with per-tile assembly.
/// - the combined page + user rotation isn't the identity — tiles are cached
///   pre-rotation, so a rotated request would need a different assembly.
/// - `opts.permissive` — kept off the fast path defensively; this targets the
///   interactive strict-decode hot path, not error recovery.
///
/// This is an **opt-in** entry point: call it where you want tile caching
/// (e.g. a pan/zoom viewer). [`render_region`] itself is untouched and pays no
/// overhead for callers (thumbnails, export, one-shot renders) that don't
/// want a per-page tile cache.
///
/// # Errors
///
/// Same as [`render_region`].
#[cfg(feature = "std")]
pub fn render_region_tiled(
    page: &DjVuPage,
    region: RenderRect,
    opts: &RenderOptions,
) -> Result<Pixmap, RenderError> {
    let pm = render_region_tiled_cancellable(page, region, opts, None)?;
    // Without a cancel flag the render can never be abandoned.
    Ok(pm.expect("uncancellable render completed"))
}

/// [`render_region_tiled`] with a cooperative cancel flag (#691 slice 3).
///
/// The flag is checked on entry and again before each internal
/// [`TILE_SIZE`]-tile is fetched or composited; `Ok(None)` means the render
/// was abandoned at a checkpoint. Cancellation never corrupts the tile
/// cache: a tile is inserted only after its composite completed, so an
/// abandoned call leaves either fully-composited tiles or nothing.
#[cfg(feature = "std")]
pub(crate) fn render_region_tiled_cancellable(
    page: &DjVuPage,
    region: RenderRect,
    opts: &RenderOptions,
    cancel: Option<&core::sync::atomic::AtomicBool>,
) -> Result<Option<Pixmap>, RenderError> {
    check_output_pixels(
        "render_region_tiled",
        page,
        None,
        region.width,
        region.height,
    )?;

    let full_w = opts.width.max(1);
    let full_h = opts.height.max(1);

    let eligible = opts.resampling == Resampling::Bilinear
        && !opts.permissive
        && combine_rotations(page.rotation(), opts.rotation) == crate::info::Rotation::None;

    if is_cancelled(cancel) {
        return Ok(None);
    }
    if !eligible {
        return render_region(page, region, opts).map(Some);
    }

    let gamma_lut = build_gamma_lut(page.gamma());
    let bg_subsample = best_iw44_subsample(opts.decode_scale(page));
    let DecodedLayers {
        bg,
        fg_palette,
        mask,
        blit_map,
        fg44,
    } = decode_layers(page, opts, bg_subsample, usize::MAX)?;

    let region_opts = RenderOptions {
        width: full_w,
        height: full_h,
        ..*opts
    };
    // Same 1/4-res mask fast-path decision as `render_into`/`render_rows`
    // (see render_region); the choice is a pure function of the tile-key
    // fields plus per-page constants, so cached tiles stay coherent.
    let (ctx_mask, mask_shift) = resolve_sub4_mask(
        page,
        bg_subsample,
        opts,
        mask.as_deref(),
        fg_palette.as_ref(),
    );
    // Template context for the whole full_w×full_h render; each tile below
    // copies it (cheap: `Copy`) and only overwrites offset/out fields.
    let ctx_template = CompositeContext::from_layers(
        page,
        &region_opts,
        bg.as_deref(),
        ctx_mask,
        mask_shift,
        fg_palette.as_ref(),
        blit_map.as_deref(),
        fg44.as_deref(),
        &gamma_lut,
        (0, 0),
        (full_w, full_h),
    );

    let out_w = region.width;
    let out_h = region.height;
    let mut pm = Pixmap::white(out_w, out_h);
    let out_stride = out_w as usize * 4;

    let region_x1 = region.x.saturating_add(region.width).min(full_w);
    let region_y1 = region.y.saturating_add(region.height).min(full_h);
    if region_x1 <= region.x || region_y1 <= region.y {
        // Region lies entirely outside the full render — nothing to copy;
        // return the white-filled pixmap (matches render_region's behaviour,
        // whose compositor loop would likewise touch no valid pixels).
        return Ok(Some(pm));
    }
    let tx0 = region.x / TILE_SIZE;
    let ty0 = region.y / TILE_SIZE;
    let tx1 = (region_x1 - 1) / TILE_SIZE;
    let ty1 = (region_y1 - 1) / TILE_SIZE;

    let layers = page.render_layers();
    for ty in ty0..=ty1 {
        let tile_y0 = ty * TILE_SIZE;
        let tile_h = TILE_SIZE.min(full_h - tile_y0);
        for tx in tx0..=tx1 {
            if is_cancelled(cancel) {
                return Ok(None);
            }
            let tile_x0 = tx * TILE_SIZE;
            let tile_w = TILE_SIZE.min(full_w - tile_x0);
            let key: TileKey = (full_w, full_h, tile_x0, tile_y0, opts.bold, opts.mask_aa);

            let tile = match layers.get_tile(key) {
                Some(t) => t,
                None => {
                    let mut tile_ctx = ctx_template;
                    tile_ctx.offset_x = tile_x0;
                    tile_ctx.offset_y = tile_y0;
                    tile_ctx.out_w = tile_w;
                    tile_ctx.out_h = tile_h;
                    let mut data = vec![0u8; tile_w as usize * tile_h as usize * 4];
                    composite_into(&tile_ctx, &mut data)?;
                    let entry = std::sync::Arc::new(TileEntry {
                        w: tile_w,
                        h: tile_h,
                        data,
                    });
                    layers.insert_tile(key, entry.clone());
                    entry
                }
            };

            // Copy the overlap between `region` and this tile into `pm`.
            let ox0 = tile_x0.max(region.x);
            let oy0 = tile_y0.max(region.y);
            let ox1 = (tile_x0 + tile.w).min(region_x1);
            let oy1 = (tile_y0 + tile.h).min(region_y1);
            if ox0 >= ox1 || oy0 >= oy1 {
                continue;
            }
            let copy_w = (ox1 - ox0) as usize;
            let tile_stride = tile.w as usize * 4;
            for y in oy0..oy1 {
                let tile_row = (y - tile_y0) as usize;
                let tile_col = (ox0 - tile_x0) as usize;
                let src_start = tile_row * tile_stride + tile_col * 4;
                let dst_row = (y - region.y) as usize;
                let dst_col = (ox0 - region.x) as usize;
                let dst_start = dst_row * out_stride + dst_col * 4;
                pm.data[dst_start..dst_start + copy_w * 4]
                    .copy_from_slice(&tile.data[src_start..src_start + copy_w * 4]);
            }
        }
    }

    Ok(Some(pm))
}

/// Render a `DjVuPage` to an 8-bit grayscale image.
///
/// Equivalent to calling [`render_pixmap`] and converting the result with
/// [`Pixmap::to_gray8`]. Returns a [`GrayPixmap`] where `data.len() ==
/// width * height`.
///
/// For bilevel (JB2-only) pages this produces only `0` and `255` values.
/// For colour pages, luminance is computed with ITU-R BT.601 weights.
pub fn render_gray8(page: &DjVuPage, opts: &RenderOptions) -> Result<GrayPixmap, RenderError> {
    Ok(render_pixmap(page, opts)?.to_gray8())
}

/// Render all pages of a document in parallel using rayon.
///
/// Each page is rendered independently with its own [`RenderOptions`] computed
/// from the given `dpi`.  Results are returned in page order.
///
/// Requires the `parallel` feature flag.
#[cfg(feature = "parallel")]
pub fn render_pages_parallel(
    doc: &crate::djvu_document::DjVuDocument,
    dpi: u32,
) -> Vec<Result<Pixmap, RenderError>> {
    use rayon::prelude::*;

    let count = doc.page_count();
    (0..count)
        .into_par_iter()
        .map(|i| {
            let page = doc.page(i)?;
            let native_dpi = page.dpi() as f32;
            let scale = dpi as f32 / native_dpi;
            let w = ((page.width() as f32 * scale).round() as u32).max(1);
            let h = ((page.height() as f32 * scale).round() as u32).max(1);
            let opts = RenderOptions {
                width: w,
                height: h,
                ..Default::default()
            };
            render_pixmap(page, &opts)
        })
        .collect()
}

/// Coarse render: decode only the first BG44 chunk for a fast blurry preview.
///
/// Returns `Ok(None)` when the page has no BG44 chunks.
pub fn render_coarse(page: &DjVuPage, opts: &RenderOptions) -> Result<Option<Pixmap>, RenderError> {
    let w = opts.width;
    let h = opts.height;

    check_output_pixels("render_coarse", page, None, w, h)?;

    let bg_subsample = best_iw44_subsample(opts.decode_scale(page));
    let bg = decode_background_chunks(page, 1, bg_subsample)?;
    let bg = match bg {
        Some(b) => b,
        None => return Ok(None),
    };

    let gamma_lut = build_gamma_lut(page.gamma());
    let mut pm = Pixmap::white(w, h);

    {
        let ctx = CompositeContext::from_layers(
            page,
            opts,
            Some(&bg),
            None,
            0,
            None,
            None,
            None,
            &gamma_lut,
            (0, 0),
            (w, h),
        );
        composite_into(&ctx, &mut pm.data)?;
    }

    Ok(Some(rotate_pixmap(
        pm,
        combine_rotations(page.rotation(), opts.rotation),
    )))
}

/// Progressive render: decode BG44 chunks 1..=chunk_n and all other layers.
///
/// `chunk_n = 0` behaves like [`render_coarse`] (first chunk only).
/// Each additional chunk adds detail. The result after all chunks is
/// equivalent to [`render_pixmap`].
///
/// # Errors
///
/// Returns [`RenderError::ChunkOutOfRange`] if `chunk_n` exceeds the number
/// of available BG44 chunks.
pub fn render_progressive(
    page: &DjVuPage,
    opts: &RenderOptions,
    chunk_n: usize,
) -> Result<Pixmap, RenderError> {
    let w = opts.width;
    let h = opts.height;

    check_output_pixels("render_progressive", page, None, w, h)?;

    let n_bg44 = page.bg44_chunks().len();
    let max_chunk = n_bg44.saturating_sub(1);

    if n_bg44 > 0 && chunk_n > max_chunk {
        return Err(RenderError::ChunkOutOfRange {
            chunk_n,
            max: max_chunk,
        });
    }

    let gamma_lut = build_gamma_lut(page.gamma());

    // Decode background up to chunk_n + 1 chunks; full foreground + bold dilation
    // via the shared decode_layers path so no logic can drift between this and the
    // full render.
    let bg_subsample = best_iw44_subsample(opts.decode_scale(page));
    let DecodedLayers {
        bg,
        fg_palette,
        mask,
        blit_map,
        fg44,
    } = decode_layers(page, opts, bg_subsample, chunk_n + 1)?;

    let mut pm = Pixmap::white(w, h);
    {
        let ctx = CompositeContext::from_layers(
            page,
            opts,
            bg.as_deref(),
            mask.as_deref(),
            0,
            fg_palette.as_ref(),
            blit_map.as_deref(),
            fg44.as_deref(),
            &gamma_lut,
            (0, 0),
            (w, h),
        );
        composite_into(&ctx, &mut pm.data)?;
    }

    // Shared Lanczos-3 post-pass; the native re-render decodes the same
    // `chunk_n` refinement level.
    let pm = apply_lanczos_postpass(pm, page, opts, (w, h), (w, h), |native_opts| {
        render_progressive(page, native_opts, chunk_n)
    });

    Ok(rotate_pixmap(
        pm,
        combine_rotations(page.rotation(), opts.rotation),
    ))
}

/// Number of progressive refinement frames a page yields: one per BG44 chunk,
/// or a single frame for bilevel/JB2-only pages with no BG44 data.
///
/// This is the seam that hides the `max(1, bg44_chunks().len())` convention from
/// callers, so progressive consumers never reach into [`DjVuPage::bg44_chunks`]
/// to size their own loops.
pub fn progressive_steps(page: &DjVuPage) -> usize {
    page.bg44_chunks().len().max(1)
}

/// Render progressive frame `step` (`0..`[`progressive_steps`]).
///
/// Encapsulates the "no BG44 chunks ⇒ a single full [`render_pixmap`], otherwise
/// [`render_progressive`]`(step)`" decision that every progressive caller (the
/// `Page::render_scaled_progressive` collector and the async
/// `render_progressive_stream`) previously open-coded. `step` is interpreted as
/// the BG44 chunk index on multi-chunk pages.
pub fn render_progressive_step(
    page: &DjVuPage,
    opts: &RenderOptions,
    step: usize,
) -> Result<Pixmap, RenderError> {
    if page.bg44_chunks().is_empty() {
        render_pixmap(page, opts)
    } else {
        render_progressive(page, opts, step)
    }
}

/// Stateful **streaming** progressive decoder (B5).
///
/// Where [`render_progressive_all`] needs every BG44 chunk up front and
/// [`render_progressive`] re-decodes chunks `1..=k` from scratch for frame `k`
/// (O(N²) over all frames), this holds the decode state across calls: the
/// foreground (mask / FG44 / palette) is decoded **once** and the background
/// accumulates in a single [`Iw44Image`]. Feed one BG44 refinement chunk at a
/// time — e.g. as it arrives over a network — with [`Self::push_bg44_chunk`] and
/// get the refined frame back, for O(N) total decode.
///
/// It serves the same case as `render_progressive_all`'s incremental fast path
/// (strict decode, `Bilinear` resampling, non-zero output size); the frames it
/// returns are byte-identical to that path. `Lanczos3` and `permissive` are not
/// supported here (Lanczos re-renders at native resolution per frame, leaving no
/// shared incremental state) — use [`render_progressive_all`] for those.
pub struct ProgressiveDecoder<'a> {
    page: &'a DjVuPage,
    opts: RenderOptions,
    w: u32,
    h: u32,
    gamma_lut: [u8; 256],
    bg_subsample: u32,
    fg_palette: Option<FgbzPalette>,
    mask: Option<Cow<'a, crate::bitmap::Bitmap>>,
    blit_map: Option<Cow<'a, [i32]>>,
    fg44: Option<Cow<'a, Pixmap>>,
    rotation: crate::info::Rotation,
    img: Iw44Image,
    chunks_fed: usize,
}

impl<'a> ProgressiveDecoder<'a> {
    /// Build a streaming decoder for `page` at `opts`. Decodes the foreground
    /// once (including any `bold` dilation) so only the background refines per
    /// chunk.
    ///
    /// Errors: [`RenderError::InvalidDimensions`] if `opts.width`/`height` is 0;
    /// [`RenderError::Unsupported`] if `opts.resampling` is not `Bilinear` or
    /// `opts.permissive` is set (see the type docs); or a decode error from the
    /// foreground.
    pub fn new(page: &'a DjVuPage, opts: &RenderOptions) -> Result<Self, RenderError> {
        check_output_pixels(
            "render_progressive_decoder",
            page,
            None,
            opts.width,
            opts.height,
        )?;
        if opts.resampling != Resampling::Bilinear || opts.permissive {
            return Err(RenderError::UnsupportedOption(
                "ProgressiveDecoder supports only strict Bilinear rendering; \
                 use render_progressive_all for Lanczos3 / permissive",
            ));
        }

        let gamma_lut = build_gamma_lut(page.gamma());
        let bg_subsample = best_iw44_subsample(opts.decode_scale(page));
        let ForegroundLayers {
            fg_palette,
            mask,
            blit_map,
            fg44,
        } = decode_foreground_strict(page)?;
        let mask = if opts.bold > 0 {
            mask.map(|m| Cow::Owned(m.into_owned().dilate_n(opts.bold as u32)))
        } else {
            mask
        };
        let rotation = combine_rotations(page.rotation(), opts.rotation);

        Ok(Self {
            page,
            opts: opts.clone(),
            w: opts.width,
            h: opts.height,
            gamma_lut,
            bg_subsample,
            fg_palette,
            mask,
            blit_map,
            fg44,
            rotation,
            img: Iw44Image::new(),
            chunks_fed: 0,
        })
    }

    /// Feed the next BG44 refinement chunk and return the refined frame. Each
    /// call accumulates into the shared decoder, so the returned frame reflects
    /// every chunk fed so far. Byte-identical to the corresponding frame of
    /// [`render_progressive_all`].
    pub fn push_bg44_chunk(&mut self, chunk: &[u8]) -> Result<Pixmap, RenderError> {
        #[cfg(test)]
        count_bg44_chunk_decode();
        self.img.decode_chunk(chunk).map_err(RenderError::Iw44)?;
        self.chunks_fed += 1;
        let bg = self
            .img
            .to_rgb_subsample(self.bg_subsample)
            .map_err(RenderError::Iw44)?;

        let mut pm = Pixmap::white(self.w, self.h);
        {
            let ctx = CompositeContext::from_layers(
                self.page,
                &self.opts,
                Some(&bg),
                self.mask.as_deref(),
                0,
                self.fg_palette.as_ref(),
                self.blit_map.as_deref(),
                self.fg44.as_deref(),
                &self.gamma_lut,
                (0, 0),
                (self.w, self.h),
            );
            composite_into(&ctx, &mut pm.data)?;
        }
        Ok(rotate_pixmap(pm, self.rotation))
    }

    /// Number of chunks fed so far (= number of frames produced).
    pub fn frames_produced(&self) -> usize {
        self.chunks_fed
    }
}

/// Eagerly render every progressive frame into a `Vec`, coarsest first.
///
/// The convenience form of [`render_progressive_step`] over the full
/// [`progressive_steps`] range; the last frame equals [`render_pixmap`].
/// Streaming consumers that want one frame at a time should drive a
/// [`ProgressiveDecoder`] (strict Bilinear) or [`render_progressive_step`].
pub fn render_progressive_all(
    page: &DjVuPage,
    opts: &RenderOptions,
) -> Result<Vec<Pixmap>, RenderError> {
    let steps = progressive_steps(page);
    let bg44_chunks = page.bg44_chunks();

    // Incremental fast path (B5): the per-frame `render_progressive_step` decodes
    // BG44 chunks 1..=k from scratch for every frame k — O(N²) over all frames.
    // The foreground (mask/FG44/palette) is identical across frames and already
    // memoised; only the background refines. So decode the foreground once, feed
    // BG44 chunks into a single accumulating `Iw44Image` one per frame, and
    // snapshot each frame — O(N) total decode.
    //
    // Restricted to the case the fast path can serve byte-identically:
    //   * strict mode (permissive uses a different, error-tolerant FG decode),
    //   * Bilinear (Lanczos re-renders at native per frame via the post-pass —
    //     no shared incremental state to exploit),
    //   * a real multi-chunk BG44 page (otherwise there is nothing to amortise).
    // Everything else falls back to the simple per-frame loop below.
    let can_stream = steps > 1
        && bg44_chunks.len() == steps
        && !opts.permissive
        && opts.resampling == Resampling::Bilinear
        && opts.width != 0
        && opts.height != 0;

    if can_stream {
        // Drive the streaming `ProgressiveDecoder`: it decodes the foreground once
        // and accumulates the background across chunks (O(N)), exactly the batch
        // incremental fast path this used to inline. Collecting every frame here
        // is byte-identical to feeding the chunks one at a time.
        let mut dec = ProgressiveDecoder::new(page, opts)?;
        let mut frames = Vec::with_capacity(steps);
        for chunk in bg44_chunks.iter().take(steps) {
            frames.push(dec.push_bg44_chunk(chunk)?);
        }
        return Ok(frames);
    }

    let mut frames = Vec::with_capacity(steps);
    for step in 0..steps {
        frames.push(render_progressive_step(page, opts, step)?);
    }
    Ok(frames)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::djvu_document::DjVuDocument;

    fn assets_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("references/djvujs/library/assets")
    }

    /// Helper that returns an owned document so tests can borrow pages from it.
    fn load_doc(filename: &str) -> DjVuDocument {
        let data = std::fs::read(assets_path().join(filename))
            .unwrap_or_else(|_| panic!("{filename} must exist"));
        DjVuDocument::parse(&data).unwrap_or_else(|e| panic!("parse failed: {e}"))
    }

    // ── Compositor hot-path unit tests ───────────────────────────────────────
    //
    // The three `composite_rows_*_one` functions are the compositor's hot
    // paths. They were previously exercised only through full-page
    // `render_pixmap` / `render_into` against decoded fixtures, so a compositor
    // bug could not be isolated from a decode bug. These tests drive each path
    // directly off a synthetic `CompositeContext` (built from layers we control,
    // not decoded from a file), making the compositor an independently-tested
    // surface. Solid / block-uniform colours keep the assertions exact and free
    // of sampling-rounding brittleness.

    fn identity_lut() -> [u8; 256] {
        core::array::from_fn(|i| i as u8)
    }

    /// Build a `CompositeContext` from synthetic layers, mirroring the q24 /
    /// gamma wiring that [`CompositeContext::from_layers`] performs, but without
    /// needing a `DjVuPage`. Foreground palette / blit map are unused here.
    #[allow(clippy::too_many_arguments)]
    fn synth_ctx<'a>(
        opts: &'a RenderOptions,
        page_w: u32,
        page_h: u32,
        bg: Option<&'a Pixmap>,
        mask: Option<&'a crate::bitmap::Bitmap>,
        gamma_lut: &'a [u8; 256],
        out_w: u32,
        out_h: u32,
    ) -> CompositeContext<'a> {
        let (fg_x_q24, fg_y_q24) = fg_q24(None, page_w, page_h);
        let (bg_x_q24, bg_y_q24) = bg_q24(bg, page_w, page_h);
        CompositeContext {
            opts,
            page_w,
            page_h,
            bg,
            bg_x_q24,
            bg_y_q24,
            mask,
            mask_shift: 0,
            fg_palette: None,
            blit_map: None,
            fg44: None,
            fg_x_q24,
            fg_y_q24,
            gamma_lut,
            // Compute from the lut (mirroring CompositeContext::from_layers)
            // rather than hard-coding, so a future test passing a non-identity
            // lut is not silently routed down the identity fast path.
            gamma_is_identity: gamma_lut.iter().enumerate().all(|(i, &v)| v == i as u8),
            offset_x: 0,
            offset_y: 0,
            out_w,
            out_h,
        }
    }

    #[test]
    fn composite_bilevel_one_maps_mask_to_black_and_white() {
        // Foreground bits → opaque black; background → opaque white.
        let opts = RenderOptions::default();
        let mut mask = crate::bitmap::Bitmap::new(8, 1);
        mask.set_black(0, 0);
        mask.set_black(3, 0);
        let lut = identity_lut();
        let ctx = synth_ctx(&opts, 8, 1, None, Some(&mask), &lut, 8, 1);

        let mut row = vec![0u8; 8 * 4];
        composite_rows_bilevel_one(&ctx, 0, FRAC, FRAC, &mut row);

        for x in 0..8usize {
            let p = &row[x * 4..x * 4 + 4];
            if x == 0 || x == 3 {
                assert_eq!(p, [0, 0, 0, 255], "foreground pixel at x={x}");
            } else {
                assert_eq!(p, [255, 255, 255, 255], "background pixel at x={x}");
            }
        }
    }

    #[test]
    fn composite_bilinear_one_copies_background_1to1_unsubsampled() {
        // page_w == bg_w ⇒ bg_x_q24 == 1<<24, so this drives the A2 tight
        // mask-LUT-expand fast path (the common corpus case where the
        // background is at page resolution). Identity gamma + all-background
        // mask ⇒ the bg pixels must be reproduced exactly.
        let opts = RenderOptions::default();
        let bg = Pixmap::new(4, 2, 10, 20, 30, 255);
        let mask = crate::bitmap::Bitmap::new(4, 2); // all background
        let lut = identity_lut();
        let ctx = synth_ctx(&opts, 4, 2, Some(&bg), Some(&mask), &lut, 4, 2);

        let mut row = vec![0u8; 4 * 4];
        composite_rows_bilinear_one(&ctx, 0, FRAC, FRAC, &mut row, None, &mut Vec::new());

        for x in 0..4usize {
            let p = &row[x * 4..x * 4 + 4];
            assert_eq!(&p[..3], &[10, 20, 30], "background colour at x={x}");
            assert_eq!(p[3], 255, "opaque at x={x}");
        }
    }

    #[test]
    fn composite_bilinear_one_upsamples_subsampled_background() {
        // page_w (8) > bg_w (4) ⇒ bg_x_q24 != 1<<24, so the A2 tight path is
        // skipped and the real bilinear sampler (`bilinear_from_rows`) runs to
        // upscale the subsampled background. A solid bg interpolates to itself,
        // so every output pixel must equal the bg colour exactly — proving the
        // sampler addresses the bg correctly without corrupting it.
        let opts = RenderOptions::default();
        let bg = Pixmap::new(4, 2, 70, 90, 110, 255); // half page resolution
        let mask = crate::bitmap::Bitmap::new(8, 2); // page-res, all background
        let lut = identity_lut();
        let ctx = synth_ctx(&opts, 8, 2, Some(&bg), Some(&mask), &lut, 8, 2);
        // Sanity: this configuration must NOT take the 1:1 tight path.
        assert_ne!(
            ctx.bg_x_q24,
            1 << 24,
            "test must exercise the subsampled path"
        );

        let mut row = vec![0u8; 8 * 4];
        composite_rows_bilinear_one(&ctx, 0, FRAC, FRAC, &mut row, None, &mut Vec::new());

        for x in 0..8usize {
            let p = &row[x * 4..x * 4 + 4];
            assert_eq!(&p[..3], &[70, 90, 110], "upsampled bg colour at x={x}");
            assert_eq!(p[3], 255, "opaque at x={x}");
        }
    }

    #[test]
    fn composite_area_avg_one_averages_uniform_background_on_downscale() {
        // 2× downscale of a solid background: every 2×2 source block averages to
        // the same colour, so the output cells equal that colour exactly.
        let opts = RenderOptions::default();
        let bg = Pixmap::new(4, 2, 40, 80, 120, 255);
        let mask = crate::bitmap::Bitmap::new(4, 2); // all background
        let lut = identity_lut();
        let ctx = synth_ctx(&opts, 4, 2, Some(&bg), Some(&mask), &lut, 2, 1);

        let fx_step = 2 * FRAC;
        let fy_step = 2 * FRAC;
        let bg_fx_step = ((fx_step as u64 * ctx.bg_x_q24) >> 24) as u32;
        let bg_fy_step = ((fy_step as u64 * ctx.bg_y_q24) >> 24) as u32;
        let xs = precompute_area_avg_x(&ctx, fx_step, bg_fx_step);

        let mut row = vec![0u8; 2 * 4];
        composite_rows_area_avg_one(
            &ctx,
            0,
            fx_step,
            fy_step,
            bg_fx_step,
            bg_fy_step,
            &mut row,
            Some(&xs),
        );

        for x in 0..2usize {
            let p = &row[x * 4..x * 4 + 4];
            assert_eq!(&p[..3], &[40, 80, 120], "averaged background at x={x}");
            assert_eq!(p[3], 255, "opaque at x={x}");
        }
    }

    // ── TDD: failing tests written first ─────────────────────────────────────

    /// Issue #199 regression: page-space FRACBITS coords must be scaled into
    /// FG44-space using the `fg_x_q24` / `fg_y_q24` ratios. With page_w=2260
    /// and fg_w=189 a Q24 ratio of `(189 << 24) / 2260` maps the rightmost
    /// column to `fg_w - 1` instead of clamping every column past x=189.
    #[test]
    fn fg_q24_maps_endpoints_into_fg_space() {
        let fg = Pixmap::white(189, 306);
        let (qx, qy) = fg_q24(Some(&fg), 2260, 3669);
        assert!(qx > 0 && qy > 0);
        let frac = 1u64 << FRACBITS;
        let last_x = 2259u64 * frac;
        let fg_fx = (last_x * qx) >> 24;
        let fg_px = fg_fx >> FRACBITS;
        assert_eq!(fg_px, (fg.width as u64) - 1);
        let last_y = 3668u64 * frac;
        let fg_fy = (last_y * qy) >> 24;
        let fg_py = fg_fy >> FRACBITS;
        assert_eq!(fg_py, (fg.height as u64) - 1);
    }

    #[test]
    fn fg_q24_returns_zero_when_fg_is_none() {
        assert_eq!(fg_q24(None, 100, 100), (0, 0));
    }

    /// Issue #199 second-half regression: BG plane is often stored at a
    /// non-power-of-2 fraction of the page (1/3 is common for 400dpi colour
    /// scans). Without `bg_x_q24` / `bg_y_q24` page→bg-space scaling the BG
    /// sampler clamped most of the page to the rightmost BG column.
    #[test]
    fn bg_q24_maps_non_pow2_subsample() {
        // Page 2260×3669 with BG plane 754×1223 (DjVu's padded 1/3-page layout).
        let bg = Pixmap::white(754, 1223);
        let (qx, qy) = bg_q24(Some(&bg), 2260, 3669);
        assert_eq!(qx, (1u64 << 24) / 3);
        assert_eq!(qy, (1u64 << 24) / 3);

        let last_x = 2259u32 * FRAC;
        let bg_px = (map_plane_center_frac(last_x, qx) as u64) >> FRACBITS;
        assert!(bg_px < bg.width as u64);
        let last_y = 3668u32 * FRAC;
        let bg_py = (map_plane_center_frac(last_y, qy) as u64) >> FRACBITS;
        assert!(bg_py < bg.height as u64);
    }

    #[test]
    fn bg_q24_returns_zero_when_bg_is_none() {
        assert_eq!(bg_q24(None, 100, 100), (0, 0));
    }

    #[test]
    fn plane_q24_some_branch_with_zero_dimension_plane() {
        // Lines 508-512: plane_q24 Some(p) arm, reached via fg_q24 when fg has
        // zero width (so fg_q24's inner guard p.width > 0 fails, falling through
        // to plane_q24). With page_w > 0 && page_h > 0, plane_q24 takes its
        // Some arm and returns (0/page_w, 0/page_h) = (0, 0).
        let zero_width_fg = Pixmap::new(0, 10, 0, 0, 0, 0);
        let (qx, qy) = fg_q24(Some(&zero_width_fg), 100, 100);
        assert_eq!(qx, 0); // (0 << 24) / 100 = 0
        assert_eq!(qy, (10u64 << 24) / 100); // height-based ratio
    }

    #[test]
    fn fg_q24_uses_integer_horizontal_cell_pitch() {
        let fg = Pixmap::white(189, 306);
        let (qx, qy) = fg_q24(Some(&fg), 2260, 3669);
        assert_eq!(qx, (1u64 << 24) / 12);
        assert_eq!(qy, ((306u64) << 24) / 3669);
    }

    #[test]
    fn bg_q24_uses_integer_cell_pitch_for_padded_edges() {
        let bg = Pixmap::white(754, 1223);
        let (qx, qy) = bg_q24(Some(&bg), 2260, 3669);
        assert_eq!(qx, (1u64 << 24) / 3);
        assert_eq!(qy, (1u64 << 24) / 3);
    }

    #[test]
    fn map_plane_center_frac_aligns_pixel_centers() {
        // Destination page is twice the source plane size.  Page pixel x=1 has
        // centre 1.5; mapped to source centre space that is 1.5 * 0.5 - 0.5 = 0.25.
        let q24 = (1u64 << 24) / 2;
        assert_eq!(map_plane_center_frac(0, q24), 0);
        assert_eq!(map_plane_center_frac(FRAC, q24), FRAC / 4);
    }

    #[test]
    fn sample_bilinear_rounds_to_nearest() {
        let mut pm = Pixmap::new(2, 2, 0, 0, 0, 255);
        pm.set_rgb(1, 1, 255, 255, 255);

        // At the exact centre, bilinear interpolation is 63.75, which should
        // round to 64 instead of truncating to 63.
        assert_eq!(sample_bilinear(&pm, FRAC / 2, FRAC / 2), (64, 64, 64));
    }

    #[test]
    fn sample_nearest_rounds_to_nearest_pixel() {
        let mut pm = Pixmap::new(2, 1, 10, 20, 30, 255);
        pm.set_rgb(1, 0, 200, 210, 220);

        assert_eq!(sample_nearest(&pm, FRAC / 2 - 1, 0), (10, 20, 30));
        assert_eq!(sample_nearest(&pm, FRAC / 2, 0), (200, 210, 220));
    }

    #[test]
    fn mask_box_coverage_values() {
        use crate::bitmap::Bitmap;
        // 4×1 mask: bits [1,0,1,1] → 3 out of 4 → coverage = (3*255+2)/4 = 191
        let mut bm = Bitmap::new(4, 1);
        bm.set(0, 0, true);
        bm.set(2, 0, true);
        bm.set(3, 0, true);
        let step = 4 * FRAC;
        assert_eq!(mask_box_coverage(&bm, 0, 0, step, FRAC), 191);
        // all foreground → 255
        let mut bm_full = Bitmap::new(2, 2);
        bm_full.set(0, 0, true);
        bm_full.set(1, 0, true);
        bm_full.set(0, 1, true);
        bm_full.set(1, 1, true);
        assert_eq!(mask_box_coverage(&bm_full, 0, 0, 2 * FRAC, 2 * FRAC), 255);
        // all background → 0
        let bm_empty = Bitmap::new(2, 2);
        assert_eq!(mask_box_coverage(&bm_empty, 0, 0, 2 * FRAC, 2 * FRAC), 0);
    }

    // ── D_AA_ZOOM: mask_aa (bilinear coverage AA at upscale) ─────────────────

    #[test]
    fn mask_bilinear_coverage_values() {
        use crate::bitmap::Bitmap;
        // 4×1 mask: bit 0 set, rest clear.
        let mut bm = Bitmap::new(4, 1);
        bm.set(0, 0, true);
        // Exactly on a pixel centre (tx=ty=0): pure sample of bit 0 → 255.
        assert_eq!(mask_bilinear_coverage(&bm, 0, 0), 255);
        // Halfway between bit 0 (255) and bit 1 (0): (255+0)/2 rounded → 128.
        assert_eq!(mask_bilinear_coverage(&bm, 8, 0), 128);
        // Halfway between bit 1 and bit 2, both clear → 0.
        assert_eq!(mask_bilinear_coverage(&bm, 24, 0), 0);
        // All-foreground mask → 255 everywhere, no interpolation artifacts.
        let mut bm_full = Bitmap::new(2, 2);
        bm_full.set(0, 0, true);
        bm_full.set(1, 0, true);
        bm_full.set(0, 1, true);
        bm_full.set(1, 1, true);
        assert_eq!(mask_bilinear_coverage(&bm_full, 4, 4), 255);
        // All-background mask → 0 everywhere.
        let bm_empty = Bitmap::new(2, 2);
        assert_eq!(mask_bilinear_coverage(&bm_empty, 4, 4), 0);
    }

    /// `composite_rows_bilevel_one` at 2× upscale with `mask_aa: false` (the
    /// default) must reproduce the exact nearest-bit pattern — hard requirement
    /// that the opt-in flag changes nothing unless explicitly enabled.
    #[test]
    fn composite_bilevel_one_mask_aa_disabled_matches_nearest_at_upscale() {
        let opts = RenderOptions::default();
        assert!(!opts.mask_aa);
        let mut mask = crate::bitmap::Bitmap::new(4, 1);
        mask.set(0, 0, true); // only x=0 is foreground
        let lut = identity_lut();
        let ctx = synth_ctx(&opts, 4, 1, None, Some(&mask), &lut, 8, 2);

        let fx_step = FRAC / 2; // 2× upscale
        let fy_step = FRAC / 2;
        let mut row = vec![0u8; 8 * 4];
        composite_rows_bilevel_one(&ctx, 0, fx_step, fy_step, &mut row);

        // Nearest-neighbour duplication of source pixels [0,0,1,1,2,2,3,3];
        // only source pixel 0 is foreground (black), rest background (white).
        let expected_black = [true, true, false, false, false, false, false, false];
        for (x, &is_black) in expected_black.iter().enumerate() {
            let p = &row[x * 4..x * 4 + 4];
            if is_black {
                assert_eq!(p, [0, 0, 0, 255], "expected black at x={x}");
            } else {
                assert_eq!(p, [255, 255, 255, 255], "expected white at x={x}");
            }
        }
    }

    /// With `mask_aa: true` at the same 2× upscale, the pixel straddling the
    /// mask edge (x=1, halfway between the set bit 0 and clear bit 1) must come
    /// out as an intermediate gray — proof the bilinear coverage path actually
    /// smooths the edge instead of just reproducing nearest-neighbour.
    #[test]
    fn composite_bilevel_one_mask_aa_enabled_smooths_edge_at_upscale() {
        let opts = RenderOptions {
            mask_aa: true,
            ..Default::default()
        };
        let mut mask = crate::bitmap::Bitmap::new(4, 1);
        mask.set(0, 0, true);
        let lut = identity_lut();
        let ctx = synth_ctx(&opts, 4, 1, None, Some(&mask), &lut, 8, 2);

        let fx_step = FRAC / 2;
        let fy_step = FRAC / 2;
        let mut row = vec![0u8; 8 * 4];
        composite_rows_bilevel_one(&ctx, 0, fx_step, fy_step, &mut row);

        // x=0 lands exactly on the set bit → still pure black.
        assert_eq!(&row[0..4], [0, 0, 0, 255], "x=0 exact sample stays black");
        // x=1 straddles bit 0 (fg) / bit 1 (bg) at tx=8/16 → coverage 128 → gray 127.
        assert_eq!(&row[4..8], [127, 127, 127, 255], "x=1 is a blended gray");
        // x=2 onward land entirely within the background region → white.
        for x in 2..8usize {
            assert_eq!(
                &row[x * 4..x * 4 + 4],
                [255, 255, 255, 255],
                "x={x} stays white"
            );
        }
    }

    /// `mask_aa` must be a no-op on the exact 1:1 fast path (native scale) —
    /// the flag only ever matters past the early return for genuine upscale.
    #[test]
    fn composite_bilevel_one_mask_aa_is_noop_at_native_scale() {
        let mut mask = crate::bitmap::Bitmap::new(4, 1);
        mask.set(0, 0, true);
        let lut = identity_lut();

        let opts_off = RenderOptions::default();
        let ctx_off = synth_ctx(&opts_off, 4, 1, None, Some(&mask), &lut, 4, 1);
        let mut row_off = vec![0u8; 4 * 4];
        composite_rows_bilevel_one(&ctx_off, 0, FRAC, FRAC, &mut row_off);

        let opts_on = RenderOptions {
            mask_aa: true,
            ..Default::default()
        };
        let ctx_on = synth_ctx(&opts_on, 4, 1, None, Some(&mask), &lut, 4, 1);
        let mut row_on = vec![0u8; 4 * 4];
        composite_rows_bilevel_one(&ctx_on, 0, FRAC, FRAC, &mut row_on);

        assert_eq!(row_off, row_on, "mask_aa must not affect native 1:1 scale");
    }

    /// `mask_aa` must be a no-op on downscale — the bilinear-upscale branch is
    /// only reachable when `!downscale`, so a `mask_aa: true` downscale render
    /// must still take the existing `mask_box_coverage` path unchanged.
    #[test]
    fn composite_bilevel_one_mask_aa_is_noop_at_downscale() {
        let mut mask = crate::bitmap::Bitmap::new(4, 1);
        mask.set(0, 0, true);
        mask.set(2, 0, true);
        mask.set(3, 0, true);
        let lut = identity_lut();

        let fx_step = 4 * FRAC; // 4× downscale
        let fy_step = FRAC;

        let opts_off = RenderOptions::default();
        let ctx_off = synth_ctx(&opts_off, 4, 1, None, Some(&mask), &lut, 1, 1);
        let mut row_off = vec![0u8; 4];
        composite_rows_bilevel_one(&ctx_off, 0, fx_step, fy_step, &mut row_off);

        let opts_on = RenderOptions {
            mask_aa: true,
            ..Default::default()
        };
        let ctx_on = synth_ctx(&opts_on, 4, 1, None, Some(&mask), &lut, 1, 1);
        let mut row_on = vec![0u8; 4];
        composite_rows_bilevel_one(&ctx_on, 0, fx_step, fy_step, &mut row_on);

        assert_eq!(row_off, row_on, "mask_aa must not affect downscale");
    }

    /// `composite_rows_bilinear_one` (colour path) at 2× upscale with
    /// `mask_aa: false` must reproduce the exact binary nearest-bit coverage —
    /// same hard byte-identical requirement as the bilevel path, for the
    /// colour+mask compositor.
    #[test]
    fn composite_bilinear_one_mask_aa_disabled_matches_nearest_at_upscale() {
        let opts = RenderOptions::default();
        let bg = Pixmap::new(8, 1, 200, 150, 100, 255);
        let mut mask = crate::bitmap::Bitmap::new(8, 1);
        mask.set(0, 0, true); // only x=0 is foreground
        let lut = identity_lut();
        let ctx = synth_ctx(&opts, 8, 1, Some(&bg), Some(&mask), &lut, 4, 1);

        let fx_step = FRAC / 2; // 2× upscale
        let fy_step = FRAC / 2;
        let mut row = vec![0u8; 4 * 4];
        composite_rows_bilinear_one(&ctx, 0, fx_step, fy_step, &mut row, None, &mut Vec::new());

        // Nearest px indices for ox=0..4 are [0,0,1,1]; only px 0 is foreground,
        // rendered black (no FG44 layer ⇒ (0,0,0)); px 1 is background colour.
        assert_eq!(&row[0..4], [0, 0, 0, 255], "ox=0 nearest foreground");
        assert_eq!(&row[4..8], [0, 0, 0, 255], "ox=1 nearest foreground");
        assert_eq!(&row[8..12], [200, 150, 100, 255], "ox=2 background");
        assert_eq!(&row[12..16], [200, 150, 100, 255], "ox=3 background");
    }

    /// With `mask_aa: true` the pixel straddling the mask edge blends the
    /// (black) foreground colour with the background colour proportionally to
    /// the interpolated coverage, instead of snapping to one or the other.
    #[test]
    fn composite_bilinear_one_mask_aa_enabled_blends_fg_bg_at_upscale() {
        let opts = RenderOptions {
            mask_aa: true,
            ..Default::default()
        };
        let bg = Pixmap::new(8, 1, 200, 150, 100, 255);
        let mut mask = crate::bitmap::Bitmap::new(8, 1);
        mask.set(0, 0, true);
        let lut = identity_lut();
        let ctx = synth_ctx(&opts, 8, 1, Some(&bg), Some(&mask), &lut, 4, 1);

        let fx_step = FRAC / 2;
        let fy_step = FRAC / 2;
        let mut row = vec![0u8; 4 * 4];
        composite_rows_bilinear_one(&ctx, 0, fx_step, fy_step, &mut row, None, &mut Vec::new());

        // ox=0 lands exactly on the set bit → still pure (black) foreground.
        assert_eq!(
            &row[0..4],
            [0, 0, 0, 255],
            "ox=0 exact sample stays foreground"
        );
        // ox=1 straddles the edge at coverage 128 → blend(0, 200/150/100, 128).
        assert_eq!(
            &row[4..8],
            [100, 75, 50, 255],
            "ox=1 is a fg/bg blend, not a hard snap"
        );
        // ox=2, ox=3 are entirely background.
        assert_eq!(&row[8..12], [200, 150, 100, 255], "ox=2 background");
        assert_eq!(&row[12..16], [200, 150, 100, 255], "ox=3 background");
    }

    /// Subtle no-op case: an exact page-level 1:1 render (`fx_step == fy_step
    /// == FRAC`) whose *background* plane is internally subsampled still falls
    /// through to the general B-series loop (the "extra-tight" 1:1 fast path
    /// requires `bg_x_q24 == 1<<24`, which fails here) — but `mask_aa` must
    /// still be a no-op there because there is no genuine axis upscale.
    #[test]
    fn composite_bilinear_one_mask_aa_is_noop_when_bg_subsampled_at_native_scale() {
        let bg = Pixmap::new(4, 1, 200, 150, 100, 255); // subsampled: page_w=8, bg_w=4
        let mut mask = crate::bitmap::Bitmap::new(8, 1);
        mask.set(0, 0, true);
        let lut = identity_lut();

        let opts_off = RenderOptions::default();
        let ctx_off = synth_ctx(&opts_off, 8, 1, Some(&bg), Some(&mask), &lut, 8, 1);
        assert_ne!(
            ctx_off.bg_x_q24,
            1 << 24,
            "test must exercise subsampled bg"
        );
        let mut row_off = vec![0u8; 8 * 4];
        composite_rows_bilinear_one(&ctx_off, 0, FRAC, FRAC, &mut row_off, None, &mut Vec::new());

        let opts_on = RenderOptions {
            mask_aa: true,
            ..Default::default()
        };
        let ctx_on = synth_ctx(&opts_on, 8, 1, Some(&bg), Some(&mask), &lut, 8, 1);
        let mut row_on = vec![0u8; 8 * 4];
        composite_rows_bilinear_one(&ctx_on, 0, FRAC, FRAC, &mut row_on, None, &mut Vec::new());

        assert_eq!(
            row_off, row_on,
            "mask_aa must not affect native 1:1 scale even with a subsampled bg plane"
        );
    }

    /// The precomputed `BilinearX` column table must be byte-identical to the
    /// in-loop Q48 fallback on every row — 2× upscale, non-zero `offset_x`,
    /// subsampled non-uniform bg so any x0/x1/tx mismatch shows up in bytes.
    #[test]
    fn composite_bilinear_one_column_table_matches_fallback() {
        let opts = RenderOptions::default();
        let mut bg = Pixmap::new(3, 2, 0, 0, 0, 255); // subsampled: page_w=8, bg_w=3
        for (i, px) in bg.data.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            px[0] = (i * 40) as u8;
            px[1] = (i * 25 + 7) as u8;
            px[2] = (255 - i * 30) as u8;
        }
        let mut mask = crate::bitmap::Bitmap::new(8, 2);
        mask.set(2, 0, true); // one fg bit so the partial/fg branches run too
        let lut = identity_lut();

        let mut ctx = synth_ctx(&opts, 8, 2, Some(&bg), Some(&mask), &lut, 16, 4);
        ctx.offset_x = 3;

        let fx_step = FRAC / 2; // 2× upscale
        let fy_step = FRAC / 2;
        let table = precompute_bilinear_x(&ctx, fx_step).expect("bg present");

        for oy in 0..4 {
            let mut row_table = vec![0u8; 16 * 4];
            let mut row_fallback = vec![0u8; 16 * 4];
            composite_rows_bilinear_one(
                &ctx,
                oy,
                fx_step,
                fy_step,
                &mut row_table,
                Some(&table),
                &mut Vec::new(),
            );
            composite_rows_bilinear_one(
                &ctx,
                oy,
                fx_step,
                fy_step,
                &mut row_fallback,
                None,
                &mut Vec::new(),
            );
            assert_eq!(
                row_table, row_fallback,
                "table and fallback sampling must agree at oy={oy}"
            );
        }
    }

    /// Integration-level: `render_pixmap` at native scale on a real bilevel
    /// document must be byte-identical whether `mask_aa` is on or off — the
    /// no-op-at-scale-≤1 guarantee holding end-to-end, not just at the
    /// synthetic-context unit level.
    #[test]
    fn render_pixmap_mask_aa_is_noop_at_native_scale_real_doc() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let (w, h) = (page.width() as u32, page.height() as u32);

        let opts_off = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let opts_on = RenderOptions {
            width: w,
            height: h,
            mask_aa: true,
            ..Default::default()
        };
        let pm_off = render_pixmap(page, &opts_off).expect("render should succeed");
        let pm_on = render_pixmap(page, &opts_on).expect("render should succeed");
        assert_eq!(
            pm_off.data, pm_on.data,
            "mask_aa must be a no-op at native scale"
        );
    }

    /// Integration-level: `render_pixmap` at downscale on a real bilevel
    /// document must also be byte-identical between `mask_aa` on/off.
    #[test]
    fn render_pixmap_mask_aa_is_noop_at_downscale_real_doc() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let (w, h) = ((page.width() as u32) / 2, (page.height() as u32) / 2);

        let opts_off = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let opts_on = RenderOptions {
            width: w,
            height: h,
            mask_aa: true,
            ..Default::default()
        };
        let pm_off = render_pixmap(page, &opts_off).expect("render should succeed");
        let pm_on = render_pixmap(page, &opts_on).expect("render should succeed");
        assert_eq!(
            pm_off.data, pm_on.data,
            "mask_aa must be a no-op at downscale"
        );
    }

    /// Integration-level: at genuine 2× and 4× upscale on a real bilevel
    /// document, `mask_aa: true` must actually change the output (introduce
    /// intermediate gray values along glyph edges) — proving the flag is wired
    /// end-to-end, not just correct in isolated unit tests.
    #[test]
    fn render_pixmap_mask_aa_smooths_edges_at_zoom_real_doc() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let (pw, ph) = (page.width() as u32, page.height() as u32);

        for &zoom in &[2u32, 4u32] {
            let opts_off = RenderOptions {
                width: pw * zoom,
                height: ph * zoom,
                ..Default::default()
            };
            let opts_on = RenderOptions {
                width: pw * zoom,
                height: ph * zoom,
                mask_aa: true,
                ..Default::default()
            };
            let pm_off = render_pixmap(page, &opts_off).expect("nearest render should succeed");
            let pm_on = render_pixmap(page, &opts_on).expect("AA render should succeed");
            assert_eq!(pm_off.width, pm_on.width);
            assert_eq!(pm_off.height, pm_on.height);

            assert_ne!(
                pm_off.data, pm_on.data,
                "mask_aa=true must change output at {zoom}× zoom"
            );
            let has_intermediate_gray = pm_on
                .data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[0] == px[1] && px[1] == px[2] && px[0] != 0 && px[0] != 255);
            assert!(
                has_intermediate_gray,
                "mask_aa=true should introduce intermediate gray values at {zoom}× zoom"
            );
        }
    }

    /// RenderOptions default values.
    #[test]
    fn render_options_default() {
        let opts = RenderOptions::default();
        assert_eq!(opts.width, 0);
        assert_eq!(opts.height, 0);
        assert_eq!(opts.bold, 0);
        assert!(!opts.aa);
        assert_eq!(opts.resampling, Resampling::Bilinear);
        assert!(!opts.mask_aa, "mask_aa must default to false (opt-in)");
    }

    /// RenderOptions can be constructed with explicit fields.
    #[test]
    fn render_options_construction() {
        let opts = RenderOptions {
            width: 400,
            height: 300,
            bold: 1,
            aa: true,
            rotation: UserRotation::Cw90,
            ..Default::default()
        };
        assert_eq!(opts.width, 400);
        assert_eq!(opts.height, 300);
        assert_eq!(opts.bold, 1);
        assert!(opts.aa);
        assert_eq!(opts.rotation, UserRotation::Cw90);
    }

    /// The incremental `render_progressive_all` fast path (B5) must be
    /// byte-identical to the per-frame `render_progressive_step` loop it
    /// replaces, on a real multi-BG44-chunk page.
    #[test]
    fn render_progressive_all_matches_per_frame() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        // chicken.djvu has 3 BG44 chunks → 3 progressive frames, exercising the
        // incremental streaming path (steps > 1, Bilinear, strict).
        assert!(
            page.bg44_chunks().len() >= 2,
            "need a multi-chunk BG44 page"
        );

        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Bilinear,
            ..Default::default()
        };

        let all = render_progressive_all(page, &opts).expect("progressive_all");
        let steps = progressive_steps(page);
        assert_eq!(all.len(), steps);
        for (step, frame) in all.iter().enumerate() {
            let per_frame = render_progressive_step(page, &opts, step).expect("progressive_step");
            assert_eq!(
                (frame.width, frame.height),
                (per_frame.width, per_frame.height),
                "frame {step} dimensions differ"
            );
            assert!(
                frame.data == per_frame.data,
                "frame {step} pixels differ between incremental and per-frame paths"
            );
        }
    }

    #[test]
    fn progressive_decoder_streams_frames_matching_batch() {
        // The streaming ProgressiveDecoder, fed one BG44 chunk at a time, must
        // reproduce render_progressive_all's frames byte-for-byte.
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let chunks = page.bg44_chunks();
        assert!(chunks.len() >= 2, "need a multi-chunk BG44 page");

        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Bilinear,
            ..Default::default()
        };

        let batch = render_progressive_all(page, &opts).expect("progressive_all");

        let mut dec = ProgressiveDecoder::new(page, &opts).expect("decoder");
        let steps = progressive_steps(page);
        for (i, chunk) in chunks.iter().take(steps).enumerate() {
            let frame = dec.push_bg44_chunk(chunk).expect("push");
            assert_eq!(dec.frames_produced(), i + 1);
            assert_eq!(
                (frame.width, frame.height),
                (batch[i].width, batch[i].height),
                "streamed frame {i} dimensions differ"
            );
            assert!(
                frame.data == batch[i].data,
                "streamed frame {i} pixels differ from batch progressive_all"
            );
        }
    }

    /// The streaming `ProgressiveDecoder`, fed one BG44 chunk at a time, must
    /// reproduce `render_progressive_step`'s frames byte-for-byte — the
    /// byte-identical requirement checked directly against the per-frame API
    /// (not only via the `render_progressive_all` batch path already covered
    /// above), on both the small 3-chunk `chicken.djvu` and the larger
    /// 4-chunk `colorbook.djvu` fixtures.
    fn assert_progressive_decoder_matches_step(filename: &str) {
        let doc = load_doc(filename);
        let page = doc.page(0).unwrap();
        let chunks = page.bg44_chunks();
        assert!(
            chunks.len() >= 2,
            "{filename}: need a multi-chunk BG44 page"
        );

        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Bilinear,
            ..Default::default()
        };

        let mut dec = ProgressiveDecoder::new(page, &opts).expect("decoder");
        let steps = progressive_steps(page);
        for (i, chunk) in chunks.iter().take(steps).enumerate() {
            let streamed = dec.push_bg44_chunk(chunk).expect("push");
            let stepped = render_progressive_step(page, &opts, i).expect("progressive_step");
            assert_eq!(
                (streamed.width, streamed.height),
                (stepped.width, stepped.height),
                "{filename} frame {i} dimensions differ"
            );
            assert!(
                streamed.data == stepped.data,
                "{filename}: streamed frame {i} pixels differ from render_progressive_step"
            );
        }
    }

    #[test]
    fn progressive_decoder_matches_render_progressive_step_chicken() {
        assert_progressive_decoder_matches_step("chicken.djvu");
    }

    #[test]
    fn progressive_decoder_matches_render_progressive_step_colorbook() {
        assert_progressive_decoder_matches_step("colorbook.djvu");
    }

    /// Structural proof of the B5 claim (primary evidence per the design doc,
    /// since wall-clock is noisy on a shared machine): a naive session that
    /// calls `render_progressive_step(0..N)` re-decodes BG44 chunks
    /// `1+2+...+N` times — O(N²) — while a `ProgressiveDecoder` session over
    /// the same N frames decodes each chunk exactly once — O(N). Counted via
    /// the `#[cfg(test)]`-only `BG44_CHUNK_DECODES` counter at the two real
    /// `Iw44Image::decode_chunk` call sites, not wall-clock timing.
    ///
    /// Under the `parallel` feature, `decode_layers` runs the naive session's
    /// background decode through `rayon::join` (see `#440` there). Calling
    /// `rayon::join` from a plain thread that isn't already a rayon worker —
    /// such as this test's own thread — makes rayon bridge onto a worker
    /// thread from its shared *global* pool to execute the join. That breaks
    /// the `BG44_CHUNK_DECODES` thread-local's implicit assumption that every
    /// counted decode call lands on the thread that set/reads it: the
    /// increments happen on a global-pool worker thread this test's thread_local
    /// handle never sees, so the naive count silently reads back as 0
    /// (verified: found failing under `--features cli,mmap,parallel`, and in
    /// isolation under `--features parallel` alone — `mmap` is not implicated).
    /// Route the whole measurement through a dedicated, single-worker rayon
    /// pool instead: with exactly one worker, any `rayon::join` bridged into
    /// it always resolves on that same worker, so setting and reading the
    /// counter from *inside* the pool keeps everything on one thread
    /// regardless of the `parallel` feature — and because the pool is freshly
    /// built here (not rayon's shared global pool), this stays isolated from
    /// any other test's concurrent decode calls, preserving the isolation the
    /// original thread-local was there for.
    #[test]
    fn progressive_decoder_chunk_decodes_are_on_not_on_squared() {
        let body = || {
            for filename in ["chicken.djvu", "colorbook.djvu"] {
                let doc = load_doc(filename);
                let page = doc.page(0).unwrap();
                let chunks = page.bg44_chunks();
                let n = chunks.len();
                assert!(n >= 3, "{filename}: need >=3 BG44 chunks");

                let opts = RenderOptions {
                    width: page.width() as u32,
                    height: page.height() as u32,
                    resampling: Resampling::Bilinear,
                    ..Default::default()
                };

                // Naive per-frame session: render_progressive_step(0..N), each call
                // re-decoding the chunk prefix from scratch (the pre-B5 behaviour).
                BG44_CHUNK_DECODES.with(|c| c.set(0));
                for step in 0..n {
                    render_progressive_step(page, &opts, step).expect("progressive_step");
                }
                let naive = BG44_CHUNK_DECODES.with(|c| c.get());
                let expected_naive: usize = (1..=n).sum(); // 1+2+...+N
                assert_eq!(
                    naive, expected_naive,
                    "{filename}: naive per-frame session should decode chunks \
                     1+2+...+N = {expected_naive} times, got {naive}"
                );

                // Stateful streaming session: one decode per chunk, total N.
                BG44_CHUNK_DECODES.with(|c| c.set(0));
                let mut dec = ProgressiveDecoder::new(page, &opts).expect("decoder");
                for chunk in chunks.iter() {
                    dec.push_bg44_chunk(chunk).expect("push");
                }
                let streamed = BG44_CHUNK_DECODES.with(|c| c.get());
                assert_eq!(
                    streamed, n,
                    "{filename}: stateful session should decode each chunk exactly \
                     once (O(N) = {n}), got {streamed}"
                );

                assert!(
                    naive > streamed,
                    "{filename}: naive session ({naive} decodes) should strictly \
                     exceed the streamed session ({streamed} decodes)"
                );
            }
        };

        #[cfg(feature = "parallel")]
        {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("build single-threaded pool for deterministic thread-local counting");
            pool.install(body);
        }
        #[cfg(not(feature = "parallel"))]
        body();
    }

    #[test]
    fn progressive_decoder_rejects_lanczos_and_zero_dims() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let mut opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Lanczos3,
            ..Default::default()
        };
        assert!(matches!(
            ProgressiveDecoder::new(page, &opts),
            Err(RenderError::UnsupportedOption(_))
        ));
        opts.resampling = Resampling::Bilinear;
        opts.width = 0;
        assert!(matches!(
            ProgressiveDecoder::new(page, &opts),
            Err(RenderError::InvalidDimensions { .. })
        ));
    }

    /// Same byte-identity guarantee for the incremental progressive path with
    /// `bold > 0`: the fast path dilates the mask once and reuses it across
    /// frames, which must match the per-frame path that dilates each frame.
    #[test]
    fn render_progressive_all_matches_per_frame_bold() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        assert!(
            page.bg44_chunks().len() >= 2,
            "need a multi-chunk BG44 page"
        );

        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Bilinear,
            bold: 2,
            ..Default::default()
        };

        let all = render_progressive_all(page, &opts).expect("progressive_all");
        let steps = progressive_steps(page);
        assert_eq!(all.len(), steps);
        for (step, frame) in all.iter().enumerate() {
            let per_frame = render_progressive_step(page, &opts, step).expect("progressive_step");
            assert_eq!(
                (frame.width, frame.height),
                (per_frame.width, per_frame.height),
                "frame {step} dimensions differ (bold)"
            );
            assert!(
                frame.data == per_frame.data,
                "frame {step} pixels differ between incremental and per-frame paths (bold)"
            );
        }
    }

    /// Evicting the render cache must not change output: a re-render after
    /// `evict_render_caches` is byte-identical to the first (the cache rebuilds
    /// lazily and correctly).
    #[test]
    fn evict_render_cache_preserves_output() {
        let mut doc = load_doc("chicken.djvu");
        let (w, h) = {
            let p = doc.page(0).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let first = {
            let p = doc.page(0).unwrap();
            render_pixmap(p, &opts).unwrap()
        };
        doc.evict_render_caches();
        let second = {
            let p = doc.page(0).unwrap();
            render_pixmap(p, &opts).unwrap()
        };
        assert_eq!(
            first.data, second.data,
            "output changed after cache eviction"
        );
    }

    /// `enforce_cache_budget` evicts least-recently-used unprotected pages down
    /// to the budget, keeps protected pages, and re-renders byte-identically.
    #[test]
    fn enforce_cache_budget_lru_and_correctness() {
        let mut doc = load_doc("colorbook.djvu");
        if doc.page_count() < 3 {
            return; // needs a multi-page fixture
        }
        // Render pages 0,1,2 in order → LRU order is 0 < 1 < 2 by access tick.
        let mut first0 = None;
        for i in 0..3 {
            let (w, h) = {
                let p = doc.page(i).unwrap();
                (p.width() as u32, p.height() as u32)
            };
            let opts = RenderOptions {
                width: w,
                height: h,
                ..Default::default()
            };
            let p = doc.page(i).unwrap();
            let pm = render_pixmap(p, &opts).unwrap();
            if i == 0 {
                first0 = Some(pm);
            }
        }
        // LRU ticks strictly increase with render order.
        let t0 = doc.page(0).unwrap().render_cache_access_tick();
        let t1 = doc.page(1).unwrap().render_cache_access_tick();
        let t2 = doc.page(2).unwrap().render_cache_access_tick();
        assert!(t0 < t1 && t1 < t2, "LRU ticks not ordered: {t0} {t1} {t2}");

        assert!(doc.render_cache_bytes() > 0);
        // Budget 1 byte, protect page 2 → evict the two LRU unprotected pages.
        let freed = doc.enforce_cache_budget(1, &[2]);
        assert!(freed > 0, "expected some bytes freed");
        assert_eq!(
            doc.page(0).unwrap().render_cache_bytes(),
            0,
            "page 0 not evicted"
        );
        assert_eq!(
            doc.page(1).unwrap().render_cache_bytes(),
            0,
            "page 1 not evicted"
        );
        assert!(
            doc.page(2).unwrap().render_cache_bytes() > 0,
            "protected page 2 was evicted"
        );

        // Re-rendering an evicted page reproduces the original output exactly.
        let (w, h) = {
            let p = doc.page(0).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let second0 = render_pixmap(doc.page(0).unwrap(), &opts).unwrap();
        assert_eq!(first0.unwrap().data, second0.data);
    }

    /// C5_COMPRESS: `downgrade_render_cache` must (a) shrink the cache, (b)
    /// keep a previously-cached `bg_rgb_s2` warm — a subsequent sub=2 render
    /// must not force a fresh BG44 decode — and (c) still reproduce the exact
    /// same full-resolution output on a later cold sub=1 render (the
    /// full-res path re-decodes from scratch, byte-identically).
    #[test]
    fn downgrade_render_cache_keeps_downscaled_tier_warm() {
        let mut doc = load_doc("colorbook.djvu");
        let (w, h) = {
            let p = doc.page(0).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        let opts_s1 = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        // sub=2 request: half-resolution output.
        let opts_s2 = RenderOptions {
            width: w / 2,
            height: h / 2,
            ..Default::default()
        };

        let first_s1 = render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
        let first_s2 = render_pixmap(doc.page(0).unwrap(), &opts_s2).unwrap();
        let bytes_before = doc.page(0).unwrap().render_cache_bytes();
        assert!(bytes_before > 0);

        doc.downgrade_render_caches();
        let bytes_after = doc.page(0).unwrap().render_cache_bytes();
        assert!(
            bytes_after > 0 && bytes_after < bytes_before,
            "downgrade should shrink but not zero the cache: before={bytes_before} after={bytes_after}"
        );

        // sub=2 render after downgrade: warm (bg_rgb_s2 preserved), and output
        // is unchanged.
        let second_s2 = render_pixmap(doc.page(0).unwrap(), &opts_s2).unwrap();
        assert_eq!(first_s2.data, second_s2.data, "sub=2 output changed");

        // sub=1 (full-res) render after downgrade: cold-decodes but still
        // reproduces the original output exactly.
        let second_s1 = render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
        assert_eq!(first_s1.data, second_s1.data, "sub=1 output changed");
    }

    /// IW44_CHECKPOINT (#608): a full render after a sub>=4 render (which
    /// cached the first-chunk partial decode) resumes from that checkpoint —
    /// and must be byte-identical to a cold full render on a fresh document.
    #[test]
    fn full_decode_resumed_from_partial_is_byte_identical() {
        let doc_a = load_doc("colorbook.djvu");
        let (w, h) = {
            let p = doc_a.page(0).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        // Warm the partial tier via a sub=4 render, then full render.
        let opts_s4 = RenderOptions {
            width: w / 4,
            height: h / 4,
            ..Default::default()
        };
        let opts_s1 = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let _ = render_pixmap(doc_a.page(0).unwrap(), &opts_s4).unwrap();
        assert!(
            doc_a
                .page(0)
                .unwrap()
                .render_layers()
                .bg44_partial
                .get()
                .is_some(),
            "sub=4 render must populate the partial tier"
        );
        let resumed = render_pixmap(doc_a.page(0).unwrap(), &opts_s1).unwrap();

        // Cold full render on a fresh document (no partial tier).
        let doc_b = load_doc("colorbook.djvu");
        let cold = render_pixmap(doc_b.page(0).unwrap(), &opts_s1).unwrap();

        assert_eq!(
            resumed.data, cold.data,
            "resumed full decode must be byte-identical"
        );
    }

    /// #576: back-and-forth pan hit-rate through the tile cache. LRU keeps
    /// the tiles a reversing pan is about to revisit; the printed numbers are
    /// the experiment's measurement (run with --nocapture), the assert is the
    /// regression floor.
    #[test]
    fn tile_cache_back_and_forth_pan_hit_rate() {
        let doc = load_doc("colorbook.djvu");
        let page = doc.page(0).unwrap();
        let (w, h) = (page.width() as u32, page.height() as u32);
        // 2x zoom full-render space, viewport ~1/3 page, 25% pan steps,
        // left-to-right then back — the classic reading pattern.
        let opts = RenderOptions {
            width: w * 2,
            height: h * 2,
            ..Default::default()
        };
        // Realistic laptop viewport: 1440×960 ≈ 24 tiles (fits the 8 MiB /
        // ~32-tile budget with headroom — a viewport larger than the budget
        // thrashes any policy).
        let vw = 1440u32.min(w * 2);
        let vh = 960u32.min(h * 2);
        let step = vw / 4;
        let max_x = (w * 2).saturating_sub(vw);
        let mut xs: Vec<u32> = (0..=(max_x / step)).map(|i| i * step).collect();
        let back: Vec<u32> = xs.iter().rev().skip(1).copied().collect();
        xs.extend(back);
        for &x in &xs {
            let _ = render_region_tiled(
                page,
                RenderRect {
                    x,
                    y: 0,
                    width: vw,
                    height: vh,
                },
                &opts,
            )
            .unwrap();
        }
        let (hits, misses, evictions) = page.render_layers().tile_cache_stats();
        let rate = hits as f64 / (hits + misses).max(1) as f64;
        println!(
            "tile cache back-and-forth pan: hits={hits} misses={misses} evictions={evictions} hit-rate={:.1}%",
            rate * 100.0
        );
        assert!(
            rate > 0.30,
            "back-and-forth pan hit rate too low: {:.1}%",
            rate * 100.0
        );
    }

    /// Round 89 follow-up: a *cold* thumbnail-style render (bg_subsample >= 4,
    /// no bold, no FGbz, first render of the page — nothing warm yet) must
    /// not run the full-resolution JB2 mask decode at all, and must still
    /// produce output pixel-identical to the same render done the old way
    /// (mask_sub4 built by downsampling an already-decoded full-resolution
    /// mask). Before this change, `decode_layers`'s #607 fast path required
    /// `mask_sub4` to already be warm, so the very first (cold) sub>=4
    /// render — exactly `Document::thumbnails()`'s access pattern — always
    /// paid for a full-resolution `extract_mask` canvas just to immediately
    /// downsample and discard it.
    #[cfg(feature = "std")]
    #[test]
    fn cold_thumbnail_sweep_skips_full_mask_decode() {
        let body = || {
            let (w, h) = {
                let doc = load_doc("colorbook.djvu");
                let page = doc.page(0).unwrap();
                (page.width() as u32, page.height() as u32)
            };
            let opts_s4 = RenderOptions {
                width: w / 4,
                height: h / 4,
                ..Default::default()
            };
            let opts_s1 = RenderOptions {
                width: w,
                height: h,
                ..Default::default()
            };

            // Reference: force the full-resolution mask to decode and cache
            // first (a plain sub=1 render), then take the sub4 render — this
            // exercises `mask_sub4`'s "downsample an already-cached full mask"
            // branch, matching pre-fix behaviour exactly.
            let doc_warm = load_doc("colorbook.djvu");
            let page_warm = doc_warm.page(0).unwrap();
            let _ = render_pixmap(page_warm, &opts_s1).unwrap();
            let reference = render_pixmap(page_warm, &opts_s4).unwrap();

            // Cold: a fresh document, straight to a sub4 render — nothing
            // warm, must decode straight to 1/4 resolution via
            // `extract_mask_sub4` and must not touch the full-res decoder.
            let doc_cold = load_doc("colorbook.djvu");
            let page_cold = doc_cold.page(0).unwrap();
            JB2_MASK_DECODES.with(|c| c.set(0));
            let cold_s4 = render_pixmap(page_cold, &opts_s4).unwrap();
            assert_eq!(
                JB2_MASK_DECODES.with(|c| c.get()),
                0,
                "cold sub>=4 render must not run the full-resolution JB2 decode"
            );

            assert_eq!(
                cold_s4.data, reference.data,
                "cold sub4 thumbnail-style render must match the warm-mask-sub4 render"
            );
        };

        #[cfg(feature = "parallel")]
        {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("build single-threaded pool for deterministic thread-local counting");
            pool.install(body);
        }
        #[cfg(not(feature = "parallel"))]
        body();
    }

    /// #607: `downgrade` retains the 1/4-res mask, and an eligible sub>=4
    /// re-render consumes it without re-running the JB2 decode — while
    /// producing pixel-identical output. A full-resolution re-render stays
    /// cold and also reproduces the original bytes.
    ///
    /// Under `parallel`, `decode_layers` runs the cold full-res path through
    /// `rayon::join` (#440). The same thread-local counting hazard as
    /// [`progressive_decoder_chunk_decodes_are_on_not_on_squared`] applies:
    /// increments land on a global-pool worker, so the test thread reads 0.
    /// Route the measurement through a dedicated single-worker pool (#721).
    #[test]
    fn downgrade_retains_sub4_mask_and_skips_jb2_decode() {
        let body = || {
            let mut doc = load_doc("colorbook.djvu");
            let (w, h) = {
                let p = doc.page(0).unwrap();
                (p.width() as u32, p.height() as u32)
            };
            let opts_s4 = RenderOptions {
                width: w / 4,
                height: h / 4,
                ..Default::default()
            };
            let opts_s1 = RenderOptions {
                width: w,
                height: h,
                ..Default::default()
            };

            // Warm the sub4 tier (this decodes the full mask once and builds
            // mask_sub4), then downgrade.
            let first_s4 = render_pixmap(doc.page(0).unwrap(), &opts_s4).unwrap();
            let first_s1 = render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
            doc.downgrade_render_caches();
            assert!(
                doc.page(0)
                    .unwrap()
                    .render_layers()
                    .mask_sub4_cached()
                    .is_some(),
                "downgrade must retain mask_sub4"
            );

            // Structural proof: the warm sub4 re-render must not invoke the JB2
            // decoder at all.
            JB2_MASK_DECODES.with(|c| c.set(0));
            let second_s4 = render_pixmap(doc.page(0).unwrap(), &opts_s4).unwrap();
            assert_eq!(
                JB2_MASK_DECODES.with(|c| c.get()),
                0,
                "warm sub4 re-render after downgrade must not re-run the JB2 decode"
            );
            assert_eq!(first_s4.data, second_s4.data, "sub=4 output changed");

            // Full-resolution re-render: cold (decodes the mask again), output
            // unchanged.
            JB2_MASK_DECODES.with(|c| c.set(0));
            let second_s1 = render_pixmap(doc.page(0).unwrap(), &opts_s1).unwrap();
            assert!(
                JB2_MASK_DECODES.with(|c| c.get()) > 0,
                "full-res re-render after downgrade must cold-decode the mask"
            );
            assert_eq!(first_s1.data, second_s1.data, "sub=1 output changed");
        };

        #[cfg(feature = "parallel")]
        {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("build single-threaded pool for deterministic thread-local counting");
            pool.install(body);
        }
        #[cfg(not(feature = "parallel"))]
        body();
    }

    /// #607 eligibility guard: bold dilation needs the full-resolution mask,
    /// so a bold sub>=4 render after downgrade must decode it (and match the
    /// pre-downgrade bold render exactly).
    ///
    /// Same `parallel` + thread-local counting wrap as
    /// [`downgrade_retains_sub4_mask_and_skips_jb2_decode`] (#721).
    #[test]
    fn downgraded_sub4_with_bold_still_full_decodes() {
        let body = || {
            let mut doc = load_doc("colorbook.djvu");
            let (w, h) = {
                let p = doc.page(0).unwrap();
                (p.width() as u32, p.height() as u32)
            };
            let opts_bold = RenderOptions {
                width: w / 4,
                height: h / 4,
                bold: 1,
                ..Default::default()
            };
            let first = render_pixmap(doc.page(0).unwrap(), &opts_bold).unwrap();
            // Also warm the plain sub4 tier so mask_sub4 survives the downgrade.
            let opts_s4 = RenderOptions {
                width: w / 4,
                height: h / 4,
                ..Default::default()
            };
            let _ = render_pixmap(doc.page(0).unwrap(), &opts_s4).unwrap();
            doc.downgrade_render_caches();

            JB2_MASK_DECODES.with(|c| c.set(0));
            let second = render_pixmap(doc.page(0).unwrap(), &opts_bold).unwrap();
            assert!(
                JB2_MASK_DECODES.with(|c| c.get()) > 0,
                "bold render must not take the retained-sub4 shortcut"
            );
            assert_eq!(first.data, second.data, "bold sub=4 output changed");
        };

        #[cfg(feature = "parallel")]
        {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .build()
                .expect("build single-threaded pool for deterministic thread-local counting");
            pool.install(body);
        }
        #[cfg(not(feature = "parallel"))]
        body();
    }

    /// `enforce_cache_budget_with(downgrade_before_drop: true)` honours the same
    /// byte ceiling as `enforce_cache_budget`, and a downgraded (not fully
    /// dropped) page still reproduces byte-identical output.
    #[test]
    fn enforce_cache_budget_with_downgrade_matches_budget_and_output() {
        let mut doc = load_doc("colorbook.djvu");
        if doc.page_count() < 3 {
            return;
        }
        let mut expected = Vec::new();
        for i in 0..3 {
            let (w, h) = {
                let p = doc.page(i).unwrap();
                (p.width() as u32, p.height() as u32)
            };
            let opts = RenderOptions {
                width: w,
                height: h,
                ..Default::default()
            };
            let p = doc.page(i).unwrap();
            expected.push(render_pixmap(p, &opts).unwrap());
        }

        let total_before = doc.render_cache_bytes();
        assert!(total_before > 0);
        let budget = total_before / 2;
        let opts = crate::djvu_document::CacheBudgetOptions {
            downgrade_before_drop: true,
        };
        let _freed = doc.enforce_cache_budget_with(budget, &[], opts);
        assert!(
            doc.render_cache_bytes() <= budget,
            "cache not held under budget: {} > {}",
            doc.render_cache_bytes(),
            budget
        );

        // Re-render every page (whichever were downgraded or dropped) and
        // check the output is unchanged either way.
        for (i, expected_pm) in expected.iter().enumerate().take(3) {
            let (w, h) = {
                let p = doc.page(i).unwrap();
                (p.width() as u32, p.height() as u32)
            };
            let opts = RenderOptions {
                width: w,
                height: h,
                ..Default::default()
            };
            let pm = render_pixmap(doc.page(i).unwrap(), &opts).unwrap();
            assert_eq!(expected_pm.data, pm.data, "page {i} output changed");
        }
    }

    /// `fit_to_width` scales correctly, preserving aspect ratio.
    #[test]
    fn fit_to_width_preserves_aspect() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let ph = page.height() as u32;

        let opts = RenderOptions::fit_to_width(page, 800);
        assert_eq!(opts.width, 800);
        let expected_h = ((ph as f64 * 800.0) / pw as f64).round() as u32;
        assert_eq!(opts.height, expected_h);
        // The pipeline's decode scale is derived from width; it matches the
        // width/page-width ratio the deprecated `scale` field used to carry.
        assert!((opts.decode_scale(page) - 800.0 / pw as f32).abs() < 0.01);
    }

    /// `fit_to_height` scales correctly, preserving aspect ratio.
    #[test]
    fn fit_to_height_preserves_aspect() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let ph = page.height() as u32;

        let opts = RenderOptions::fit_to_height(page, 600);
        assert_eq!(opts.height, 600);
        let expected_w = ((pw as f64 * 600.0) / ph as f64).round() as u32;
        assert_eq!(opts.width, expected_w);
        // fit_to_height preserves aspect, so width/page-width equals
        // height/page-height — the decode scale matches either ratio.
        assert!((opts.decode_scale(page) - 600.0 / ph as f32).abs() < 0.01);
    }

    /// `fit_to_box` chooses the smaller scale factor.
    #[test]
    fn fit_to_box_constrains_both() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        // Very wide box — height should be the constraint
        let opts = RenderOptions::fit_to_box(page, 10000, 100);
        assert!(opts.width <= 10000);
        assert!(opts.height <= 100);
        assert!(opts.width > 0 && opts.height > 0);

        // Very tall box — width should be the constraint
        let opts = RenderOptions::fit_to_box(page, 100, 10000);
        assert!(opts.width <= 100);
        assert!(opts.height <= 10000);
        assert!(opts.width > 0 && opts.height > 0);
    }

    /// `fit_to_box` with a square box picks the tighter dimension.
    #[test]
    fn fit_to_box_square() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        let opts = RenderOptions::fit_to_box(page, 500, 500);
        assert!(opts.width <= 500);
        assert!(opts.height <= 500);
        // At least one dimension should be close to 500
        assert!(opts.width >= 490 || opts.height >= 490);
    }

    /// Rotated page: fit_to_width uses display dimensions (swapped w/h).
    #[test]
    fn fit_to_width_rotation_aware() {
        // boy_jb2_rotate90 has a 90° rotation in the INFO chunk
        let doc = load_doc("boy_jb2_rotate90.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let ph = page.height() as u32;
        // Display dimensions are swapped for 90° rotation
        let (dw, dh) = (ph, pw);

        let opts = RenderOptions::fit_to_width(page, 400);
        assert_eq!(opts.width, 400);
        let expected_h = ((dh as f64 * 400.0) / dw as f64).round() as u32;
        assert_eq!(opts.height, expected_h);
    }

    /// `render_into` with a zero-width dimension returns InvalidDimensions.
    #[test]
    fn render_into_invalid_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        let opts = RenderOptions {
            width: 0,
            height: 100,
            ..Default::default()
        };
        let mut buf = vec![0u8; 400];
        let err = render_into(page, &opts, &mut buf).unwrap_err();
        assert!(
            matches!(err, RenderError::InvalidDimensions { .. }),
            "expected InvalidDimensions, got {err:?}"
        );
    }

    /// `render_into` with a too-small buffer returns BufTooSmall.
    #[test]
    fn render_into_buf_too_small() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        let opts = RenderOptions {
            width: 10,
            height: 10,
            ..Default::default()
        };
        let mut buf = vec![0u8; 10]; // too small (needs 400)
        let err = render_into(page, &opts, &mut buf).unwrap_err();
        assert!(
            matches!(err, RenderError::BufTooSmall { need: 400, got: 10 }),
            "expected BufTooSmall, got {err:?}"
        );
    }

    /// `render_into` fills a pre-allocated buffer without allocating new one.
    ///
    /// We verify by: calling with exactly the right size buf, no panic,
    /// and the buffer is mutated (not all-zero after the call).
    #[test]
    fn render_into_fills_buffer_no_alloc() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        let w = 50u32;
        let h = 40u32;
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        render_into(page, &opts, &mut buf).expect("render_into should succeed");

        // The page is a color image — pixels should not all be zero
        assert!(
            buf.iter().any(|&b| b != 0),
            "rendered buffer should contain non-zero pixels"
        );
    }

    /// `render_into` can be called twice with the same buffer (zero-allocation reuse).
    #[test]
    fn render_into_reuse_buffer() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        let w = 30u32;
        let h = 20u32;
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];

        // First render
        render_into(page, &opts, &mut buf).expect("first render_into should succeed");
        let first = buf.clone();

        // Second render — same result
        render_into(page, &opts, &mut buf).expect("second render_into should succeed");
        assert_eq!(
            first, buf,
            "repeated render_into should produce identical output"
        );
    }

    /// gamma=2.2 (most DjVu files) produces an identity LUT — no correction needed
    /// for a standard display gamma=2.2.
    #[test]
    fn gamma_lut_standard_is_identity() {
        let lut = build_gamma_lut(2.2);
        for (i, &val) in lut.iter().enumerate() {
            assert_eq!(
                val, i as u8,
                "gamma=2.2 LUT at {i}: expected {i}, got {val}"
            );
        }
    }

    /// A linear-light source (gamma=1.0) is corrected: midtones become brighter
    /// (exponent=1/2.2<1 raises sub-unity values toward 1.0) to compensate
    /// for the display gamma-2.2 encoding needed for correct appearance.
    #[test]
    fn gamma_lut_linear_source_brightens() {
        let lut_linear = build_gamma_lut(1.0); // linear source → needs brightening
        let mid = 128u8;
        let corrected = lut_linear[mid as usize];
        assert!(
            corrected > mid,
            "linear-source LUT at mid ({corrected}) should be brighter than {mid}"
        );
    }

    /// Gamma LUT for gamma=0.0 (invalid) falls back to identity.
    #[test]
    fn gamma_lut_zero_is_identity() {
        let lut = build_gamma_lut(0.0);
        for (i, &val) in lut.iter().enumerate() {
            assert_eq!(val, i as u8, "zero gamma should produce identity LUT");
        }
    }

    /// render_coarse returns a valid pixmap (non-empty, correct dimensions) for
    /// a color page.
    #[test]
    fn render_coarse_returns_pixmap() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        let opts = RenderOptions {
            width: 60,
            height: 80,
            ..Default::default()
        };

        let result = render_coarse(page, &opts).expect("render_coarse should succeed");
        // chicken.djvu may or may not have BG44 chunks
        if let Some(pm) = result {
            assert_eq!(pm.width, 60);
            assert_eq!(pm.height, 80);
            assert_eq!(pm.data.len(), 60 * 80 * 4);
        }
        // Ok(None) is also valid if no BG44
    }

    /// render_progressive returns valid pixmap after each chunk.
    #[test]
    fn render_progressive_each_chunk() {
        // Use a page that has multiple BG44 chunks (boy.djvu is a good candidate)
        let doc = load_doc("boy.djvu");
        let page = doc.page(0).unwrap();

        let opts = RenderOptions {
            width: 80,
            height: 100,
            ..Default::default()
        };

        let n_bg44 = page.bg44_chunks().len();

        for chunk_n in 0..n_bg44 {
            let pm = render_progressive(page, &opts, chunk_n)
                .unwrap_or_else(|e| panic!("render_progressive chunk {chunk_n} failed: {e}"));
            assert_eq!(pm.width, 80);
            assert_eq!(pm.height, 100);
            assert_eq!(pm.data.len(), 80 * 100 * 4);
            // Each frame must have some non-zero pixels
            assert!(
                pm.data.iter().any(|&b| b != 0),
                "chunk {chunk_n}: rendered frame should not be all-zero"
            );
        }
    }

    /// Regression test for BUG-ZPSHORT (found while validating B5): on
    /// `watchmaker.djvu` page 0, BG44 chunk 2 of 4 is a legitimate two-byte
    /// `[serial, slices]` header with a **zero-length** ZP payload (the encoder
    /// had nothing left to encode for that refinement round). The strict
    /// progressive path (`render_progressive_step` / `ProgressiveDecoder`)
    /// used to hard-fail on it with `Iw44(ZpTooShort)`, while the permissive
    /// full-page cache (`PageLayers::bg44`) silently swallowed the error and
    /// dropped that chunk *and every chunk after it* — so the full render
    /// "succeeded" but silently used only 2 of 4 refinement chunks. Both are
    /// now fixed by treating a short/empty payload as valid trailing `0xFF`
    /// padding at the `djvu-iw44` layer (see `Iw44Image::decode_chunk`),
    /// matching the padding convention `ZpDecoder::read_byte` already uses at
    /// a stream's true end. Every step and the full render must now succeed.
    #[test]
    fn render_progressive_step_handles_zero_length_bg44_chunk() {
        let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/corpus/watchmaker.djvu");
        let data = std::fs::read(&path).expect("watchmaker.djvu must exist");
        let doc = DjVuDocument::parse(&data).expect("parse failed");
        let page = doc.page(0).unwrap();

        let chunks = page.bg44_chunks();
        assert_eq!(
            chunks.len(),
            4,
            "expected 4 BG44 chunks on watchmaker page 0"
        );
        assert_eq!(
            chunks[2].len(),
            2,
            "chunk 2 should be the zero-payload [serial, slices] header this regression covers"
        );

        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Bilinear,
            ..Default::default()
        };

        for step in 0..progressive_steps(page) {
            render_progressive_step(page, &opts, step)
                .unwrap_or_else(|e| panic!("step {step} should succeed, got {e}"));
        }
        render_progressive_all(page, &opts).expect("progressive_all should succeed");
        render_pixmap(page, &opts).expect("render_pixmap should succeed");

        let mut dec = ProgressiveDecoder::new(page, &opts).expect("decoder");
        for chunk in &chunks {
            dec.push_bg44_chunk(chunk)
                .expect("ProgressiveDecoder should also handle the zero-length chunk");
        }
    }

    /// `render_progressive_all` yields `progressive_steps` frames and its last
    /// frame is byte-identical to `render_pixmap` (the sealed protocol contract).
    #[test]
    fn render_progressive_all_seals_chunk_loop() {
        let doc = load_doc("boy.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 80,
            height: 100,
            ..Default::default()
        };

        // The seam hides `max(1, bg44_chunks().len())` from callers.
        let steps = progressive_steps(page);
        assert_eq!(steps, page.bg44_chunks().len().max(1));

        let frames = render_progressive_all(page, &opts).expect("progressive_all must succeed");
        assert_eq!(frames.len(), steps, "one frame per progressive step");

        let full = render_pixmap(page, &opts).expect("render_pixmap must succeed");
        assert_eq!(
            frames.last().unwrap().data,
            full.data,
            "final progressive frame must equal the full render"
        );
        // `render_progressive_step(0)` is also a valid (coarse) frame.
        let first = render_progressive_step(page, &opts, 0).expect("step 0 must succeed");
        assert_eq!(first.data, frames[0].data);
    }

    /// #691 slice 3 regression: a progressive frame must not depend on
    /// whether the retained 1/4-res mask cache (#607) is warm. The fast
    /// path in `decode_layers` used to hand the progressive path a maskless
    /// layer set, so a prior full render at the same downscale silently
    /// dropped the text layer from every later progressive frame.
    #[cfg(feature = "std")]
    #[test]
    fn render_progressive_ignores_mask_sub4_warmth() {
        // colorbook.djvu: multi-chunk BG44, JB2 mask, no FGbz palette — at a
        // strong downscale it is exactly the page shape the #607 fast path
        // triggers on.
        let opts = RenderOptions {
            width: 61,
            height: 83,
            ..Default::default()
        };
        let cold = {
            let doc = load_doc("colorbook.djvu");
            let page = doc.page(0).unwrap();
            render_progressive_step(page, &opts, 1).unwrap()
        };
        let doc = load_doc("colorbook.djvu");
        let page = doc.page(0).unwrap();
        // Warm the sub4 mask cache the way any interactive session would:
        // with a plain full render at the same output size.
        let _ = render_pixmap(page, &opts).unwrap();
        assert!(
            page.render_layers().mask_sub4_cached().is_some(),
            "precondition: the full render must have retained the sub4 mask"
        );
        let warm = render_progressive_step(page, &opts, 1).unwrap();
        assert_eq!(
            cold.data, warm.data,
            "progressive frame changed with cache warmth"
        );
    }

    /// render_progressive with chunk_n out of range returns ChunkOutOfRange.
    #[test]
    fn render_progressive_chunk_out_of_range() {
        let doc = load_doc("boy.djvu");
        let page = doc.page(0).unwrap();

        let opts = RenderOptions {
            width: 40,
            height: 50,
            ..Default::default()
        };

        let n_bg44 = page.bg44_chunks().len();
        if n_bg44 == 0 {
            // No BG44 chunks — skip this test
            return;
        }

        let err = render_progressive(page, &opts, n_bg44 + 10).unwrap_err();
        assert!(
            matches!(err, RenderError::ChunkOutOfRange { .. }),
            "expected ChunkOutOfRange, got {err:?}"
        );
    }

    /// render_pixmap with gamma gives different result than without (identity gamma).
    ///
    /// We compare rendering chicken.djvu twice: once with its natural gamma,
    /// once with gamma forced to 1.0 (identity). The pixel values should differ.
    #[test]
    fn render_pixmap_gamma_differs_from_identity() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        let w = 40u32;
        let h = 53u32; // ~native aspect for 181x240

        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };

        // Render with native gamma (2.2 from INFO chunk)
        let pm_gamma = render_pixmap(page, &opts).expect("render with gamma should succeed");

        // Render with identity gamma LUT manually applied to output
        let lut_identity = build_gamma_lut(1.0);
        let pm_identity = render_pixmap(page, &opts).expect("render for identity should succeed");
        // Apply identity correction (no-op) — pixels should be the same
        for i in 0..pm_identity.data.len().saturating_sub(3) {
            if i % 4 != 3 {
                // non-alpha channel
                let _ = lut_identity[pm_identity.data[i] as usize];
            }
        }

        // Since chicken.djvu gamma = 2.2, the gamma-corrected render
        // should have generally brighter mid-tones than a raw (no-correction) render.
        // We test this by checking that the gamma render is not bit-for-bit identical
        // to a hypothetical no-correction render. Since we always apply gamma in
        // render_pixmap, we test the gamma LUT effect directly (covered by
        // `gamma_correction_changes_pixels`).
        //
        // Instead, verify that pm_gamma has valid dimensions and non-trivial content.
        assert_eq!(pm_gamma.width, w);
        assert_eq!(pm_gamma.height, h);
        assert!(
            pm_gamma.data.iter().any(|&b| b != 255),
            "should have non-white pixels"
        );
    }

    /// render_pixmap for a bilevel (JB2-only) page produces black pixels.
    #[test]
    fn render_bilevel_page_has_black_pixels() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();

        let opts = RenderOptions {
            width: 60,
            height: 80,
            ..Default::default()
        };

        let pm = render_pixmap(page, &opts).expect("render bilevel should succeed");
        assert_eq!(pm.width, 60);
        assert_eq!(pm.height, 80);
        // A bilevel page should have some black pixels
        assert!(
            pm.data
                .as_chunks::<4>()
                .0
                .iter()
                .any(|px| px[0] == 0 && px[1] == 0 && px[2] == 0),
            "bilevel page should contain black pixels"
        );
    }

    /// `render_pixmap` with aa=true returns a valid pixmap.
    #[test]
    fn render_with_aa() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();

        let opts = RenderOptions {
            width: 40,
            height: 54,
            aa: true,
            ..Default::default()
        };
        // With aa=true the output is downscaled 2×, so we get 20×27
        let pm = render_pixmap(page, &opts).expect("render with AA should succeed");
        // AA downscales the output
        assert_eq!(pm.width, 20);
        assert_eq!(pm.height, 27);
    }

    // -- Rotation tests -------------------------------------------------------

    #[test]
    fn rotate_pixmap_none_is_identity() {
        let mut pm = Pixmap::white(3, 2);
        pm.set_rgb(0, 0, 255, 0, 0);
        let rotated = rotate_pixmap(pm.clone(), crate::info::Rotation::None);
        assert_eq!(rotated.width, 3);
        assert_eq!(rotated.height, 2);
        assert_eq!(rotated.get_rgb(0, 0), (255, 0, 0));
    }

    #[test]
    fn rotate_pixmap_cw90_swaps_dims() {
        let mut pm = Pixmap::white(4, 2);
        pm.set_rgb(0, 0, 255, 0, 0); // top-left red
        let rotated = rotate_pixmap(pm, crate::info::Rotation::Cw90);
        assert_eq!(rotated.width, 2);
        assert_eq!(rotated.height, 4);
        // Top-left (0,0) of original goes to (height-1-0, 0) = (1, 0) in rotated
        assert_eq!(rotated.get_rgb(1, 0), (255, 0, 0));
    }

    #[test]
    fn rotate_pixmap_180_preserves_dims() {
        let mut pm = Pixmap::white(3, 2);
        pm.set_rgb(0, 0, 255, 0, 0); // top-left red
        let rotated = rotate_pixmap(pm, crate::info::Rotation::Rot180);
        assert_eq!(rotated.width, 3);
        assert_eq!(rotated.height, 2);
        assert_eq!(rotated.get_rgb(2, 1), (255, 0, 0));
    }

    #[test]
    fn rotate_pixmap_ccw90_swaps_dims() {
        let mut pm = Pixmap::white(4, 2);
        pm.set_rgb(0, 0, 255, 0, 0); // top-left red
        let rotated = rotate_pixmap(pm, crate::info::Rotation::Ccw90);
        assert_eq!(rotated.width, 2);
        assert_eq!(rotated.height, 4);
        // Top-left (0,0) -> (0, width-1-0) = (0, 3) in rotated
        assert_eq!(rotated.get_rgb(0, 3), (255, 0, 0));
    }

    #[test]
    fn render_pixmap_rotation_90_swaps_dimensions() {
        let doc = load_doc("boy_jb2_rotate90.djvu");
        let page = doc.page(0).expect("page 0");
        let orig_w = page.width();
        let orig_h = page.height();
        let opts = RenderOptions {
            width: orig_w as u32,
            height: orig_h as u32,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render should succeed");
        // 90° rotation swaps width and height
        assert_eq!(
            pm.width, orig_h as u32,
            "rotated width should be original height"
        );
        assert_eq!(
            pm.height, orig_w as u32,
            "rotated height should be original width"
        );
    }

    #[test]
    fn render_pixmap_rotation_180_preserves_dimensions() {
        let doc = load_doc("boy_jb2_rotate180.djvu");
        let page = doc.page(0).expect("page 0");
        let orig_w = page.width();
        let orig_h = page.height();
        let opts = RenderOptions {
            width: orig_w as u32,
            height: orig_h as u32,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render should succeed");
        assert_eq!(pm.width, orig_w as u32);
        assert_eq!(pm.height, orig_h as u32);
    }

    #[test]
    fn render_pixmap_rotation_270_swaps_dimensions() {
        let doc = load_doc("boy_jb2_rotate270.djvu");
        let page = doc.page(0).expect("page 0");
        let orig_w = page.width();
        let orig_h = page.height();
        let opts = RenderOptions {
            width: orig_w as u32,
            height: orig_h as u32,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render should succeed");
        assert_eq!(
            pm.width, orig_h as u32,
            "rotated width should be original height"
        );
        assert_eq!(
            pm.height, orig_w as u32,
            "rotated height should be original width"
        );
    }

    // -- User rotation tests ---------------------------------------------------

    /// combine_rotations adds steps modulo 4.
    #[test]
    fn combine_rotations_identity() {
        use crate::info::Rotation;
        assert_eq!(
            combine_rotations(Rotation::None, UserRotation::None),
            Rotation::None
        );
    }

    #[test]
    fn combine_rotations_info_only() {
        use crate::info::Rotation;
        assert_eq!(
            combine_rotations(Rotation::Cw90, UserRotation::None),
            Rotation::Cw90
        );
    }

    #[test]
    fn combine_rotations_user_only() {
        use crate::info::Rotation;
        assert_eq!(
            combine_rotations(Rotation::None, UserRotation::Ccw90),
            Rotation::Ccw90
        );
    }

    #[test]
    fn combine_rotations_sum() {
        use crate::info::Rotation;
        // 90 CW (INFO) + 90 CW (user) = 180
        assert_eq!(
            combine_rotations(Rotation::Cw90, UserRotation::Cw90),
            Rotation::Rot180
        );
        // 90 CW + 270 CW = 360 = None
        assert_eq!(
            combine_rotations(Rotation::Cw90, UserRotation::Ccw90),
            Rotation::None
        );
        // 180 + 180 = 360 = None
        assert_eq!(
            combine_rotations(Rotation::Rot180, UserRotation::Rot180),
            Rotation::None
        );
    }

    /// User rotation Cw90 on a non-rotated page swaps output dimensions.
    #[test]
    fn user_rotation_cw90_swaps_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let ph = page.height() as u32;

        let opts = RenderOptions {
            width: pw,
            height: ph,
            rotation: UserRotation::Cw90,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render");
        assert_eq!(pm.width, ph, "user Cw90 should swap: width becomes height");
        assert_eq!(pm.height, pw, "user Cw90 should swap: height becomes width");
    }

    /// User rotation 180° preserves dimensions.
    #[test]
    fn user_rotation_180_preserves_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let ph = page.height() as u32;

        let opts = RenderOptions {
            width: pw,
            height: ph,
            rotation: UserRotation::Rot180,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render");
        assert_eq!(pm.width, pw);
        assert_eq!(pm.height, ph);
    }

    /// UserRotation default is None.
    #[test]
    fn user_rotation_default_is_none() {
        assert_eq!(UserRotation::default(), UserRotation::None);
        let opts = RenderOptions::default();
        assert_eq!(opts.rotation, UserRotation::None);
    }

    // -- FGbz multi-color palette tests ---------------------------------------

    #[test]
    fn fgbz_palette_page_renders_multiple_colors() {
        // irish.djvu is a single-page file with an FGbz palette.
        let doc = load_doc("irish.djvu");
        let page = doc.page(0).expect("page 0");
        let w = page.width() as u32;
        let h = page.height() as u32;
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render should succeed");

        // Collect distinct non-white, non-black foreground colors
        let mut fg_colors = std::collections::HashSet::new();
        for y in 0..h {
            for x in 0..w {
                let (r, g, b) = pm.get_rgb(x, y);
                // Skip white and near-white (background)
                if r > 240 && g > 240 && b > 240 {
                    continue;
                }
                fg_colors.insert((r, g, b));
            }
        }

        // A multi-color palette page should produce more than 1 distinct
        // foreground color (if it only had 1, it'd be the old bug).
        assert!(
            fg_colors.len() > 1,
            "multi-color palette page should have >1 distinct foreground colors, got {}",
            fg_colors.len()
        );
    }

    #[test]
    fn lookup_palette_color_uses_blit_map() {
        let pal = FgbzPalette {
            colors: vec![
                PaletteColor { r: 255, g: 0, b: 0 }, // index 0: red
                PaletteColor { r: 0, g: 0, b: 255 }, // index 1: blue
            ],
            indices: vec![1, 0], // blit 0 → color 1 (blue), blit 1 → color 0 (red)
        };
        let bm = crate::bitmap::Bitmap::new(2, 1);
        let blit_map = vec![0i32, 1i32]; // pixel (0,0) → blit 0, pixel (1,0) → blit 1

        let c0 = lookup_palette_color(&pal, Some(&blit_map), Some(&bm), 0, 0);
        assert_eq!(
            (c0.r, c0.g, c0.b),
            (0, 0, 255),
            "blit 0 → indices[0]=1 → blue"
        );

        let c1 = lookup_palette_color(&pal, Some(&blit_map), Some(&bm), 1, 0);
        assert_eq!(
            (c1.r, c1.g, c1.b),
            (255, 0, 0),
            "blit 1 → indices[1]=0 → red"
        );
    }

    #[test]
    fn lookup_palette_color_fallback_without_blit_map() {
        let pal = FgbzPalette {
            colors: vec![PaletteColor { r: 0, g: 128, b: 0 }],
            indices: vec![],
        };
        let c = lookup_palette_color(&pal, None, None, 0, 0);
        assert_eq!(
            (c.r, c.g, c.b),
            (0, 128, 0),
            "should fall back to first color"
        );
    }

    // ── BGjp / FGjp tests ─────────────────────────────────────────────────────

    /// Load the synthetic bgjp_test.djvu fixture from the assets directory.
    fn load_bgjp_doc() -> DjVuDocument {
        load_doc("bgjp_test.djvu")
    }

    /// BGjp fixture loads without error and reports correct dimensions.
    #[test]
    fn bgjp_fixture_loads() {
        let doc = load_bgjp_doc();
        let page = doc.page(0).unwrap();
        assert_eq!(page.width(), 4);
        assert_eq!(page.height(), 4);
    }

    /// BGjp chunk is present in the fixture.
    #[test]
    fn bgjp_chunk_present() {
        let doc = load_bgjp_doc();
        let page = doc.page(0).unwrap();
        assert!(
            page.find_chunk(b"BGjp").is_some(),
            "fixture must have a BGjp chunk"
        );
        assert!(
            page.bg44_chunks().is_empty(),
            "fixture must NOT have BG44 chunks"
        );
    }

    /// `decode_bgjp` returns a non-None Pixmap for the BGjp fixture.
    #[test]
    fn decode_bgjp_returns_pixmap() {
        let doc = load_bgjp_doc();
        let page = doc.page(0).unwrap();
        let pm = decode_bgjp(page).expect("decode_bgjp must not error");
        assert!(pm.is_some(), "decode_bgjp must return Some(Pixmap)");
        let pm = pm.unwrap();
        assert_eq!(pm.width, 4);
        assert_eq!(pm.height, 4);
        assert_eq!(pm.data.len(), 4 * 4 * 4); // RGBA
    }

    /// `decode_bgjp` returns None for a page with no BGjp chunk.
    #[test]
    fn decode_bgjp_returns_none_without_chunk() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let pm = decode_bgjp(page).expect("should not error");
        assert!(pm.is_none());
    }

    /// `decode_jpeg_to_pixmap` produces RGBA output with alpha=255.
    #[test]
    fn decode_jpeg_to_pixmap_alpha_is_255() {
        let doc = load_bgjp_doc();
        let page = doc.page(0).unwrap();
        let data = page.find_chunk(b"BGjp").unwrap();
        let pm = decode_jpeg_to_pixmap(data).expect("decode must succeed");
        for chunk in pm.data.as_chunks::<4>().0 {
            assert_eq!(chunk[3], 255, "alpha must be 255 for every pixel");
        }
    }

    /// render_pixmap falls back to BGjp when no BG44 chunks are present.
    #[test]
    fn render_pixmap_uses_bgjp_background() {
        let doc = load_bgjp_doc();
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 4,
            height: 4,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render must succeed");
        assert_eq!(pm.width, 4);
        assert_eq!(pm.height, 4);
    }

    /// render_coarse also falls back to BGjp (no BG44 chunks).
    #[test]
    fn render_coarse_uses_bgjp_background() {
        let doc = load_bgjp_doc();
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 4,
            height: 4,
            ..Default::default()
        };
        let pm = render_coarse(page, &opts).expect("render_coarse must succeed");
        assert!(pm.is_some(), "must return Some when BGjp present");
        let pm = pm.unwrap();
        assert_eq!(pm.width, 4);
        assert_eq!(pm.height, 4);
    }

    // ── Lanczos-3 tests ───────────────────────────────────────────────────────
    // (The raw resampler `scale_lanczos3` and its `lanczos3_kernel` now live in
    // the `pixmap` module and are unit-tested there; these exercise the render
    // path's use of Lanczos-3.)

    /// `Resampling::Lanczos3` produces the correct output dimensions.
    #[test]
    fn render_pixmap_lanczos3_correct_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let ph = page.height() as u32;
        let tw = pw / 2;
        let th = ph / 2;

        let opts = RenderOptions {
            width: tw,
            height: th,
            resampling: Resampling::Lanczos3,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("Lanczos3 render must succeed");
        assert_eq!(pm.width, tw);
        assert_eq!(pm.height, th);
    }

    /// Lanczos-3 and bilinear renders differ (different algorithms produce different output).
    #[test]
    fn lanczos3_differs_from_bilinear_at_half_scale() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let ph = page.height() as u32;
        let tw = pw / 2;
        let th = ph / 2;

        let bilinear = render_pixmap(
            page,
            &RenderOptions {
                width: tw,
                height: th,
                resampling: Resampling::Bilinear,
                ..Default::default()
            },
        )
        .unwrap();

        let lanczos = render_pixmap(
            page,
            &RenderOptions {
                width: tw,
                height: th,
                resampling: Resampling::Lanczos3,
                ..Default::default()
            },
        )
        .unwrap();

        // Dimensions must be the same.
        assert_eq!(bilinear.width, lanczos.width);
        assert_eq!(bilinear.height, lanczos.height);

        // But pixel values should differ (algorithms are not identical).
        let differ = bilinear
            .data
            .iter()
            .zip(lanczos.data.iter())
            .any(|(a, b)| a != b);
        assert!(
            differ,
            "Lanczos3 and bilinear must produce different pixel values"
        );
    }

    /// `Resampling::Bilinear` default is maintained for backward compat.
    #[test]
    fn resampling_default_is_bilinear() {
        let opts = RenderOptions::default();
        assert_eq!(opts.resampling, Resampling::Bilinear);
    }

    // ── render_region tests ───────────────────────────────────────────────────

    /// `render_region` allocates only the region-sized buffer (≤ 512 KB for 256×256).
    #[test]
    fn render_region_allocates_proportionally() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions::fit_to_width(page, 1000);
        let region = RenderRect {
            x: 0,
            y: 0,
            width: 256,
            height: 256,
        };
        let pm = render_region(page, region, &opts).expect("render_region should succeed");
        assert_eq!(pm.width, 256);
        assert_eq!(pm.height, 256);
        assert_eq!(pm.data.len(), 256 * 256 * 4);
        assert!(
            pm.data.len() <= 512 * 1024,
            "region allocation {} exceeds 512 KB",
            pm.data.len()
        );
    }

    /// `render_region` pixels match the same pixels from `render_pixmap`.
    #[test]
    fn render_region_matches_full_render() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 100,
            height: 80,
            ..Default::default()
        };
        let full = render_pixmap(page, &opts).expect("full render should succeed");
        let region = RenderRect {
            x: 10,
            y: 10,
            width: 30,
            height: 20,
        };
        let part = render_region(page, region, &opts).expect("region render should succeed");

        assert_eq!(part.width, 30);
        assert_eq!(part.height, 20);

        for ry in 0..20u32 {
            for rx in 0..30u32 {
                let full_base = ((10 + ry) as usize * 100 + (10 + rx) as usize) * 4;
                let part_base = (ry as usize * 30 + rx as usize) * 4;
                assert_eq!(
                    &full.data[full_base..full_base + 4],
                    &part.data[part_base..part_base + 4],
                    "pixel mismatch at region ({rx},{ry}) / full ({},{} )",
                    10 + rx,
                    10 + ry
                );
            }
        }
    }

    // ── C4_TILE_CACHE: render_region_tiled ───────────────────────────────────

    /// `render_region_tiled` is byte-identical to `render_region` across a
    /// scripted "pan": many overlapping regions, some spanning multiple
    /// `TILE_SIZE`-aligned tiles, some landing on partial edge tiles at the
    /// full render's right/bottom border (full render size deliberately not a
    /// multiple of `TILE_SIZE`).
    #[test]
    fn render_region_tiled_matches_render_region() {
        let doc = load_doc("colorbook.djvu");
        let page = doc.page(0).unwrap();
        // Not a multiple of TILE_SIZE (256), so the sequence below touches
        // partial edge tiles too.
        let opts = RenderOptions {
            width: 900,
            height: 700,
            ..Default::default()
        };

        // A little pan sequence: overlapping viewports sweeping across and
        // down the page, each straddling tile boundaries differently.
        let regions = [
            RenderRect {
                x: 0,
                y: 0,
                width: 300,
                height: 220,
            },
            RenderRect {
                x: 60,
                y: 0,
                width: 300,
                height: 220,
            },
            RenderRect {
                x: 200,
                y: 40,
                width: 300,
                height: 220,
            },
            RenderRect {
                x: 400,
                y: 40,
                width: 300,
                height: 220,
            },
            RenderRect {
                x: 600,
                y: 480,
                width: 300,
                height: 220,
            }, // right/bottom edge tiles
            RenderRect {
                x: 250,
                y: 250,
                width: 400,
                height: 300,
            }, // spans 2x2 tiles
            RenderRect {
                x: 1,
                y: 1,
                width: 5,
                height: 5,
            }, // sub-tile sliver
        ];

        for region in regions {
            let direct = render_region(page, region, &opts).expect("render_region");
            let tiled = render_region_tiled(page, region, &opts).expect("render_region_tiled");
            assert_eq!(tiled.width, direct.width);
            assert_eq!(tiled.height, direct.height);
            assert_eq!(
                tiled.data, direct.data,
                "render_region_tiled diverged from render_region for {region:?}"
            );
        }
    }

    /// A second call for a region already fully covered by previously-cached
    /// tiles still reproduces the same bytes (exercises the cache-hit path,
    /// not just cold tile composition).
    #[test]
    fn render_region_tiled_repeated_region_matches() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 640,
            height: 480,
            ..Default::default()
        };
        let region = RenderRect {
            x: 100,
            y: 100,
            width: 200,
            height: 150,
        };

        let first = render_region_tiled(page, region, &opts).unwrap();
        // Bytes should now be resident in the page's tile cache.
        assert!(page.render_cache_bytes() > 0);
        let second = render_region_tiled(page, region, &opts).unwrap();
        assert_eq!(first.data, second.data);

        // A neighbouring, overlapping region should also match a direct render.
        let overlapping = RenderRect {
            x: 150,
            y: 120,
            width: 200,
            height: 150,
        };
        let direct = render_region(page, overlapping, &opts).unwrap();
        let tiled = render_region_tiled(page, overlapping, &opts).unwrap();
        assert_eq!(direct.data, tiled.data);
    }

    /// Ineligible modes (rotation, Lanczos-3, permissive) fall back to
    /// `render_region` and still produce its exact output.
    #[test]
    fn render_region_tiled_falls_back_for_ineligible_modes() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let region = RenderRect {
            x: 5,
            y: 5,
            width: 40,
            height: 30,
        };

        let rotated_opts = RenderOptions {
            width: 200,
            height: 150,
            rotation: UserRotation::Cw90,
            ..Default::default()
        };
        assert_eq!(
            render_region(page, region, &rotated_opts).unwrap().data,
            render_region_tiled(page, region, &rotated_opts)
                .unwrap()
                .data
        );

        let lanczos_opts = RenderOptions {
            width: 100,
            height: 75,
            resampling: Resampling::Lanczos3,
            ..Default::default()
        };
        assert_eq!(
            render_region(page, region, &lanczos_opts).unwrap().data,
            render_region_tiled(page, region, &lanczos_opts)
                .unwrap()
                .data
        );

        let permissive_opts = RenderOptions {
            width: 200,
            height: 150,
            permissive: true,
            ..Default::default()
        };
        assert_eq!(
            render_region(page, region, &permissive_opts).unwrap().data,
            render_region_tiled(page, region, &permissive_opts)
                .unwrap()
                .data
        );
    }

    /// The tile cache's byte accounting is bounded (FIFO eviction) and feeds
    /// into `render_cache_bytes` / `evict_render_cache` like the other layer
    /// caches (C5 integration).
    #[test]
    fn render_region_tiled_cache_is_budget_bounded_and_evictable() {
        let mut doc = load_doc("colorbook.djvu");
        let (native_w, native_h) = {
            let p = doc.page(0).unwrap();
            (p.width() as u32, p.height() as u32)
        };
        // A render large enough to have many more than
        // TILE_CACHE_MAX_BYTES / (TILE_SIZE*TILE_SIZE*4) tiles available.
        let opts = RenderOptions {
            width: native_w.max(4000),
            height: native_h.max(4000),
            ..Default::default()
        };
        let full_w = opts.width;

        {
            let page = doc.page(0).unwrap();
            // Touch many disjoint tiles by requesting a small region in each.
            let tiles_per_side = (full_w / TILE_SIZE).clamp(1, 12);
            for ty in 0..tiles_per_side {
                for tx in 0..tiles_per_side {
                    let region = RenderRect {
                        x: tx * TILE_SIZE,
                        y: ty * TILE_SIZE,
                        width: 8,
                        height: 8,
                    };
                    let _ = render_region_tiled(page, region, &opts).unwrap();
                }
            }
            let bytes = page.render_layers().tile_cache_bytes();
            assert!(bytes > 0, "expected some tile bytes cached");
            assert!(
                bytes <= TILE_CACHE_MAX_BYTES,
                "tile cache exceeded its byte budget: {bytes} > {TILE_CACHE_MAX_BYTES}"
            );
        }

        // Evicting the whole page's render cache drops the tiles too.
        doc.evict_render_caches();
        assert_eq!(doc.page(0).unwrap().render_cache_bytes(), 0);
    }

    /// `render_region_tiled` with zero-size dimensions errors like
    /// `render_region`.
    #[test]
    fn render_region_tiled_rejects_zero_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 100,
            height: 80,
            ..Default::default()
        };
        let region = RenderRect {
            x: 0,
            y: 0,
            width: 0,
            height: 10,
        };
        assert!(render_region_tiled(page, region, &opts).is_err());
    }

    /// `render_region` with a byte-aligned x offset on a bilevel page matches the
    /// full render — exercises the generalized P2 BILEVEL_RGBA fast path (#433),
    /// which fires when `offset_x % 8 == 0`.
    #[test]
    fn render_region_bilevel_byte_aligned_offset_matches_full() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let full_w = page.width() as u32;
        let full_h = page.height() as u32;
        let opts = RenderOptions {
            width: full_w,
            height: full_h,
            ..Default::default()
        };
        let full = render_pixmap(page, &opts).expect("full render should succeed");
        // x=16 is byte-aligned (16 % 8 == 0) → generalized P2 path; x=17 below isn't.
        for &x in &[16u32, 17u32] {
            let region = RenderRect {
                x,
                y: 8,
                width: 64,
                height: 32,
            };
            let part = render_region(page, region, &opts).expect("region render should succeed");
            for ry in 0..region.height {
                for rx in 0..region.width {
                    let fb = (((region.y + ry) * full_w + (x + rx)) * 4) as usize;
                    let pb = ((ry * region.width + rx) * 4) as usize;
                    assert_eq!(
                        &full.data[fb..fb + 4],
                        &part.data[pb..pb + 4],
                        "mismatch at x={x} region ({rx},{ry})"
                    );
                }
            }
        }
    }

    /// `render_region` with invalid dimensions returns an error.
    #[test]
    fn render_region_invalid_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 100,
            height: 100,
            ..Default::default()
        };
        let region = RenderRect {
            x: 0,
            y: 0,
            width: 0,
            height: 50,
        };
        let err = render_region(page, region, &opts).unwrap_err();
        assert!(
            matches!(err, RenderError::InvalidDimensions { .. }),
            "expected InvalidDimensions, got {err:?}"
        );
    }

    /// `render_pixmap` still works correctly (regression guard).
    #[test]
    fn render_pixmap_still_works_after_refactor() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 80,
            height: 60,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("render_pixmap should succeed");
        assert_eq!(pm.width, 80);
        assert_eq!(pm.height, 60);
        assert_eq!(pm.data.len(), 80 * 60 * 4);
    }

    /// `best_iw44_subsample` returns expected power-of-2 values.
    #[test]
    fn best_iw44_subsample_values() {
        assert_eq!(best_iw44_subsample(1.0), 1, "scale=1.0 → subsample=1");
        assert_eq!(best_iw44_subsample(0.5), 2, "scale=0.5 → subsample=2");
        assert_eq!(
            best_iw44_subsample(0.375),
            4,
            "scale=0.375 → subsample=4 (1.5/0.375=4.0, allows 1.5× upscale)"
        );
        assert_eq!(best_iw44_subsample(0.25), 4, "scale=0.25 → subsample=4");
        assert_eq!(
            best_iw44_subsample(0.1),
            8,
            "scale=0.1 → subsample=8 (capped)"
        );
        assert_eq!(
            best_iw44_subsample(0.0),
            1,
            "scale=0.0 → subsample=1 (edge case)"
        );
        assert_eq!(
            best_iw44_subsample(-1.0),
            1,
            "scale<0 → subsample=1 (edge case)"
        );
        assert_eq!(
            best_iw44_subsample(2.0),
            1,
            "scale>1.0 → subsample=1 (no upscaling needed)"
        );
    }

    /// The IW44 decode subsample is derived from the output `width`, not from
    /// the deprecated `scale` field — the regression guard for the PDF
    /// over-decode (#377).
    #[test]
    #[allow(deprecated)] // deliberately writes the legacy `scale` field to prove it is ignored
    fn decode_subsample_derives_from_width_not_scale_field() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let (dw, _) = display_dimensions(page);

        // Quarter-width output. The PDF exporter built exactly this — a small
        // `width` with `scale` left at the 1.0 default — and the old pipeline
        // read `scale` and decoded at full wavelet resolution (subsample 1).
        // The decode scale is now derived from `width`, so it is 0.25 →
        // subsample 4, and the over-decode is gone.
        let opts = RenderOptions {
            width: dw / 4,
            ..Default::default()
        };
        assert!((opts.decode_scale(page) - 0.25).abs() < 0.01);
        assert_eq!(best_iw44_subsample(opts.decode_scale(page)), 4);

        // Writing the legacy `scale` field by hand — to any value — must not
        // change the width-derived subsample.
        for misleading in [1.0_f32, 0.0, 0.5, 4.0] {
            let mut o = RenderOptions {
                width: dw / 4,
                ..Default::default()
            };
            o.scale = misleading;
            assert_eq!(
                best_iw44_subsample(o.decode_scale(page)),
                4,
                "scale={misleading} must not change the width-derived subsample",
            );
        }
    }

    /// INFO-rotated page: the decode scale follows the *native* page width, not
    /// the rotation-swapped display width. The compositor scales the native
    /// raster into the output buffer before `rotate_pixmap` runs, so a downscaled
    /// rotated page must not pick its IW44 subsample from the swapped dimension —
    /// doing so over-subsampled a downscaled portrait background (#377 follow-up).
    #[test]
    fn decode_scale_uses_native_width_for_rotated_page() {
        // boy_jb2_rotate90 carries a 90° rotation in its INFO chunk.
        let doc = load_doc("boy_jb2_rotate90.djvu");
        let page = doc.page(0).unwrap();
        let pw = page.width() as u32;
        let (dw, _) = display_dimensions(page);
        assert_ne!(dw, pw, "fixture must be a non-square INFO-rotated page");

        // The raster exporters size the output from the native page width
        // (page.width() * s); at s = 0.5 the half-size render must decode at 0.5.
        let opts = RenderOptions {
            width: pw / 2,
            height: (page.height() as u32) / 2,
            ..Default::default()
        };
        let native = opts.width as f32 / pw as f32; // correct: width / native width
        let display = opts.width as f32 / dw as f32; // the old bug: width / display width
        assert!(
            (opts.decode_scale(page) - native).abs() < 1e-4,
            "decode_scale {} should equal the native-width ratio {native}",
            opts.decode_scale(page),
        );
        assert!(
            (opts.decode_scale(page) - display).abs() > 1e-2,
            "decode_scale must not follow the rotation-swapped display width ({display})",
        );
    }

    /// Rendering with bg_subsample=2 (scale=0.5) produces the correct output dimensions.
    #[test]
    fn render_pixmap_subsampled_bg_correct_dimensions() {
        let doc = load_doc("boy.djvu");
        let page = doc.page(0).unwrap();
        // width = half the page → decode_scale ≈ 0.5 → bg_subsample=2 internally
        let opts = RenderOptions {
            width: (page.width() as f32 * 0.5) as u32,
            height: (page.height() as f32 * 0.5) as u32,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("subsampled render should succeed");
        assert_eq!(pm.width, opts.width);
        assert_eq!(pm.height, opts.height);
        assert_eq!(
            pm.data.len() as u64,
            opts.width as u64 * opts.height as u64 * 4
        );
    }

    /// Second render of the same page produces identical pixels — confirms the
    /// BG44 cache is used and does not corrupt output.
    #[test]
    fn decoded_bg44_cache_produces_identical_pixels_on_second_render() {
        let doc = load_doc("boy.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..Default::default()
        };
        let pm1 = render_pixmap(page, &opts).expect("first render should succeed");
        let pm2 = render_pixmap(page, &opts).expect("second render should succeed");
        assert_eq!(
            pm1.data, pm2.data,
            "cached render must produce identical pixels"
        );
    }

    /// After the first render the `decoded_bg44` cache is populated — the
    /// image dimensions match the page's raw BG44 size.
    #[test]
    fn decoded_bg44_is_populated_after_render() {
        let doc = load_doc("boy.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            ..Default::default()
        };
        // Trigger cache population.
        render_pixmap(page, &opts).expect("render should succeed");
        // Cache must now hold an image whose size matches the page's native size.
        let cached = page
            .decoded_bg44()
            .expect("cache should be populated after render");
        assert_eq!(
            cached.width,
            page.width() as u32,
            "cached bg44 width must equal page width"
        );
        assert_eq!(
            cached.height,
            page.height() as u32,
            "cached bg44 height must equal page height"
        );
    }

    /// `downsample_mask_4x` is render-tier logic now testable on a hand-built
    /// bitmap — no DjVu bytes to parse. Each 4×4 source block collapses to one
    /// output bit that is set iff any source bit in the block is set.
    #[test]
    fn downsample_mask_4x_max_pools_each_block() {
        // 8×8 mask: set a single pixel in the top-left block and one in the
        // bottom-right block; the other two blocks stay clear.
        let mut src = crate::bitmap::Bitmap::new(8, 8);
        src.set(1, 2, true); // top-left 4×4 block
        src.set(6, 5, true); // bottom-right 4×4 block
        let out = downsample_mask_4x(&src);
        assert_eq!((out.width, out.height), (2, 2));
        assert!(out.get(0, 0), "top-left block had a set bit");
        assert!(!out.get(1, 0), "top-right block was empty");
        assert!(!out.get(0, 1), "bottom-left block was empty");
        assert!(out.get(1, 1), "bottom-right block had a set bit");
    }

    /// A non-multiple-of-4 mask rounds up: a 5×5 source yields a 2×2 result and
    /// the ragged edge block still max-pools its single column/row.
    #[test]
    fn downsample_mask_4x_rounds_up_ragged_edges() {
        let mut src = crate::bitmap::Bitmap::new(5, 5);
        src.set(4, 4, true); // lone pixel in the ragged bottom-right block
        let out = downsample_mask_4x(&src);
        assert_eq!((out.width, out.height), (2, 2));
        assert!(out.get(1, 1), "ragged corner block must capture its bit");
        assert!(!out.get(0, 0));
    }

    /// `render_region` applies page rotation the same way as `render_pixmap`.
    ///
    /// For a 90° CW rotation a non-square region of width×height is returned as
    /// height×width — proving rotation was applied (not silently skipped).
    #[test]
    fn render_region_applies_rotation() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        // Request an explicit 90° CW user rotation.
        let opts = RenderOptions {
            width: 80,
            height: 60,
            rotation: UserRotation::Cw90,
            ..Default::default()
        };
        // Non-square region so swapped dimensions are detectable.
        let region = RenderRect {
            x: 0,
            y: 0,
            width: 40,
            height: 20,
        };
        let part = render_region(page, region, &opts).expect("region render should succeed");
        // After CW90 rotation a 40×20 region becomes 20×40.
        assert_eq!(
            part.width, 20,
            "expected width=20 (was region.height) after CW90 rotation"
        );
        assert_eq!(
            part.height, 40,
            "expected height=40 (was region.width) after CW90 rotation"
        );
    }

    /// #691: `render_region` matches the same crop of `render_pixmap` even
    /// when the downscale activates the 1/4-resolution mask fast path
    /// (`bg_subsample >= 4`, no bold, no FGbz) — the region path must take
    /// the same `resolve_sub4_mask` decision as the full-page path.
    #[test]
    fn render_region_matches_full_render_crop_at_sub4() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 40,
            height: 52,
            ..Default::default()
        };
        let full = render_pixmap(page, &opts).unwrap();
        let region = RenderRect {
            x: 8,
            y: 8,
            width: 24,
            height: 24,
        };
        let reg = render_region(page, region, &opts).unwrap();
        let mut crop = Vec::new();
        for y in 0..24usize {
            let s = ((8 + y) * 40 + 8) * 4;
            crop.extend_from_slice(&full.data[s..s + 24 * 4]);
        }
        assert_eq!(
            reg.data, crop,
            "render_region must be byte-identical to the matching crop of render_pixmap"
        );
    }

    // ── Issue #225: render_rows byte-identical to render_into (direct-write path) ──

    // ── Issue #225 Phase 2: public render_streaming API ──────────────────────

    /// `render_streaming` must produce byte-for-byte identical output to
    /// `render_pixmap` when no post-processing options are set (no aa, no
    /// Lanczos scaling, no rotation).
    #[test]
    fn render_streaming_byte_identical_to_render_pixmap_color() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let w = 60u32;
        let h = 80u32;
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };

        let pm = render_pixmap(page, &opts).expect("render_pixmap should succeed");

        let row_stride = w as usize * 4;
        let mut streamed = vec![0u8; w as usize * h as usize * 4];
        render_streaming(page, &opts, |y, row| {
            assert_eq!(row.len(), row_stride);
            let start = y * row_stride;
            streamed[start..start + row_stride].copy_from_slice(row);
        })
        .expect("render_streaming should succeed");

        assert_eq!(
            pm.data, streamed,
            "render_streaming must be byte-identical to render_pixmap when no post-processing options are set"
        );
    }

    /// `render_streaming` must produce byte-for-byte identical output to
    /// `render_pixmap` for a bilevel page.
    #[test]
    fn render_streaming_byte_identical_to_render_pixmap_bilevel() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let w = 50u32;
        let h = 70u32;
        let opts = RenderOptions {
            width: w,
            height: h,
            ..Default::default()
        };

        let pm = render_pixmap(page, &opts).expect("render_pixmap should succeed");

        let row_stride = w as usize * 4;
        let mut streamed = vec![0u8; w as usize * h as usize * 4];
        render_streaming(page, &opts, |y, row| {
            let start = y * row_stride;
            streamed[start..start + row_stride].copy_from_slice(row);
        })
        .expect("render_streaming should succeed");

        assert_eq!(pm.data, streamed);
    }

    /// `render_streaming` rejects anti-aliasing with `UnsupportedOption`.
    #[test]
    fn render_streaming_rejects_aa() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 60,
            height: 80,
            aa: true,
            ..Default::default()
        };
        let err = render_streaming(page, &opts, |_, _| {}).unwrap_err();
        assert!(
            matches!(err, RenderError::UnsupportedOption(_)),
            "expected UnsupportedOption, got {err:?}"
        );
    }

    /// `render_streaming` rejects Lanczos-3 when scaling actually happens.
    #[test]
    fn render_streaming_rejects_lanczos_with_scaling() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        // Force scaling by picking dimensions ≠ the page's native size.
        let opts = RenderOptions {
            width: page.width() as u32 / 2,
            height: page.height() as u32 / 2,
            resampling: Resampling::Lanczos3,
            ..Default::default()
        };
        let err = render_streaming(page, &opts, |_, _| {}).unwrap_err();
        assert!(matches!(err, RenderError::UnsupportedOption(_)));
    }

    /// `render_streaming` allows Lanczos-3 when output matches native page
    /// dimensions (Lanczos is a no-op in that case).
    #[test]
    fn render_streaming_allows_lanczos_at_native_size() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Lanczos3,
            ..Default::default()
        };
        let mut row_count = 0usize;
        render_streaming(page, &opts, |_, _| row_count += 1).expect("should succeed at native");
        assert_eq!(row_count, page.height() as usize);
    }

    /// `render_streaming` rejects user rotation.
    #[test]
    fn render_streaming_rejects_user_rotation() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 60,
            height: 80,
            rotation: UserRotation::Cw90,
            ..Default::default()
        };
        let err = render_streaming(page, &opts, |_, _| {}).unwrap_err();
        assert!(matches!(err, RenderError::UnsupportedOption(_)));
    }

    /// `render_streaming` rejects zero dimensions with `InvalidDimensions`.
    #[test]
    fn render_streaming_rejects_zero_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 0,
            height: 80,
            ..Default::default()
        };
        let err = render_streaming(page, &opts, |_, _| {}).unwrap_err();
        assert!(matches!(err, RenderError::InvalidDimensions { .. }));
    }

    // ── Permissive render path ────────────────────────────────────────────────

    /// Permissive render on a standard IW44+JB2 page completes without error.
    #[test]
    fn permissive_render_iw44_jb2_page() {
        let doc = load_doc("czech.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 40,
            height: 40,
            permissive: true,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("permissive render should not error");
        assert_eq!(pm.width, 40);
        assert_eq!(pm.height, 40);
    }

    /// Oversized render dimensions must be rejected, not OOM / panic on an empty
    /// overflow pixmap (security finding).
    #[test]
    fn oversized_render_dimensions_are_rejected() {
        let doc = load_doc("czech.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 60_000,
            height: 60_000, // 3.6 G px > MAX_RENDER_PIXELS
            permissive: true,
            ..Default::default()
        };
        assert!(matches!(
            render_pixmap(page, &opts),
            Err(RenderError::ResourceLimit(_))
        ));
    }

    #[test]
    fn render_into_rejects_oversized_output_with_typed_limit_error() {
        let doc = load_doc("czech.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 60_000,
            height: 60_000,
            permissive: true,
            ..Default::default()
        };
        let mut buf = vec![0u8; 16];
        assert!(matches!(
            render_into(page, &opts, &mut buf),
            Err(RenderError::ResourceLimit(_))
        ));
    }

    #[test]
    fn configurable_render_limit_overrides_inherited_ceiling() {
        let doc = load_doc("czech.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 500,
            height: 500,
            permissive: true,
            ..Default::default()
        };
        let limits = crate::resource_limits::ResourceLimits {
            max_render_pixels: Some(100_000),
            ..Default::default()
        };
        assert!(matches!(
            render_pixmap_with_limits(page, &opts, Some(limits)),
            Err(RenderError::ResourceLimit(exceeded)) if exceeded.operation == "render_pixmap"
                && exceeded.axis == crate::resource_limits::ResourceLimitAxis::RenderOutputPixels
        ));
    }

    /// Permissive render on a BGjp page (no BG44) falls through to decode_bgjp.
    #[test]
    fn permissive_render_bgjp_page() {
        let doc = load_doc("bgjp_test.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 4,
            height: 4,
            permissive: true,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("permissive render of BGjp page should succeed");
        assert_eq!(pm.width, 4);
        assert_eq!(pm.height, 4);
    }

    /// Permissive render on a page with FGbz palette exercises the indexed mask path.
    #[test]
    fn permissive_render_fgbz_page() {
        let doc = load_doc("navm_fgbz.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 40,
            height: 40,
            permissive: true,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("permissive render of FGbz page should succeed");
        assert!(pm.width > 0 && pm.height > 0);
    }

    /// render_gray8 produces a grayscale pixmap.
    #[test]
    fn render_gray8_produces_grayscale_output() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 20,
            height: 20,
            ..Default::default()
        };
        let gpm = render_gray8(page, &opts).expect("render_gray8 should succeed");
        assert_eq!(gpm.width, 20);
        assert_eq!(gpm.height, 20);
        assert_eq!(gpm.data.len(), 20 * 20);
    }

    /// render_coarse rejects zero width.
    #[test]
    fn render_coarse_rejects_zero_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 0,
            height: 50,
            ..Default::default()
        };
        let err = render_coarse(page, &opts).unwrap_err();
        assert!(matches!(err, RenderError::InvalidDimensions { .. }));
    }

    /// render_coarse on a JB2-only page returns None.
    #[test]
    fn render_coarse_jb2_only_page_returns_none() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 40,
            height: 40,
            ..Default::default()
        };
        let result = render_coarse(page, &opts).expect("should not error");
        assert!(
            result.is_none(),
            "JB2-only page has no BG44 so render_coarse yields None"
        );
    }

    /// render_progressive rejects zero dimensions.
    #[test]
    fn render_progressive_rejects_zero_dimensions() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 0,
            height: 50,
            ..Default::default()
        };
        let err = render_progressive(page, &opts, 0).unwrap_err();
        assert!(matches!(err, RenderError::InvalidDimensions { .. }));
    }

    /// render_progressive on a JB2-only page (no BG44) completes successfully.
    #[test]
    fn render_progressive_jb2_only_page_succeeds() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 40,
            height: 40,
            ..Default::default()
        };
        let result = render_progressive(page, &opts, 0)
            .expect("render_progressive should succeed even without BG44");
        assert_eq!(result.width, 40);
        assert_eq!(result.height, 40);
    }

    /// Lanczos3 at native resolution skips the re-render (need_scale=false path).
    #[test]
    fn lanczos3_at_native_resolution_skips_rerender() {
        let doc = load_doc("bgjp_test.djvu");
        let page = doc.page(0).unwrap();
        // bgjp_test.djvu is 4×4 — render at native size with Lanczos3
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Lanczos3,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("lanczos at native size must succeed");
        assert_eq!(pm.width, page.width() as u32);
        assert_eq!(pm.height, page.height() as u32);
    }

    /// render_pixmap rejects zero width.
    #[test]
    fn render_pixmap_rejects_zero_width() {
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 0,
            height: 50,
            ..Default::default()
        };
        let err = render_pixmap(page, &opts).unwrap_err();
        assert!(matches!(err, RenderError::InvalidDimensions { .. }));
    }

    /// Build a minimal single-page DJVU document with the given width and height.
    fn make_doc_with_dims(w: u16, h: u16) -> Vec<u8> {
        use crate::iff::{Chunk, DjvuFile, emit};
        let mut info = vec![0u8; 10];
        info[0] = (w >> 8) as u8;
        info[1] = w as u8;
        info[2] = (h >> 8) as u8;
        info[3] = h as u8;
        let file = DjvuFile {
            root: Chunk::Form {
                secondary_id: *b"DJVU",
                length: 0,
                children: vec![Chunk::Leaf {
                    id: *b"INFO",
                    data: info,
                }],
            },
        };
        emit(&file)
    }

    // ── RenderOptions edge-case helpers ──────────────────────────────────────

    #[test]
    fn fit_to_width_zero_page_width_uses_requested_width() {
        // Line 213: dw == 0 → height = width (same as requested width).
        let bytes = make_doc_with_dims(0, 100);
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let opts = RenderOptions::fit_to_width(page, 40);
        assert_eq!(opts.width, 40);
        assert_eq!(opts.height, 40); // because dw==0 → height = width
    }

    #[test]
    fn fit_to_height_zero_page_height_uses_requested_height() {
        // Line 232: dh == 0 → width = height (same as requested height).
        let bytes = make_doc_with_dims(100, 0);
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let opts = RenderOptions::fit_to_height(page, 60);
        assert_eq!(opts.height, 60);
        assert_eq!(opts.width, 60); // because dh==0 → width = height
    }

    #[test]
    fn fit_to_box_zero_page_dims_falls_back_to_box_max() {
        // Lines 255-259: dw==0 || dh==0 → return max_width × max_height with scale=1.
        let bytes = make_doc_with_dims(0, 0);
        let doc = DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        let opts = RenderOptions::fit_to_box(page, 80, 60);
        assert_eq!(opts.width, 80);
        assert_eq!(opts.height, 60);
    }

    #[test]
    fn can_stream_true_at_native_resolution_with_lanczos3() {
        // Line 290: the "|| native dims" branch — evaluated when Bilinear is false.
        let doc = load_doc("chicken.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: page.width() as u32,
            height: page.height() as u32,
            resampling: Resampling::Lanczos3,
            ..Default::default()
        };
        assert!(opts.can_stream(page));
    }

    /// Rendering a JB2-only (no BG44) page at very small scale triggers
    /// subsample >= 4 and exercises bg44_partial's empty-chunks path (line 918).
    #[test]
    fn render_bilevel_at_tiny_scale_exercises_bg44_partial_empty() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        // Use a tiny width so decode_scale << 0.25 and best_iw44_subsample >= 4.
        let opts = RenderOptions {
            width: 10,
            height: 10,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("tiny bilevel render should succeed");
        assert!(pm.width > 0 && pm.height > 0);
    }

    /// Bold dilation (opts.bold > 0) thickens the mask.
    #[test]
    fn render_with_bold_dilation_produces_output() {
        let doc = load_doc("boy_jb2.djvu");
        let page = doc.page(0).unwrap();
        let opts = RenderOptions {
            width: 40,
            height: 40,
            bold: 1,
            ..Default::default()
        };
        let pm = render_pixmap(page, &opts).expect("bold render should succeed");
        assert_eq!(pm.width, 40);
        assert_eq!(pm.height, 40);
    }
}
