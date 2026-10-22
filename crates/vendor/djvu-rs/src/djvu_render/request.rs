//! The public render API: requests, cancellation, regions and tiles.

use super::*;

// ── Public API ────────────────────────────────────────────────────────────────

/// A cooperative stop signal shared between a render and its caller.
///
/// Clones share one flag: cancel any clone and every render holding a clone
/// stops at its next checkpoint with [`RenderError::Cancelled`] (or
/// `TileError::Cancelled` on the tile API). Checkpoints sit *between* units of
/// work — on entry, between the layer decode and the composite, and before
/// each internal cache tile — so an in-flight decode always runs to
/// completion; cancellation bounds further work, not the current unit. A
/// token is one-way: once cancelled it stays cancelled (create a fresh token
/// per request generation instead of resetting).
///
/// Cancellation never changes rendered bytes and never corrupts caches: work
/// either completes a unit fully or abandons it without publishing anything
/// partial.
#[derive(Debug, Clone, Default)]
pub struct CancelToken {
    pub(super) flag: Arc<core::sync::atomic::AtomicBool>,
}

impl CancelToken {
    /// A fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Signal every holder of a clone of this token to stop.
    pub fn cancel(&self) {
        self.flag.store(true, core::sync::atomic::Ordering::Relaxed);
    }

    /// Whether [`cancel`](Self::cancel) has been called on any clone.
    pub fn is_cancelled(&self) -> bool {
        self.flag.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// The raw flag the render internals poll.
    #[cfg(feature = "std")]
    pub(crate) fn as_flag(&self) -> &core::sync::atomic::AtomicBool {
        &self.flag
    }
}

/// How much of the page a [`RenderRequest`] decodes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Quality {
    /// Every layer at full detail, as [`render_pixmap`] renders it.
    #[default]
    Full,
    /// Progressive frame `k`, `0..`[`progressive_steps`]`(page)`: the
    /// background from BG44 chunks `0..=k` and every foreground layer, as
    /// [`render_progressive`] renders it. On a page without BG44 chunks the
    /// single frame `0` is the full render.
    Step(usize),
    /// The background from its first BG44 chunk only, with no foreground: a
    /// fast blurry preview, as [`render_coarse`] renders it. A page without
    /// a background has no coarse render: [`RenderError::NoBackground`].
    Coarse,
}

/// One render of one page: what to render, and how.
///
/// This is the one front door to the renderer. It gathers every choice the
/// `render_*` functions spread over their names and arguments:
///
/// - the page size and look — [`RenderOptions`];
/// - an optional **region**, a rectangle of the output page;
/// - the **quality** — full, a progressive step, or a coarse preview;
/// - per-render **resource limits**;
/// - a **cancel token**;
/// - whether a region may use the page's composited-tile **cache**.
///
/// Then one method picks the output: a new [`Pixmap`]
/// ([`pixmap`](Self::pixmap)), a caller's RGBA buffer
/// ([`write_rgba`](Self::write_rgba)), or a row sink
/// ([`rows`](Self::rows)). Every combination gives the same bytes as the
/// matching crop of the full [`pixmap`](Self::pixmap).
///
/// # Example
///
/// ```no_run
/// use djvu_rs::djvu_render::{Quality, RenderOptions, RenderRect, RenderRequest};
/// # let doc = djvu_rs::djvu_document::DjVuDocument::parse(&[]).unwrap();
/// # let page = doc.page(0).unwrap();
/// let opts = RenderOptions::fit_to_width(page, 1024);
/// // The whole page.
/// let full = RenderRequest::new(opts.clone()).pixmap(page).unwrap();
/// // A viewport of the same page, from the tile cache.
/// let viewport = RenderRect { x: 0, y: 200, width: 1024, height: 600 };
/// let part = RenderRequest::new(opts.clone())
///     .region(viewport)
///     .cached(true)
///     .pixmap(page)
///     .unwrap();
/// // A quick preview first.
/// let preview = RenderRequest::new(opts).quality(Quality::Step(0)).pixmap(page);
/// # let _ = (full, part, preview);
/// ```
#[derive(Debug, Clone)]
pub struct RenderRequest {
    pub(super) options: RenderOptions,
    pub(super) region: Option<RenderRect>,
    pub(super) quality: Quality,
    pub(super) limits: Option<crate::resource_limits::ResourceLimits>,
    pub(super) cancel: Option<CancelToken>,
    pub(super) cached: bool,
    /// The name resource-limit errors report.
    pub(super) operation: &'static str,
}

impl RenderRequest {
    /// A full-quality render of the whole page at `options`.
    pub fn new(options: RenderOptions) -> Self {
        Self {
            options,
            region: None,
            quality: Quality::Full,
            limits: None,
            cancel: None,
            cached: false,
            operation: "render",
        }
    }

    /// Render only `region` of the output page.
    ///
    /// The region is in display space: it addresses the page as
    /// [`pixmap`](Self::pixmap) returns it — after anti-aliasing halves it,
    /// after Lanczos-3 rescales it, and after the INFO and user rotations
    /// turn it. The output is `region.width × region.height`, the exact crop
    /// of the whole page, with white pixels outside the page.
    pub fn region(mut self, region: RenderRect) -> Self {
        self.region = Some(region);
        self
    }

    /// Decode the page at `quality` (default [`Quality::Full`]).
    pub fn quality(mut self, quality: Quality) -> Self {
        self.quality = quality;
        self
    }

    /// Override the resource limits for this render. `None` (the default)
    /// keeps the limits the page inherited from its document.
    pub fn limits(mut self, limits: Option<crate::resource_limits::ResourceLimits>) -> Self {
        self.limits = limits;
        self
    }

    /// Stop the render at its next checkpoint once `token` is cancelled; see
    /// [`CancelToken`].
    pub fn cancel(mut self, token: CancelToken) -> Self {
        self.cancel = Some(token);
        self
    }

    /// Let a full-quality region render assemble its pixels from the page's
    /// composited-tile cache, and add the tiles it composites to it (see
    /// [`render_region_tiled`]). Repeated and overlapping regions — a
    /// viewer's pans — then cost only their new tiles. The bytes are the
    /// same either way. Ignored without a region, and for progressive or
    /// coarse quality, whose pixels are never cached. Needs the `std`
    /// feature; without it the flag is ignored.
    pub fn cached(mut self, cached: bool) -> Self {
        self.cached = cached;
        self
    }

    /// Name the operation that resource-limit errors report.
    pub(crate) fn operation(mut self, operation: &'static str) -> Self {
        self.operation = operation;
        self
    }

    /// Render to a new pixmap: the whole page, or [`region`](Self::region)
    /// of it.
    ///
    /// # Errors
    ///
    /// - [`RenderError::InvalidDimensions`] for a zero-sized output;
    /// - [`RenderError::ResourceLimit`] when the output is over the limit;
    /// - [`RenderError::ChunkOutOfRange`] for a [`Quality::Step`] past the
    ///   last frame;
    /// - [`RenderError::NoBackground`] for a coarse render of a page without
    ///   a background;
    /// - [`RenderError::Cancelled`] when the cancel token stopped the render;
    /// - decode errors (strict renders only).
    pub fn pixmap(&self, page: &DjVuPage) -> Result<Pixmap, RenderError> {
        let (w, h) = self.output_size();
        check_output_pixels(self.operation, page, self.limits, w, h)?;
        let detail = self.detail(page)?;
        self.checkpoint()?;
        let opts = &self.options;
        let Some(region) = self.region else {
            let composite = self.decode(page, detail)?;
            return composite.page_pixmap(self.limits);
        };
        #[cfg(feature = "std")]
        if self.cached && detail == Detail::Full {
            let flag = self.cancel.as_ref().map(CancelToken::as_flag);
            return display_region(page, opts, region, |native| {
                render_region_tiled_cancellable(page, native, opts, flag)
            })?
            .ok_or(RenderError::Cancelled);
        }
        let composite = self.decode(page, detail)?;
        let rotation = opts.output_rotation(page);
        display_region(page, opts, region, |native| {
            Ok(Some(rotate_pixmap(composite.region(native)?, rotation)))
        })?
        .ok_or(RenderError::Cancelled)
    }

    /// Like [`pixmap`](Self::pixmap), with a [`RenderReport`] of the layers
    /// a permissive render skipped or recovered (#696).
    ///
    /// The pixmap is byte-identical to [`pixmap`](Self::pixmap). In strict
    /// mode the report is always clean (decode errors propagate instead of
    /// being recovered); in permissive mode it lists each background
    /// truncation, dropped mask, or skipped foreground/palette in the order
    /// the renderer took them.
    ///
    /// # Errors
    ///
    /// Same as [`pixmap`](Self::pixmap).
    #[cfg(feature = "std")]
    pub fn pixmap_with_report(
        &self,
        page: &DjVuPage,
    ) -> Result<(Pixmap, RenderReport), RenderError> {
        // Install a per-thread recovery sink; the guard clears it on every
        // exit path (including panics) so a plain render never records.
        struct SinkGuard;
        impl Drop for SinkGuard {
            fn drop(&mut self) {
                RECOVERY_SINK.with(|sink| *sink.borrow_mut() = None);
            }
        }
        RECOVERY_SINK.with(|sink| *sink.borrow_mut() = Some(Vec::new()));
        let _guard = SinkGuard;
        let pixmap = self.pixmap(page)?;
        let recoveries = RECOVERY_SINK.with(|sink| sink.borrow_mut().take().unwrap_or_default());
        Ok((pixmap, RenderReport { recoveries }))
    }

    /// Render into `buf`: RGBA rows of the output (the whole page, or
    /// [`region`](Self::region) of it), with no allocation for the pixels.
    ///
    /// The buffer receives pixels as they leave the compositor, so the
    /// whole-pixmap steps — anti-aliasing, Lanczos-3 at a scaled size, and
    /// rotation — are refused rather than skipped
    /// ([`RenderOptions::can_stream`] tells in advance). Without them the
    /// output page is the canvas, and a region must lie inside it.
    ///
    /// # Errors
    ///
    /// As [`pixmap`](Self::pixmap), plus:
    ///
    /// - [`RenderError::UnsupportedOption`] for a whole-pixmap option, or a
    ///   region that leaves the page;
    /// - [`RenderError::BufTooSmall`] when `buf` holds fewer than
    ///   `width × height × 4` bytes.
    pub fn write_rgba(&self, page: &DjVuPage, buf: &mut [u8]) -> Result<(), RenderError> {
        let (w, h) = self.output_size();
        check_output_pixels(self.operation, page, self.limits, w, h)?;
        let window = self.stream_window(page)?;
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
        let detail = self.detail(page)?;
        self.checkpoint()?;
        self.decode(page, detail)?.write(window, buf)
    }

    /// Render row by row: `sink(y, rgba_row)` receives each output row, top
    /// to bottom, `width × 4` bytes long.
    ///
    /// This is the constant-memory path for low-memory targets and for
    /// streaming encoders: one scratch row, plus the decoded layers. A page
    /// whose full-resolution background would not fit the `djvu-iw44` band
    /// budget (128 MiB) is composited from bands of the wavelet image, one
    /// band in memory at a time (#811). The rows are those of
    /// [`pixmap`](Self::pixmap).
    ///
    /// # Errors
    ///
    /// As [`write_rgba`](Self::write_rgba), without the buffer check.
    pub fn rows<F>(&self, page: &DjVuPage, sink: F) -> Result<(), RenderError>
    where
        F: FnMut(usize, &[u8]),
    {
        let (w, h) = self.output_size();
        check_output_pixels(self.operation, page, self.limits, w, h)?;
        let window = self.stream_window(page)?;
        let detail = self.detail(page)?;
        self.checkpoint()?;
        self.decode(page, detail)?.rows(window, sink)
    }

    /// The output size: the region, or the whole canvas.
    pub(super) fn output_size(&self) -> (u32, u32) {
        match self.region {
            Some(r) => (r.width, r.height),
            None => (self.options.width, self.options.height),
        }
    }

    /// The canvas window a streaming output composites, or why it cannot.
    pub(super) fn stream_window(&self, page: &DjVuPage) -> Result<RenderRect, RenderError> {
        if let Some(reason) = self.options.whole_pixmap_reason(page) {
            return Err(RenderError::UnsupportedOption(reason));
        }
        let (cw, ch) = (self.options.width, self.options.height);
        let window = self.region.unwrap_or(RenderRect {
            x: 0,
            y: 0,
            width: cw,
            height: ch,
        });
        let inside = window
            .x
            .checked_add(window.width)
            .is_some_and(|x1| x1 <= cw)
            && window
                .y
                .checked_add(window.height)
                .is_some_and(|y1| y1 <= ch);
        if !inside {
            return Err(RenderError::UnsupportedOption(
                "a streamed region must lie inside the page; use pixmap",
            ));
        }
        Ok(window)
    }

    /// The layer detail [`Self::quality`] asks for on `page`.
    pub(super) fn detail(&self, page: &DjVuPage) -> Result<Detail, RenderError> {
        Ok(match self.quality {
            Quality::Full => Detail::Full,
            Quality::Coarse => Detail::Coarse,
            Quality::Step(step) => {
                let steps = progressive_steps(page);
                if step >= steps {
                    return Err(RenderError::ChunkOutOfRange {
                        chunk_n: step,
                        max: steps - 1,
                    });
                }
                if page.bg44_chunks().is_empty() {
                    // No refinement ladder: the single step is the full render.
                    Detail::Full
                } else {
                    Detail::Chunks(step + 1)
                }
            }
        })
    }

    /// Decode the layers at `detail`, then pass the post-decode checkpoint.
    pub(super) fn decode<'p>(
        &self,
        page: &'p DjVuPage,
        detail: Detail,
    ) -> Result<Composite<'p>, RenderError> {
        let composite = Composite::decode(page, &self.options, detail)?;
        if detail == Detail::Coarse && !composite.has_background() {
            return Err(RenderError::NoBackground);
        }
        self.checkpoint()?;
        Ok(composite)
    }

    /// [`RenderError::Cancelled`] once the cancel token is cancelled.
    pub(super) fn checkpoint(&self) -> Result<(), RenderError> {
        if self.cancel.as_ref().is_some_and(CancelToken::is_cancelled) {
            Err(RenderError::Cancelled)
        } else {
            Ok(())
        }
    }
}

/// Render the display-space `region` of the page with `render_native`, which
/// renders a native (pre-rotation) rectangle of the unrotated output page and
/// returns it turned by the output rotation, or `None` when cancelled.
///
/// Only the part of `region` inside the display page reaches `render_native`;
/// the rest is white, as in [`render_region`].
pub(super) fn display_region(
    page: &DjVuPage,
    opts: &RenderOptions,
    region: RenderRect,
    render_native: impl FnOnce(RenderRect) -> Result<Option<Pixmap>, RenderError>,
) -> Result<Option<Pixmap>, RenderError> {
    use crate::info::Rotation;
    let rotation = opts.output_rotation(page);
    let native = unrotated_size(page, opts);
    let (dw, dh) = match rotation {
        Rotation::Cw90 | Rotation::Ccw90 => (native.1, native.0),
        Rotation::None | Rotation::Rot180 => native,
    };
    let x1 = region.x.saturating_add(region.width).min(dw);
    let y1 = region.y.saturating_add(region.height).min(dh);
    if x1 <= region.x || y1 <= region.y {
        return Ok(Some(Pixmap::white(region.width, region.height)));
    }
    let inside = RenderRect {
        x: region.x,
        y: region.y,
        width: x1 - region.x,
        height: y1 - region.y,
    };
    let Some(pm) = render_native(native_rect(rotation, native, inside))? else {
        return Ok(None);
    };
    if inside == region {
        return Ok(Some(pm));
    }
    Ok(Some(white_crop(&pm, (region.x, region.y), region)))
}

/// Render a `DjVuPage` into a pre-allocated RGBA buffer.
///
/// This is the zero-allocation render path when `buf` is reused across calls
/// with the same dimensions. The buffer must be at least `width * height * 4`
/// bytes.
///
/// The buffer holds the composited page as it leaves the compositor, so the
/// whole-pixmap steps of [`render_pixmap`] — anti-aliasing, Lanczos-3 at a
/// scaled size, and rotation — are refused rather than skipped, exactly as in
/// [`render_streaming`]. [`RenderOptions::can_stream`] answers the question
/// without rendering.
///
/// # Errors
///
/// - [`RenderError::BufTooSmall`] if `buf.len() < width * height * 4`
/// - [`RenderError::InvalidDimensions`] if `width == 0 || height == 0`
/// - [`RenderError::UnsupportedOption`] if a whole-pixmap option is set
/// - Propagates IW44 / JB2 decode errors.
#[deprecated(note = "use `RenderRequest::new(opts).write_rgba(page, buf)`")]
pub fn render_into(
    page: &DjVuPage,
    opts: &RenderOptions,
    buf: &mut [u8],
) -> Result<(), RenderError> {
    RenderRequest::new(opts.clone())
        .operation("render_into")
        .write_rgba(page, buf)
}

/// Like [`render_into`], with an optional caller-supplied resource limit override.
#[deprecated(note = "use `RenderRequest::new(opts).limits(limits).write_rgba(page, buf)`")]
pub fn render_into_with_limits(
    page: &DjVuPage,
    opts: &RenderOptions,
    limits: Option<crate::resource_limits::ResourceLimits>,
    buf: &mut [u8],
) -> Result<(), RenderError> {
    RenderRequest::new(opts.clone())
        .limits(limits)
        .operation("render_into")
        .write_rgba(page, buf)
}

/// Output rows `oy0..oy0 + rows` of an RGBA buffer `w` pixels wide.
#[inline]
pub(super) fn band_rows_mut(buf: &mut [u8], w: u32, oy0: u32, rows: u32) -> &mut [u8] {
    let stride = w as usize * 4;
    &mut buf[oy0 as usize * stride..(oy0 as usize + rows as usize) * stride]
}

/// Build the options for the native-resolution canvas that feeds the
/// Lanczos-3 post-filter: full page size, no scaling, no AA, no rotation, and
/// bilinear resampling.
///
/// Bold and permissive flags are carried through so the high-resolution
/// re-render matches the requested render in everything but the final
/// resampling step.
pub(super) fn native_render_opts(page: &DjVuPage, opts: &RenderOptions) -> RenderOptions {
    // Full page size (decode scale derives to 1.0), bilinear resampling, no AA /
    // no rotation. Bold and permissive are carried through.
    RenderOptions {
        width: page.width() as u32,
        height: page.height() as u32,
        bold: opts.bold,
        permissive: opts.permissive,
        ..Default::default()
    }
}

/// Whether `resampling` runs the Lanczos-3 post-pass for a `full`-sized render
/// of `page`: Lanczos-3 at any size other than the native one.
///
/// At the native size the post-pass is the identity, so such a render streams,
/// tiles, and caches like a bilinear one.
pub(super) fn lanczos_rescales(page: &DjVuPage, resampling: Resampling, full: (u32, u32)) -> bool {
    resampling == Resampling::Lanczos3
        && (page.width() as u32 != full.0 || page.height() as u32 != full.1)
}

/// How far, in output pixels, a Lanczos-3 rescale of `native` pixels to
/// `out` pixels spreads a change of one native pixel: the kernel's 3-pixel
/// half-width at the coarser of the two scales, plus one for rounding.
#[cfg(feature = "std")]
pub(super) fn lanczos_reach(out: u32, native: u32) -> u64 {
    (3 * u64::from(out))
        .div_ceil(u64::from(native.max(1)))
        .max(3)
        + 1
}

/// Whether a render of `page` with `opts` halves the canvas for
/// anti-aliasing: `opts.aa`, unless Lanczos-3 rescales the page, which
/// ignores `aa`.
///
/// The answer depends on the options alone, so the output size does too: a
/// Lanczos-3 render whose native composite fails falls back to the bilinear
/// canvas at the requested size, still without the halving.
pub(super) fn aa_halves(page: &DjVuPage, opts: &RenderOptions) -> bool {
    let full = (opts.width.max(1), opts.height.max(1));
    opts.aa && !lanczos_rescales(page, opts.resampling, full)
}

/// The size of the page [`render_pixmap`] returns for `opts`, before
/// rotation: the requested size, halved when [`aa_halves`].
pub(crate) fn unrotated_size(page: &DjVuPage, opts: &RenderOptions) -> (u32, u32) {
    let full = (opts.width.max(1), opts.height.max(1));
    if aa_halves(page, opts) {
        ((full.0 / 2).max(1), (full.1 / 2).max(1))
    } else {
        full
    }
}

/// Render a page and return the pixmap together with a [`RenderReport`] of any
/// layers a permissive render skipped or recovered (#696).
///
/// The pixmap is byte-identical to [`render_pixmap`]. In strict mode the report
/// is always clean (decode errors propagate instead of being recovered); in
/// permissive mode it lists each background truncation, dropped mask, or
/// skipped foreground/palette in the order the renderer took them.
#[cfg(feature = "std")]
#[deprecated(note = "use `RenderRequest::new(opts).pixmap_with_report(page)`")]
pub fn render_pixmap_with_report(
    page: &DjVuPage,
    opts: &RenderOptions,
) -> Result<(Pixmap, RenderReport), RenderError> {
    RenderRequest::new(opts.clone())
        .operation("render_pixmap")
        .pixmap_with_report(page)
}

/// Render a `DjVuPage` to a new [`Pixmap`] using the given options: the
/// whole page at full quality, the same as
/// [`RenderRequest::new`]`(opts).`[`pixmap`](RenderRequest::pixmap)`(page)`.
pub fn render_pixmap(page: &DjVuPage, opts: &RenderOptions) -> Result<Pixmap, RenderError> {
    RenderRequest::new(opts.clone())
        .operation("render_pixmap")
        .pixmap(page)
}

/// Render a page to an owned RGBA pixmap with an optional resource limit override.
///
/// When `limits` is `None`, limits inherited from the parent document at parse
/// time apply (see [`ParseOptions::limits`](crate::resource_limits::ParseOptions::limits)).
/// Per-render overrides use [`render_pixmap_with_limits`] /
/// [`render_into_with_limits`].
#[deprecated(note = "use `RenderRequest::new(opts).limits(limits).pixmap(page)`")]
pub fn render_pixmap_with_limits(
    page: &DjVuPage,
    opts: &RenderOptions,
    limits: Option<crate::resource_limits::ResourceLimits>,
) -> Result<Pixmap, RenderError> {
    RenderRequest::new(opts.clone())
        .limits(limits)
        .operation("render_pixmap")
        .pixmap(page)
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
/// background (BG44) and mask (JB2) buffers. For a page whose full-resolution
/// background would not fit the `djvu-iw44` band budget (128 MiB) the
/// background is never built whole: the rows are composited from bands of the
/// wavelet image, one band in memory at a time (#811), so the peak stays near
/// one band regardless of the page size.
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
/// - the page's INFO rotation combined with `opts.rotation` is the identity
///   (an upright page with no user rotation, or a user rotation that cancels
///   the INFO rotation)
///
/// [`RenderOptions::can_stream`] answers the same question without rendering.
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
#[deprecated(note = "use `RenderRequest::new(opts).rows(page, sink)`")]
pub fn render_streaming<F>(
    page: &DjVuPage,
    opts: &RenderOptions,
    sink: F,
) -> Result<(), RenderError>
where
    F: FnMut(usize, &[u8]),
{
    RenderRequest::new(opts.clone())
        .operation("render_streaming")
        .rows(page, sink)
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
/// The region is the exact crop of [`render_pixmap`] before its rotation,
/// then turned by the same rotation; pixels outside the page are white.
/// Every option applies: with `opts.aa` the region addresses the halved page
/// (`opts.width / 2 × opts.height / 2`), and Lanczos-3 at a scaled size
/// crops the rescaled page.
///
/// # Errors
///
/// - [`RenderError::InvalidDimensions`] if `region.width == 0 || region.height == 0`
/// - Propagates IW44 / JB2 decode errors.
#[deprecated(
    note = "use `RenderRequest::new(opts).region(r).pixmap(page)`; its region is in display (rotated) coordinates"
)]
pub fn render_region(
    page: &DjVuPage,
    region: RenderRect,
    opts: &RenderOptions,
) -> Result<Pixmap, RenderError> {
    check_output_pixels("render_region", page, None, region.width, region.height)?;
    let pm = Composite::decode(page, opts, Detail::Full)?.region(region)?;
    Ok(rotate_pixmap(pm, opts.output_rotation(page)))
}

/// `true` when a cooperative cancel flag is present and set.
///
/// `Relaxed` is enough: the flag carries no data, it only asks in-flight work
/// to stop at its next checkpoint.
#[cfg(feature = "std")]
#[inline]
pub(crate) fn is_cancelled(cancel: Option<&core::sync::atomic::AtomicBool>) -> bool {
    cancel.is_some_and(|flag| flag.load(core::sync::atomic::Ordering::Relaxed))
}

/// Render a sub-rectangle of a page, assembling the output from a per-page
/// cache of composited `TILE_SIZE`×`TILE_SIZE` output tiles.
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
/// Every request uses the cache. Tiles hold the unrotated output page, the
/// one [`render_region`] crops, so the options that derive it from a canvas
/// of another size are cached too:
///
/// - Anti-aliasing: a missing tile is averaged from the doubled window of
///   the canvas, exactly as [`render_region`] does.
/// - Lanczos-3 at a scaled size: the first miss rescales the whole page and,
///   when the page fits the tile-cache budget, caches all of its tiles, so
///   the rest of a grid hits instead of rescaling the page again.
///
/// - Rotation: tiles are cached in native orientation and the assembled
///   region is rotated once, exactly as [`render_region`] does.
/// - `opts.permissive`: strict and permissive requests share tiles. Layers
///   decode before any tile lookup, so a strict request on a damaged page
///   still fails there. On an intact page both modes decode identical
///   layers (the same background chunks too, see `full_detail_chunks`),
///   hence identical tiles, whichever mode fills the cache first.
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
#[deprecated(
    note = "use `RenderRequest::new(opts).region(r).cached(true).pixmap(page)`; its region is in display (rotated) coordinates"
)]
pub fn render_region_tiled(
    page: &DjVuPage,
    region: RenderRect,
    opts: &RenderOptions,
) -> Result<Pixmap, RenderError> {
    let pm = render_region_tiled_cancellable(page, region, opts, None)?;
    // Without a cancel flag the render can never be abandoned.
    Ok(pm.expect("uncancellable render completed"))
}

/// Map a display-space rectangle back to the native rectangle whose rotated
/// render equals it.
///
/// `native` is the pre-rotation canvas `(W, H)`; `r = (x, y, w, h)` lies
/// inside that canvas turned by `rotation`. The region renderers select their
/// sub-rectangle before rotating (`rotate_pixmap` runs last), so a display
/// rectangle is pulled back through the inverse rotation:
///
/// | combined rotation | native rect |
/// |---|---|
/// | `None`  | `(x, y, w, h)` |
/// | `Cw90`  | `(y, H − x − w, h, w)` |
/// | `Rot180`| `(W − x − w, H − y − h, w, h)` |
/// | `Ccw90` | `(W − y − h, x, h, w)` |
pub(crate) fn native_rect(
    rotation: crate::info::Rotation,
    native: (u32, u32),
    r: RenderRect,
) -> RenderRect {
    use crate::info::Rotation;
    let (fw, fh) = native;
    match rotation {
        Rotation::None => r,
        Rotation::Cw90 => RenderRect {
            x: r.y,
            y: fh - r.x - r.width,
            width: r.height,
            height: r.width,
        },
        Rotation::Rot180 => RenderRect {
            x: fw - r.x - r.width,
            y: fh - r.y - r.height,
            width: r.width,
            height: r.height,
        },
        Rotation::Ccw90 => RenderRect {
            x: fw - r.y - r.height,
            y: r.x,
            width: r.height,
            height: r.width,
        },
    }
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

    // Tiles cover the unrotated output page, the one `render_region` crops.
    let canvas = (opts.width.max(1), opts.height.max(1));
    let (full_w, full_h) = unrotated_size(page, opts);
    let aa = aa_halves(page, opts);
    let lanczos = lanczos_rescales(page, opts.resampling, canvas);

    if is_cancelled(cancel) {
        return Ok(None);
    }
    // Tiles are cached in native orientation; the assembled region turns
    // once at the end, as in `render_region`.
    let rotation = opts.output_rotation(page);

    let composite = Composite::decode(page, opts, Detail::Full)?;
    // The Lanczos-3 tiles of this region, filtered together on the first miss
    // and cut for every other miss of this call, with their origin.
    // `Some(None)`: the native composite failed, and tiles come from the
    // bilinear canvas, as in `Composite::region`.
    let mut lanczos_block: Option<Option<(Pixmap, (u32, u32))>> = None;
    // Template context for the whole full_w×full_h render; each tile below
    // copies it (cheap: `Copy`) and only overwrites offset/out fields.
    let ctx_template = composite.context();
    // #811: a banded background is fetched one tile row at a time, on the
    // first cache miss in that row, and dropped with the row.
    let banded = composite.banded_background();

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
        return Ok(Some(rotate_pixmap(pm, rotation)));
    }
    let tx0 = region.x / TILE_SIZE;
    let ty0 = region.y / TILE_SIZE;
    let tx1 = (region_x1 - 1) / TILE_SIZE;
    let ty1 = (region_y1 - 1) / TILE_SIZE;

    let layers = page.render_layers();
    for ty in ty0..=ty1 {
        let tile_y0 = ty * TILE_SIZE;
        let tile_h = TILE_SIZE.min(full_h - tile_y0);
        // The background band for this tile row: `(pixmap, first plane row)`.
        let mut row_band: Option<(Pixmap, u32)> = None;
        for tx in tx0..=tx1 {
            if is_cancelled(cancel) {
                return Ok(None);
            }
            let tile_x0 = tx * TILE_SIZE;
            let tile_w = TILE_SIZE.min(full_w - tile_x0);
            let key = TileKey {
                canvas,
                x: tile_x0,
                y: tile_y0,
                bold: opts.bold,
                mask_aa: opts.mask_aa,
                aa,
                lanczos,
            };
            let tile_rect = RenderRect {
                x: tile_x0,
                y: tile_y0,
                width: tile_w,
                height: tile_h,
            };

            let tile = match layers.get_tile(key) {
                Some(t) => t,
                None if lanczos => {
                    if lanczos_block.is_none() {
                        let (x, y) = (tx0 * TILE_SIZE, ty0 * TILE_SIZE);
                        let block = RenderRect {
                            x,
                            y,
                            width: (tx1 + 1).saturating_mul(TILE_SIZE).min(full_w) - x,
                            height: (ty1 + 1).saturating_mul(TILE_SIZE).min(full_h) - y,
                        };
                        lanczos_block = Some(
                            composite
                                .lanczos_window(None, block)?
                                .map(|pm| (pm, (x, y))),
                        );
                    }
                    let pm = match &lanczos_block {
                        Some(Some((block, origin))) => white_crop(block, *origin, tile_rect),
                        _ => composite.pixmap(tile_rect)?,
                    };
                    insert_pixmap_tile(layers, key, pm)
                }
                None if aa => insert_pixmap_tile(layers, key, composite.region(tile_rect)?),
                None => {
                    if let Some(image) = banded
                        && row_band.is_none()
                    {
                        let (lo, hi) = bg_rows_needed(
                            (page.width() as u32, page.height() as u32),
                            (full_w, full_h),
                            (image.width, image.height),
                            tile_y0..tile_y0 + tile_h,
                        );
                        // Only the columns of this region's tiles.
                        let cols = bg_cols_needed(
                            page.width() as u32,
                            full_w,
                            image.width,
                            tx0 * TILE_SIZE..(tx1 + 1).saturating_mul(TILE_SIZE).min(full_w),
                        );
                        row_band = Some((image.rgb_window(lo..hi, cols)?, lo));
                    }
                    let mut tile_ctx = match (banded, &row_band) {
                        (Some(image), Some((band, lo))) => {
                            ctx_template.with_bg(Some(PlaneView::band(band, image.height, *lo)))
                        }
                        _ => ctx_template,
                    };
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

    Ok(Some(rotate_pixmap(pm, rotation)))
}

/// Cache `pm` as the tile `key` and return the entry.
#[cfg(feature = "std")]
pub(super) fn insert_pixmap_tile(
    layers: &PageLayers,
    key: TileKey,
    pm: Pixmap,
) -> std::sync::Arc<TileEntry> {
    let entry = std::sync::Arc::new(TileEntry {
        w: pm.width,
        h: pm.height,
        data: pm.data,
    });
    layers.insert_tile(key, entry.clone());
    entry
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
            render_pixmap(page, &RenderSize::at_dpi(page, dpi as f32).options())
        })
        .collect()
}
