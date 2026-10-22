//! Render options, errors and the recovery report.

use super::*;

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

    /// A [`CancelToken`] stopped the render at a checkpoint.
    #[error("render cancelled")]
    Cancelled,

    /// A coarse render ([`Quality::Coarse`]) of a page without a background
    /// layer.
    #[error("page has no background layer for a coarse render")]
    NoBackground,
}

/// A refused output pixmap is a render-output limit.
///
/// [`Pixmap::try_new`] caps one pixmap at [`Pixmap::MAX_PIXELS`]; on the render
/// paths that ceiling belongs to the same axis as the configurable
/// `max_render_pixels`, so it surfaces as [`RenderError::ResourceLimit`]. A
/// `usize` overflow of `width * height` is an invalid size, not a limit.
impl From<crate::pixmap::PixmapError> for RenderError {
    fn from(e: crate::pixmap::PixmapError) -> Self {
        match e {
            crate::pixmap::PixmapError::Overflow { width, height } => {
                RenderError::InvalidDimensions { width, height }
            }
            crate::pixmap::PixmapError::TooLarge {
                width,
                height,
                pixels,
                max,
            } => RenderError::ResourceLimit(crate::resource_limits::ResourceLimitExceeded {
                operation: "render",
                axis: crate::resource_limits::ResourceLimitAxis::RenderOutputPixels,
                found: pixels as u64,
                limit: max as u64,
                page_number: None,
                width: Some(width),
                height: Some(height),
            }),
            // `PixmapError` is `#[non_exhaustive]` in a sibling crate; a
            // variant this version does not know is still a refused size.
            #[allow(unreachable_patterns)]
            _ => RenderError::UnsupportedOption("output pixmap size refused"),
        }
    }
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
    /// Compositor width in pixels, in the page's native (pre-rotation)
    /// orientation. The returned pixmap is this size after the page's INFO
    /// rotation and [`rotation`](Self::rotation): a quarter turn swaps the
    /// sides. The `fit_to_*` constructors take display sizes and set this.
    pub width: u32,
    /// Compositor height in pixels, in the page's native orientation (see
    /// [`width`](Self::width)).
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
    /// Whether to apply anti-aliasing downscale pass: the page is composited
    /// at `width`×`height` and each 2×2 block is averaged, so the output is
    /// half the requested size.
    ///
    /// Ignored when [`Resampling::Lanczos3`] rescales the page (any size other
    /// than the native one): the Lanczos-3 filter already smooths the page,
    /// and the output keeps the requested size.
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

pub(super) fn effective_max_render_pixels(
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

pub(super) fn check_output_pixels(
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
    pub(super) static RECOVERY_SINK: core::cell::RefCell<Option<Vec<RenderRecovery>>> =
        const { core::cell::RefCell::new(None) };
}

/// Record a permissive recovery when a report is being collected.
#[cfg(feature = "std")]
pub(super) fn record_recovery(layer: RecoveredLayer, detail: impl Into<String>) {
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
pub(super) fn record_recovery(_layer: RecoveredLayer, _detail: &str) {}

/// Unwrap a permissive layer decode, recording a recovery on failure.
pub(super) fn permissive_layer<T>(
    result: Result<Option<T>, RenderError>,
    layer: RecoveredLayer,
) -> Option<T> {
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
    /// preserving aspect ratio. Respects page rotation from the INFO chunk:
    /// `width` is the width of the rendered (rotated) pixmap.
    pub fn fit_to_width(page: &crate::djvu_document::DjVuPage, width: u32) -> Self {
        let size = RenderSize::fit_width(page, width);
        let scale = size.display.0 as f32 / display_dimensions(page).0.max(1) as f32;
        RenderOptions {
            scale,
            ..size.options()
        }
    }

    /// Create render options that scale the page to fit the given height,
    /// preserving aspect ratio. Respects page rotation from the INFO chunk:
    /// `height` is the height of the rendered (rotated) pixmap.
    pub fn fit_to_height(page: &crate::djvu_document::DjVuPage, height: u32) -> Self {
        let size = RenderSize::fit_height(page, height);
        let scale = size.display.1 as f32 / display_dimensions(page).1.max(1) as f32;
        RenderOptions {
            scale,
            ..size.options()
        }
    }

    /// Create render options that scale the page to fit within a bounding box,
    /// preserving aspect ratio. Respects page rotation from the INFO chunk:
    /// the rendered (rotated) pixmap fits in `max_width × max_height`.
    pub fn fit_to_box(
        page: &crate::djvu_document::DjVuPage,
        max_width: u32,
        max_height: u32,
    ) -> Self {
        let size = RenderSize::fit_box(page, max_width, max_height);
        let (dw, dh) = display_dimensions(page);
        let scale = if dw == 0 || dh == 0 {
            1.0
        } else {
            (max_width as f64 / dw as f64).min(max_height as f64 / dh as f64) as f32
        };
        RenderOptions {
            scale,
            ..size.options()
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
    /// The rotation is the combined INFO + user rotation: a user rotation that
    /// cancels the page's INFO rotation streams too.
    ///
    /// This is the single source of truth for streaming eligibility; export
    /// paths call it instead of re-deriving the rule, and [`render_streaming`]
    /// refuses exactly the options it rejects.
    pub fn can_stream(&self, page: &crate::djvu_document::DjVuPage) -> bool {
        self.whole_pixmap_reason(page).is_none()
    }

    /// Why a render of `page` needs a whole [`Pixmap`] before its output is
    /// final, or `None` when rows come out of the compositor final.
    ///
    /// The three whole-image post-passes are the anti-aliasing halving,
    /// Lanczos-3 rescaling, and the combined rotation. The reason doubles as
    /// the [`render_streaming`] error message.
    pub(crate) fn whole_pixmap_reason(
        &self,
        page: &crate::djvu_document::DjVuPage,
    ) -> Option<&'static str> {
        if self.aa {
            Some("anti-aliasing requires a full pixmap; use render_pixmap")
        } else if lanczos_rescales(page, self.resampling, (self.width, self.height)) {
            Some("Lanczos-3 resampling at scaled output requires a full pixmap; use render_pixmap")
        } else if self.output_rotation(page) != crate::info::Rotation::None {
            Some("rotation requires a full pixmap; use render_pixmap")
        } else {
            None
        }
    }

    /// The combined INFO + user rotation applied after compositing.
    pub(crate) fn output_rotation(
        &self,
        page: &crate::djvu_document::DjVuPage,
    ) -> crate::info::Rotation {
        combine_rotations(page.rotation(), self.rotation)
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
