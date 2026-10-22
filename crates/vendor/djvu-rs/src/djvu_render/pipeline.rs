//! The render pipeline: one composite per request.

use super::*;

// ── Render pipeline ──────────────────────────────────────────────────────────

/// How much of a page a render decodes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Detail {
    /// Every layer. The background follows [`full_detail_chunks`]: all BG44
    /// chunks, or only the first at subsample 4 and above.
    Full,
    /// Every layer, with the background from BG44 chunks `0..n`: frame `n - 1`
    /// of a progressive render.
    Chunks(usize),
    /// The background from its first BG44 chunk only, with no foreground.
    Coarse,
}

/// A page's layers decoded for one render canvas.
///
/// This is the one place a render turns [`RenderOptions`] into what the
/// compositor reads: the gamma table, the IW44 subsample, the layers at the
/// requested [`Detail`], and the mask plane. Every entry point builds one,
/// composites a window of the canvas, and hands the pixels to its own output
/// (a caller's buffer, a row sink, a pixmap, or cached tiles). Whole-page
/// renders finish through [`Composite::page_pixmap`].
///
/// The canvas is the full render in native orientation, `opts.width ×
/// opts.height` (at least 1 × 1); a window is any rectangle of it, and pixels
/// of a window outside the canvas are left as they are.
pub(super) struct Composite<'p> {
    pub(super) page: &'p DjVuPage,
    pub(super) canvas: RenderOptions,
    pub(super) detail: Detail,
    pub(super) bg: Background,
    pub(super) fg_palette: Option<FgbzPalette>,
    pub(super) mask: Option<Arc<crate::bitmap::Bitmap>>,
    pub(super) mask_shift: u32,
    pub(super) blit_map: Option<Arc<Vec<i32>>>,
    pub(super) fg44: Option<Arc<Pixmap>>,
    pub(super) gamma_lut: [u8; 256],
}

impl<'p> Composite<'p> {
    /// Decode `page` at `detail` for a render at `opts`.
    ///
    /// # Errors
    ///
    /// Propagates IW44 / JB2 decode errors (strict renders only).
    pub(super) fn decode(
        page: &'p DjVuPage,
        opts: &RenderOptions,
        detail: Detail,
    ) -> Result<Self, RenderError> {
        let canvas = RenderOptions {
            width: opts.width.max(1),
            height: opts.height.max(1),
            ..*opts
        };
        let bg_subsample = best_iw44_subsample(opts.decode_scale(page));
        let DecodedLayers {
            bg,
            fg_palette,
            mask,
            blit_map,
            fg44,
        } = match detail {
            Detail::Full => decode_layers(page, opts, bg_subsample, usize::MAX)?,
            Detail::Chunks(n) => decode_layers(page, opts, bg_subsample, n)?,
            Detail::Coarse => DecodedLayers {
                bg: decode_background_chunks(page, 1, bg_subsample)?,
                fg_palette: None,
                mask: None,
                blit_map: None,
                fg44: None,
            },
        };
        // Only a full-detail composite reads the 1/4-res mask: the
        // progressive frames always used the full-resolution one, and a
        // region is a crop of the full render, so it makes the same choice
        // (#691).
        let (mask, mask_shift) = if detail == Detail::Full {
            sub4_mask(page, bg_subsample, opts, mask, fg_palette.as_ref())
        } else {
            (mask, 0)
        };
        Ok(Self {
            page,
            canvas,
            detail,
            bg,
            fg_palette,
            mask,
            mask_shift,
            blit_map,
            fg44,
            gamma_lut: build_gamma_lut(page.gamma()),
        })
    }

    /// The whole canvas as a window.
    pub(super) fn whole(&self) -> RenderRect {
        RenderRect {
            x: 0,
            y: 0,
            width: self.canvas.width,
            height: self.canvas.height,
        }
    }

    /// `true` when the page has any background layer.
    pub(super) fn has_background(&self) -> bool {
        self.bg.is_some()
    }

    /// Run `f` once per background band of `window` (see [`for_each_bg_band`]).
    pub(super) fn bands<F>(&self, window: RenderRect, f: F) -> Result<(), RenderError>
    where
        F: FnMut(&CompositeContext<'_>, u32) -> Result<(), RenderError>,
    {
        for_each_bg_band(
            self.page,
            &self.canvas,
            &self.bg,
            self.mask.as_deref(),
            self.mask_shift,
            self.fg_palette.as_ref(),
            self.blit_map.as_deref().map(Vec::as_slice),
            self.fg44.as_deref(),
            &self.gamma_lut,
            (window.x, window.y),
            (window.width, window.height),
            f,
        )
    }

    /// Composite `window` into `buf`: RGBA rows `window.width` pixels wide.
    pub(super) fn write(&self, window: RenderRect, buf: &mut [u8]) -> Result<(), RenderError> {
        self.bands(window, |ctx, oy0| {
            composite_into(ctx, band_rows_mut(buf, window.width, oy0, ctx.out_h))
        })
    }

    /// Composite `window` into a new pixmap, white where it leaves the canvas.
    pub(super) fn pixmap(&self, window: RenderRect) -> Result<Pixmap, RenderError> {
        let mut pm = Pixmap::white(window.width, window.height);
        self.write(window, &mut pm.data)?;
        Ok(pm)
    }

    /// Composite `window` row by row, top to bottom.
    pub(super) fn rows<F>(&self, window: RenderRect, mut sink: F) -> Result<(), RenderError>
    where
        F: FnMut(usize, &[u8]),
    {
        self.bands(window, |ctx, oy0| {
            composite_rows(ctx, |y, row| sink(y + oy0 as usize, row))
        })
    }

    /// The context of the whole canvas over a whole (or missing) background;
    /// the tile cache copies it per tile.
    #[cfg(feature = "std")]
    pub(super) fn context(&self) -> CompositeContext<'_> {
        CompositeContext::from_layers(
            self.page,
            &self.canvas,
            self.bg.whole().map(PlaneView::whole),
            self.mask.as_deref(),
            self.mask_shift,
            self.fg_palette.as_ref(),
            self.blit_map.as_deref().map(Vec::as_slice),
            self.fg44.as_deref(),
            &self.gamma_lut,
            (0, 0),
            (self.canvas.width, self.canvas.height),
        )
    }

    /// The wavelet image of a banded background (#811), which the tile cache
    /// fetches one tile row at a time.
    #[cfg(feature = "std")]
    pub(super) fn banded_background(&self) -> Option<&Arc<Iw44Image>> {
        match &self.bg {
            Background::Banded { image, .. } => Some(image),
            _ => None,
        }
    }

    /// The whole page: the canvas, then the whole-pixmap steps in their one
    /// order — the anti-aliasing halving, the Lanczos-3 rescale (which
    /// replaces the canvas), and the combined INFO + user rotation.
    pub(super) fn page_pixmap(
        &self,
        limits: Option<crate::resource_limits::ResourceLimits>,
    ) -> Result<Pixmap, RenderError> {
        let pm = match self.lanczos_canvas(limits)? {
            Some(pm) => pm,
            None if aa_halves(self.page, &self.canvas) => aa_downscale(&self.pixmap(self.whole())?),
            None => self.pixmap(self.whole())?,
        };
        Ok(rotate_pixmap(pm, self.canvas.output_rotation(self.page)))
    }

    /// `window` of the page before rotation, white where it leaves the page:
    /// the exact crop of [`page_pixmap`](Self::page_pixmap) before its
    /// rotation. Cut from the Lanczos-3 canvas, or averaged from the doubled
    /// window of the canvas under anti-aliasing, or composited directly.
    pub(super) fn region(&self, window: RenderRect) -> Result<Pixmap, RenderError> {
        let full = (self.canvas.width, self.canvas.height);
        if lanczos_rescales(self.page, self.canvas.resampling, full) {
            // Only the part of `window` on the page goes through the filter.
            let x1 = window.x.saturating_add(window.width).min(full.0);
            let y1 = window.y.saturating_add(window.height).min(full.1);
            if x1 <= window.x || y1 <= window.y {
                return Ok(Pixmap::white(window.width, window.height));
            }
            let on_page = RenderRect {
                x: window.x,
                y: window.y,
                width: x1 - window.x,
                height: y1 - window.y,
            };
            if let Some(pm) = self.lanczos_window(None, on_page)? {
                return Ok(white_crop(&pm, (window.x, window.y), window));
            }
        }
        if !aa_halves(self.page, &self.canvas) {
            return self.pixmap(window);
        }
        // Pixel (x, y) of the halved page averages canvas pixels 2x..=2x+1,
        // 2y..=2y+1, so the part of `window` on the page needs only the
        // doubled window of the canvas.
        let (w, h) = (
            (self.canvas.width / 2).max(1),
            (self.canvas.height / 2).max(1),
        );
        let x1 = window.x.saturating_add(window.width).min(w);
        let y1 = window.y.saturating_add(window.height).min(h);
        if x1 <= window.x || y1 <= window.y {
            return Ok(Pixmap::white(window.width, window.height));
        }
        let (x, y) = (window.x * 2, window.y * 2);
        let doubled = RenderRect {
            x,
            y,
            width: ((x1 - window.x) * 2).min(self.canvas.width - x),
            height: ((y1 - window.y) * 2).min(self.canvas.height - y),
        };
        let halved = aa_downscale(&self.pixmap(doubled)?);
        Ok(white_crop(&halved, (window.x, window.y), window))
    }

    /// The Lanczos-3 canvas: when the options ask for Lanczos-3 at a size
    /// other than the native one, the page composited at its native size at
    /// the same [`Detail`] and rescaled to the canvas; otherwise `None`.
    ///
    /// The native composite is unrotated and not anti-aliased, like every
    /// canvas. It stays `None` when that composite fails (over the output
    /// limit, or a decode error), so the caller keeps the bilinear canvas at
    /// the same size: [`aa_halves`] is false either way.
    pub(super) fn lanczos_canvas(
        &self,
        limits: Option<crate::resource_limits::ResourceLimits>,
    ) -> Result<Option<Pixmap>, RenderError> {
        self.lanczos_window(limits, self.whole())
    }

    /// `window` of the [`lanczos_canvas`](Self::lanczos_canvas), byte for
    /// byte, at the cost of the window: only the part of the native page the
    /// filter reads for `window` is composited and rescaled. `window` must be
    /// non-empty and inside the canvas.
    ///
    /// `None` under the same rule as the whole canvas: no Lanczos-3 rescale,
    /// or a native page over the output limit or failing to composite.
    pub(super) fn lanczos_window(
        &self,
        limits: Option<crate::resource_limits::ResourceLimits>,
        window: RenderRect,
    ) -> Result<Option<Pixmap>, RenderError> {
        let full = (self.canvas.width, self.canvas.height);
        if !lanczos_rescales(self.page, self.canvas.resampling, full) {
            return Ok(None);
        }
        let native_opts = native_render_opts(self.page, &self.canvas);
        let native_full = (native_opts.width, native_opts.height);
        let (x, y, width, height) = crate::pixmap::lanczos3_source_window(
            native_full,
            full,
            (window.x, window.y, window.width, window.height),
        );
        let source = RenderRect {
            x,
            y,
            width,
            height,
        };
        let native = check_output_pixels(
            "render_native",
            self.page,
            limits,
            native_opts.width,
            native_opts.height,
        )
        .and_then(|()| Composite::decode(self.page, &native_opts, self.detail))
        .and_then(|native| native.pixmap(source));
        match native {
            // The scaler refuses an output above `Pixmap::MAX_PIXELS`; that is
            // a render-output limit, reported as one rather than as a blank
            // page.
            Ok(native) => Ok(Some(crate::pixmap::scale_lanczos3_window(
                &native,
                (x, y),
                native_full,
                full,
                (window.x, window.y, window.width, window.height),
            )?)),
            Err(_) => Ok(None),
        }
    }
}
