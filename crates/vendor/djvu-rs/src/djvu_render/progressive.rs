//! Coarse and progressive rendering.

use super::*;

/// Coarse render: decode only the first BG44 chunk for a fast blurry preview.
///
/// The background alone is composited, then finished like [`render_pixmap`]:
/// anti-aliasing, Lanczos-3 at a scaled size, and rotation all apply.
///
/// Returns `Ok(None)` when the page has no BG44 chunks.
#[deprecated(
    note = "use `RenderRequest::new(opts).quality(Quality::Coarse).pixmap(page)`; a page without a background gives `RenderError::NoBackground`"
)]
pub fn render_coarse(page: &DjVuPage, opts: &RenderOptions) -> Result<Option<Pixmap>, RenderError> {
    match RenderRequest::new(opts.clone())
        .quality(Quality::Coarse)
        .operation("render_coarse")
        .pixmap(page)
    {
        Err(RenderError::NoBackground) => Ok(None),
        result => result.map(Some),
    }
}

/// Progressive render: decode BG44 chunks 1..=chunk_n and all other layers.
///
/// `chunk_n = 0` decodes the first chunk only, like [`render_coarse`], but with
/// the foreground. Each additional chunk adds detail. The result after all
/// chunks equals [`render_pixmap`], with one exception: at scales of about a quarter of the native size and below (IW44 subsample 4
/// or more), [`render_pixmap`] decodes only the first background chunk and
/// reads a quarter-resolution mask, so the two differ slightly there.
///
/// The whole-pixmap steps — anti-aliasing, Lanczos-3 at a scaled size, and
/// rotation — apply to every frame as they do in [`render_pixmap`].
///
/// # Errors
///
/// Returns [`RenderError::ChunkOutOfRange`] if `chunk_n` exceeds the number
/// of available BG44 chunks.
#[deprecated(note = "use `RenderRequest::new(opts).quality(Quality::Step(n)).pixmap(page)`")]
pub fn render_progressive(
    page: &DjVuPage,
    opts: &RenderOptions,
    chunk_n: usize,
) -> Result<Pixmap, RenderError> {
    check_output_pixels("render_progressive", page, None, opts.width, opts.height)?;

    let n_bg44 = page.bg44_chunks().len();
    let max_chunk = n_bg44.saturating_sub(1);

    if n_bg44 > 0 && chunk_n > max_chunk {
        return Err(RenderError::ChunkOutOfRange {
            chunk_n,
            max: max_chunk,
        });
    }

    Composite::decode(page, opts, Detail::Chunks(chunk_n + 1))?.page_pixmap(None)
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

/// Progressive frame `step`; a page without BG44 chunks has one full frame,
/// whatever `step` asks for.
pub(super) fn progressive_frame(
    page: &DjVuPage,
    opts: &RenderOptions,
    step: usize,
) -> Result<Pixmap, RenderError> {
    let step = if page.bg44_chunks().is_empty() {
        0
    } else {
        step
    };
    RenderRequest::new(opts.clone())
        .quality(Quality::Step(step))
        .operation("render_progressive")
        .pixmap(page)
}

/// Render progressive frame `step` (`0..`[`progressive_steps`]).
///
/// Encapsulates the "no BG44 chunks ⇒ a single full [`render_pixmap`], otherwise
/// [`render_progressive`]`(step)`" decision that every progressive caller (the
/// `Page::render_scaled_progressive` collector and the async
/// `render_progressive_stream`) previously open-coded. `step` is interpreted as
/// the BG44 chunk index on multi-chunk pages.
#[deprecated(note = "use `RenderRequest::new(opts).quality(Quality::Step(step)).pixmap(page)`")]
pub fn render_progressive_step(
    page: &DjVuPage,
    opts: &RenderOptions,
    step: usize,
) -> Result<Pixmap, RenderError> {
    progressive_frame(page, opts, step)
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
    /// The foreground layers and the canvas; the background is replaced by
    /// each frame's snapshot of `img`.
    pub(super) composite: Composite<'a>,
    pub(super) bg_subsample: u32,
    /// Shared so a very large page can hand bands of it to the compositor
    /// (#811); nobody else holds it between frames.
    pub(super) img: Arc<Iw44Image>,
    pub(super) chunks_fed: usize,
}

impl<'a> ProgressiveDecoder<'a> {
    /// Build a streaming decoder for `page` at `opts`. Decodes the foreground
    /// once (including any `bold` dilation) so only the background refines per
    /// chunk.
    ///
    /// Errors: [`RenderError::InvalidDimensions`] if `opts.width`/`height` is 0;
    /// [`RenderError::UnsupportedOption`] if `opts.resampling` is not `Bilinear` or
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

        let ForegroundLayers {
            fg_palette,
            mask,
            blit_map,
            fg44,
        } = decode_foreground_strict(page)?;
        let mask = if opts.bold > 0 {
            mask.map(|m| Arc::new(Arc::unwrap_or_clone(m).dilate_n(opts.bold as u32)))
        } else {
            mask
        };

        Ok(Self {
            composite: Composite {
                page,
                canvas: opts.clone(),
                detail: Detail::Chunks(0),
                bg: Background::None,
                fg_palette,
                mask,
                mask_shift: 0,
                blit_map,
                fg44,
                gamma_lut: build_gamma_lut(page.gamma()),
            },
            bg_subsample: best_iw44_subsample(opts.decode_scale(page)),
            img: Arc::new(Iw44Image::new()),
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
        // Never shared between frames, so this is the in-place path.
        Arc::make_mut(&mut self.img)
            .decode_chunk(chunk)
            .map_err(RenderError::Iw44)?;
        self.chunks_fed += 1;
        self.composite.detail = Detail::Chunks(self.chunks_fed);
        self.composite.bg = Background::from_shared_iw44(&self.img, self.bg_subsample)?;
        let frame = self.composite.page_pixmap(None);
        // Release the frame's background so the next chunk decodes in place.
        self.composite.bg = Background::None;
        frame
    }

    /// Number of chunks fed so far (= number of frames produced).
    pub fn frames_produced(&self) -> usize {
        self.chunks_fed
    }
}

/// Eagerly render every progressive frame into a `Vec`, coarsest first.
///
/// The convenience form of [`render_progressive_step`] over the full
/// [`progressive_steps`] range. The last frame equals [`render_pixmap`], except
/// at scales of about a quarter of the native size and below (IW44 subsample 4
/// or more), [`render_pixmap`] decodes only the first background chunk and
/// reads a quarter-resolution mask, so the two differ slightly there.
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
        frames.push(progressive_frame(page, opts, step)?);
    }
    Ok(frames)
}
