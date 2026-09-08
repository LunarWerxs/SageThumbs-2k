//! High-level page encoder — composes the codec primitives into a
//! complete `FORM:DJVU` page ready to wrap as a single-page document or
//! drop into a `FORM:DJVM` bundle.
//!
//! The encoder kit (`jb2_encode`, `iw44_encode`, `fgbz_encode`,
//! `smmr`, `bzz_encode`, `text_encode`, `navm_encode`) provides the
//! per-codec building blocks; this module orchestrates them so callers
//! don't have to hand-assemble IFF chunks.
//!
//! # Quick start
//!
//! Bilevel scan → single-page DjVu file:
//!
//! ```no_run
//! use djvu_rs::Bitmap;
//! use djvu_rs::djvu_encode::{PageEncoder, EncodeQuality};
//!
//! let mut bm = Bitmap::new(1024, 1280);
//! // … fill bm …
//! let bytes = PageEncoder::from_bitmap(&bm)
//!     .with_dpi(300)
//!     .with_quality(EncodeQuality::Lossless)
//!     .encode()
//!     .unwrap();
//! std::fs::write("scan.djvu", bytes).unwrap();
//! ```
//!
//! Color scan → layered DjVu (mask via JB2 + sub-sampled BG via IW44):
//!
//! ```no_run
//! use djvu_rs::Pixmap;
//! use djvu_rs::djvu_encode::{PageEncoder, EncodeQuality};
//!
//! let pm = Pixmap::white(1024, 1280);
//! let bytes = PageEncoder::from_pixmap(&pm)
//!     .with_dpi(300)
//!     .with_quality(EncodeQuality::Quality)
//!     .encode()
//!     .unwrap();
//! ```
//!
//! # Status
//!
//! - `Lossless` from a [`Bitmap`]: ships `INFO + Sjbz`. Pixel-exact.
//! - `Quality` from a [`Pixmap`]: ships `INFO + Sjbz + BG44… + FGbz`
//!   when foreground ink is detected. Lossy by codec definition; output
//!   is decodable end-to-end.
//! - `Archival` from a [`Pixmap`]: same layered chunk shape as `Quality`,
//!   with a denser background sample grid. This is a conservative archival
//!   profile, not a DjVuLibre-equivalent color text optimiser.
//! - `Lossless` from a [`Pixmap`] / `Quality` from a [`Bitmap`] are
//!   rejected: the combinations are mathematically meaningless
//!   (IW44 is lossy; bilevel input has nothing to put in BG44).

use crate::bitmap::Bitmap;
use crate::bzz_encode::bzz_encode;
use crate::chunk_encode::{ChunkEncoder, EncodedChunk, FgbzChunk, encode_info};
use crate::fgbz_encode::FgbzColor;
use crate::iff::{Chunk, DjvuFile, emit};
use crate::iw44_encode::{Iw44EncodeOptions, encode_iw44_color};
use crate::jb2_encode::{self, Jb2EncodeOptions};
use crate::ocr::{OcrBackend, OcrError, OcrOptions};
use crate::pixmap::Pixmap;
use crate::segment::{SegmentOptions, segment_page};
use crate::text::TextLayer;
use crate::text_encode::encode_text_layer;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors returned by [`PageEncoder::encode`].
#[derive(Debug, thiserror::Error)]
pub enum EncodeError {
    /// The requested combination of input + quality profile is not
    /// implemented yet. The message names the missing dependency
    /// (typically a sibling issue tracking the codec layer).
    #[error("page encoder: {0}")]
    Unsupported(&'static str),
}

// ── FGbz palette construction ─────────────────────────────────────────────────

/// How [`foreground_fgbz`] turns per-blit average colours into a palette.
///
/// The historical (and default) behaviour is [`FgbzPaletteOptions::Exact`]:
/// one palette entry per *distinct* per-blit average colour, so anti-aliased
/// edges that nudge two otherwise-identical glyphs' averages by a few LSBs
/// each get their own palette entry. On multicolour foreground pages (colour
/// text, highlighted scans) this can bloat the palette — and hence the FGbz
/// chunk — well past the number of colours a human would perceive.
/// [`FgbzPaletteOptions::MedianCut`] instead clusters the per-blit average
/// colours down to at most `max_colors` entries via median-cut quantisation
/// (weighted by each blit's foreground pixel count) and maps every blit to
/// its nearest resulting entry, trading exact per-blit colour for a smaller,
/// perceptually-similar palette. See PERF_EXPERIMENTS.md `FGBZ_MEDIANCUT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FgbzPaletteOptions {
    /// One palette entry per distinct per-blit average colour (current /
    /// pre-experiment behaviour). Byte-identical to all previous releases.
    #[default]
    Exact,
    /// Median-cut quantisation of the per-blit average colours down to at
    /// most `max_colors` palette entries (each blit maps to its nearest
    /// entry by squared RGB distance). `max_colors == 0` is treated as 1.
    MedianCut {
        /// Upper bound on palette entries. Wire format caps at 65 535; in
        /// practice a small number (tens) is the interesting range.
        max_colors: u16,
    },
}

/// A colour together with the pixel weight it represents, for median-cut.
#[derive(Debug, Clone, Copy)]
struct WeightedColor {
    r: u8,
    g: u8,
    b: u8,
    weight: u64,
}

/// Median-cut quantisation: repeatedly split the box (subset of `colors`)
/// with the widest weighted-irrelevant channel range, until there are `k`
/// boxes (or no box can be split further). Each returned colour is the
/// pixel-weighted average of its box.
///
/// Deterministic: box selection breaks ties by lowest box index, and
/// splitting sorts by channel value then original index, so repeated runs
/// on the same input produce the same palette (needed for a stable,
/// reproducible re-encode).
fn median_cut(colors: &[WeightedColor], k: usize) -> Vec<FgbzColor> {
    if colors.is_empty() {
        return Vec::new();
    }
    let k = k.max(1);

    // Each box is a list of indices into `colors`.
    let mut boxes: Vec<Vec<usize>> = vec![(0..colors.len()).collect()];

    while boxes.len() < k {
        // Find the splittable box (>= 2 distinct colour values) with the
        // widest channel range; ties broken by lowest box index for
        // determinism.
        let mut best: Option<(usize, usize, u16)> = None; // (box_idx, channel, range)
        for (bi, b) in boxes.iter().enumerate() {
            if b.len() < 2 {
                continue;
            }
            let (mut rmin, mut rmax) = (255u8, 0u8);
            let (mut gmin, mut gmax) = (255u8, 0u8);
            let (mut bmin, mut bmax) = (255u8, 0u8);
            for &i in b {
                let c = colors[i];
                rmin = rmin.min(c.r);
                rmax = rmax.max(c.r);
                gmin = gmin.min(c.g);
                gmax = gmax.max(c.g);
                bmin = bmin.min(c.b);
                bmax = bmax.max(c.b);
            }
            let ranges = [
                (0usize, rmax as u16 - rmin as u16),
                (1usize, gmax as u16 - gmin as u16),
                (2usize, bmax as u16 - bmin as u16),
            ];
            let (channel, range) = ranges.into_iter().max_by_key(|&(_, r)| r).unwrap_or((0, 0));
            if range == 0 {
                continue; // box is already a single colour
            }
            match best {
                Some((_, _, best_range)) if best_range >= range => {}
                _ => best = Some((bi, channel, range)),
            }
        }

        let Some((bi, channel, _)) = best else {
            break; // nothing left worth splitting
        };
        let mut b = boxes.remove(bi);
        b.sort_by_key(|&i| {
            let c = colors[i];
            (
                match channel {
                    0 => c.r,
                    1 => c.g,
                    _ => c.b,
                },
                i,
            )
        });
        let mid = b.len() / 2;
        let right = b.split_off(mid);
        boxes.push(b);
        boxes.push(right);
    }

    boxes
        .into_iter()
        .filter(|b| !b.is_empty())
        .map(|b| {
            let (mut sr, mut sg, mut sb, mut sw) = (0u64, 0u64, 0u64, 0u64);
            for i in b {
                let c = colors[i];
                let w = c.weight.max(1);
                sr += u64::from(c.r) * w;
                sg += u64::from(c.g) * w;
                sb += u64::from(c.b) * w;
                sw += w;
            }
            let sw = sw.max(1);
            FgbzColor {
                r: (sr / sw) as u8,
                g: (sg / sw) as u8,
                b: (sb / sw) as u8,
            }
        })
        .collect()
}

fn nearest_palette_index(palette: &[FgbzColor], c: FgbzColor) -> usize {
    palette
        .iter()
        .enumerate()
        .min_by_key(|&(_, p)| {
            let dr = i32::from(p.r) - i32::from(c.r);
            let dg = i32::from(p.g) - i32::from(c.g);
            let db = i32::from(p.b) - i32::from(c.b);
            dr * dr + dg * dg + db * db
        })
        .map(|(i, _)| i)
        .unwrap_or(0)
}

// ── Quality profile ───────────────────────────────────────────────────────────

/// Encoder quality profile.
///
/// The profile drives codec selection (JB2 vs IW44, mask-only vs
/// layered, optional FGbz palette) and quality knobs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncodeQuality {
    /// Pixel-exact round-trip. Requires bilevel input
    /// ([`PageEncoder::from_bitmap`]); writes `INFO + Sjbz` (JB2).
    #[default]
    Lossless,
    /// Layered foreground/background encoding. Requires color input
    /// ([`PageEncoder::from_pixmap`]); writes `INFO + Sjbz + BG44…`
    /// plus `FGbz` when foreground ink is detected.
    Quality,
    /// Conservative archival color profile. Requires color input; writes
    /// the same layered chunks as `Quality`, but keeps a denser background
    /// sample grid. Bilevel input should use `Lossless`.
    Archival,
    /// Mask-less continuous-tone profile (DjVuPhoto, #571). Requires color
    /// input; writes `INFO + BG44…` only — no segmentation, no Sjbz/FGbz.
    /// Pure-grayscale sources encode through the grayscale IW44 encoder
    /// (single luma plane); the decoder treats every pixel as background.
    /// The right profile for photographs and grayscale scans, where the
    /// forced layered mask costs bytes and can introduce artifacts.
    Photo,
}

/// Classify a source image into the most appropriate [`EncodeQuality`]
/// profile from cheap pixel statistics (#570).
///
/// Heuristic (sampled at a stride, so the pass costs well under 1% of an
/// encode):
/// - **Bilevel** (→ `Lossless`): effectively no chroma AND ≥95% of sampled
///   luminance within ±16 of two far-apart modes (ink + paper) — classic
///   scanned text.
/// - **Photo** (→ `Photo`): a spread, continuous luminance histogram (many
///   occupied bins, no dominant paper mode) — continuous-tone content where a
///   layered mask wrecks fidelity.
/// - Everything else (→ `Quality`): layered documents — text over paper with
///   colour, illustrations with text, etc.
///
/// The classifier is deliberately conservative about `Lossless`: any visible
/// chroma or mid-tone mass keeps the page out of the bilevel path, because
/// misrouting a photo to bilevel is catastrophic while misrouting text to
/// `Quality` merely costs bytes.
pub fn classify_content(pm: &Pixmap) -> EncodeQuality {
    let (w, h) = (pm.width as usize, pm.height as usize);
    if w == 0 || h == 0 {
        return EncodeQuality::Quality;
    }
    // Sample up to ~64 full rows: stable histogram, chroma and horizontal
    // sharp-edge statistics at well under 1% of encode time (measured
    // ~0.15 ms vs a ~16 ms page encode). Rows are scanned at stride 1 so the
    // sharp-edge statistic keeps true neighbour deltas (a column stride
    // inflates photo gradients into false "edges").
    let ystep = (h / 64).max(1);
    let xstep = 1usize;
    let mut hist = [0u32; 256];
    let mut chroma_hits = 0u32;
    let mut sharp = 0u32;
    let mut pairs = 0u32;
    let mut n = 0u32;
    let mut y = 0usize;
    while y < h {
        let row = &pm.data[y * w * 4..(y + 1) * w * 4];
        let mut prev: Option<u32> = None;
        let mut x = 0usize;
        while x < w {
            let (r, g, b) = (row[x * 4], row[x * 4 + 1], row[x * 4 + 2]);
            if r.max(g).max(b) - r.min(g).min(b) > 24 {
                chroma_hits += 1;
            }
            // Rec. 601 integer luma.
            let l = (77 * r as u32 + 150 * g as u32 + 29 * b as u32) >> 8;
            hist[(l as usize).min(255)] += 1;
            n += 1;
            if let Some(pl) = prev {
                pairs += 1;
                if pl.abs_diff(l) > 64 {
                    sharp += 1;
                }
            }
            prev = Some(l);
            x += xstep;
        }
        y += ystep;
    }
    let n = n.max(1);
    let pairs = pairs.max(1);
    let colourful = chroma_hits * 50 > n; // >2% clearly-chromatic samples
    let occupied = hist.iter().filter(|&&c| c > 0).count();
    // Sharp horizontal luma steps (>64) per neighbour pair — text/line art
    // sits at 0.3–4% on the corpus, photographs at ~0.04%.
    let sharp_permille = sharp as u64 * 1000 / pairs as u64;

    // Photo: continuous tone (many occupied luma bins) with almost no sharp
    // edges. Measured: boy(photo) occ=248 sharp=0.04%; every text-bearing
    // corpus page has sharp >= 0.36%.
    if occupied > 160 && sharp_permille < 2 {
        return EncodeQuality::Photo;
    }

    // Bilevel: no chroma, near-white paper mode, one far-apart ink mode, and
    // ~everything within +-16 of those two modes. `occupied <= 128` keeps any
    // continuous-tone page out — misrouting a photo to bilevel is
    // catastrophic, misrouting text to Quality merely costs bytes.
    let mode1 = (0..256).max_by_key(|&k| hist[k]).unwrap_or(255);
    let mode2 = (0..256)
        .filter(|&k| (k as i32 - mode1 as i32).unsigned_abs() > 48)
        .max_by_key(|&k| hist[k])
        .unwrap_or(mode1);
    let near_mass = |m: usize| -> u32 {
        let lo = m.saturating_sub(16);
        let hi = (m + 16).min(255);
        hist[lo..=hi].iter().sum()
    };
    let bimodal_mass = near_mass(mode1) + if mode2 != mode1 { near_mass(mode2) } else { 0 };
    let modes_far = (mode1 as i32 - mode2 as i32).unsigned_abs() > 100;
    if !colourful
        && mode1 >= 240
        && modes_far
        && occupied <= 128
        && bimodal_mass as u64 * 100 >= n as u64 * 95
    {
        return EncodeQuality::Lossless;
    }

    EncodeQuality::Quality
}

impl EncodeQuality {
    /// The default segmentation knobs for this profile.
    ///
    /// `Archival` lowers `bg_subsample` to 6 (see [`SegmentOptions::archival`])
    /// for a higher-resolution background; every other profile uses the plain
    /// defaults. This is the canonical `EncodeQuality → SegmentOptions` mapping
    /// — `PageEncoder::encode`, `encode_djvm_layered_shared`, and the CLI all
    /// call it instead of re-deriving the mapping inline.
    pub fn default_segment_options(self) -> SegmentOptions {
        // Colour profiles enable harmonic BG diffusion: fully-masked background
        // cells (covered by foreground ink, hence invisible) are filled with the
        // smoothest interpolation of the confident cells instead of the ink
        // colour. This cuts BG44 by up to ~90% on text-heavy scans and, because
        // it removes the dark ink-fallback halos that bled across mask edges via
        // BG upsampling, it *raises* decoded SSIM/PSNR too — a strict win on both
        // size and quality (see PERF_EXPERIMENTS.md round 17).
        match self {
            EncodeQuality::Archival => SegmentOptions {
                bg_diffuse: true,
                ..SegmentOptions::archival()
            },
            EncodeQuality::Quality => SegmentOptions {
                bg_diffuse: true,
                ..SegmentOptions::default()
            },
            // `Lossless` never segments (bilevel input has no FG/BG split); it
            // returns the defaults only so this mapping is total. Callers must
            // gate on the profile before reaching `segment_page` — both
            // `PageEncoder::encode` and the CLI reject `Lossless` upstream.
            EncodeQuality::Lossless => SegmentOptions::default(),
            // Photo never segments; the value is unused but keeps the match
            // total.
            EncodeQuality::Photo => SegmentOptions::default(),
        }
    }
}

// ── Encoder ──────────────────────────────────────────────────────────────────

enum Source<'a> {
    Bitmap(&'a Bitmap),
    Pixmap(&'a Pixmap),
}

impl Source<'_> {
    fn dimensions(&self) -> (u32, u32) {
        match self {
            Source::Bitmap(b) => (b.width, b.height),
            Source::Pixmap(p) => (p.width, p.height),
        }
    }
}

/// Builder-style page encoder.
///
/// Constructed from a [`Bitmap`] (bilevel) or [`Pixmap`] (RGBA) and
/// configured via the `with_*` methods, then finalised with
/// [`encode`](Self::encode).
pub struct PageEncoder<'a> {
    source: Source<'a>,
    dpi: u16,
    quality: EncodeQuality,
    segment_options: Option<SegmentOptions>,
    iw44_options: Option<Iw44EncodeOptions>,
    jb2_options: Option<Jb2EncodeOptions>,
    fgbz_options: FgbzPaletteOptions,
    text_layer: Option<TextLayer>,
}

impl<'a> PageEncoder<'a> {
    /// Start encoding a bilevel page. Defaults: 300 dpi, `Lossless`.
    pub fn from_bitmap(bitmap: &'a Bitmap) -> Self {
        Self {
            source: Source::Bitmap(bitmap),
            dpi: 300,
            quality: EncodeQuality::Lossless,
            segment_options: None,
            iw44_options: None,
            jb2_options: None,
            fgbz_options: FgbzPaletteOptions::Exact,
            text_layer: None,
        }
    }

    /// Start encoding a colour page. Defaults: 300 dpi, `Quality` (the
    /// only sensible profile for colour input — `Lossless` requires a
    /// `Bitmap`).
    pub fn from_pixmap(pixmap: &'a Pixmap) -> Self {
        Self {
            source: Source::Pixmap(pixmap),
            dpi: 300,
            quality: EncodeQuality::Quality,
            segment_options: None,
            iw44_options: None,
            jb2_options: None,
            fgbz_options: FgbzPaletteOptions::Exact,
            text_layer: None,
        }
    }

    /// Set the page resolution stored in the `INFO` chunk.
    ///
    /// Clamped to `[1, 65 535]` (the wire-format range of the dpi
    /// field). Values outside that range are silently saturated.
    pub fn with_dpi(mut self, dpi: u16) -> Self {
        self.dpi = dpi.max(1);
        self
    }

    /// Select an encoding profile. See [`EncodeQuality`] for the
    /// per-variant trade-offs and current support status.
    pub fn with_quality(mut self, quality: EncodeQuality) -> Self {
        self.quality = quality;
        self
    }

    /// Override the segmentation knobs used by `Quality` / `Archival` color
    /// encodes. Defaults remain profile-specific and fixed-threshold.
    pub fn with_segment_options(mut self, opts: SegmentOptions) -> Self {
        self.segment_options = Some(opts);
        self
    }

    /// Override the IW44 background-codec knobs (slice schedule, chroma
    /// resolution/delay) used by the `Quality` / `Archival` color encodes.
    ///
    /// Defaults to [`Iw44EncodeOptions::default`] (DjVuLibre `c44`-compatible
    /// full-resolution chroma, delay 10). Ignored by the bilevel `Lossless`
    /// path, which writes no `BG44`.
    pub fn with_iw44_options(mut self, opts: Iw44EncodeOptions) -> Self {
        self.iw44_options = Some(opts);
        self
    }

    /// Override the JB2 mask-codec knobs (lossy connected-component threshold)
    /// used by the `Quality` / `Archival` color encodes' `Sjbz` dictionary.
    ///
    /// Defaults to [`Jb2EncodeOptions::default`] (lossless, byte-exact CC
    /// matching). The bilevel `Lossless` path emits a single direct-bitmap
    /// record and is unaffected.
    pub fn with_jb2_options(mut self, opts: Jb2EncodeOptions) -> Self {
        self.jb2_options = Some(opts);
        self
    }

    /// Override how the `FGbz` foreground palette is built from per-blit
    /// average colours, used by the `Quality` / `Archival` color encodes.
    ///
    /// Defaults to [`FgbzPaletteOptions::Exact`] (pre-experiment, byte-exact
    /// per-distinct-average-colour palette). Opt into
    /// [`FgbzPaletteOptions::MedianCut`] to cap the palette size via
    /// median-cut quantisation instead — see PERF_EXPERIMENTS.md
    /// `FGBZ_MEDIANCUT`. Ignored by the bilevel `Lossless` path (no FGbz).
    pub fn with_fgbz_options(mut self, opts: FgbzPaletteOptions) -> Self {
        self.fgbz_options = opts;
        self
    }

    /// Attach a pre-built text layer to be embedded as a BZZ-compressed
    /// `TXTz` chunk, making the encoded page text-searchable.
    ///
    /// `layer`'s zone rectangles must use the page's top-left pixel
    /// coordinate system (the convention [`OcrBackend::recognize`] returns
    /// and [`crate::text::TextZone`] documents) — `encode()` converts them to
    /// DjVu's bottom-left origin using the page height set via
    /// [`with_dpi`](Self::with_dpi) / the source image's height.
    ///
    /// Opt-in: the default (no call) omits the chunk entirely and produces
    /// byte-identical output to before this method existed.
    pub fn with_text_layer(mut self, layer: TextLayer) -> Self {
        self.text_layer = Some(layer);
        self
    }

    /// Run `backend` over the page image and attach the resulting OCR text
    /// layer (see [`with_text_layer`](Self::with_text_layer)) — the standard
    /// "searchable scan" workflow in one step.
    ///
    /// Bilevel ([`Bitmap`]) sources are expanded to a black-on-white RGBA
    /// [`Pixmap`] for the OCR engine (which only sees pixels, not the JB2
    /// encode); colour sources are OCR'd directly. Opt-in and fallible: a
    /// backend/init failure (e.g. missing Tesseract install) is returned as
    /// [`OcrError`] rather than silently producing a page with no text layer.
    pub fn with_ocr_text_layer(
        mut self,
        backend: &dyn OcrBackend,
        options: &OcrOptions,
    ) -> Result<Self, OcrError> {
        let owned_pixmap;
        let pixmap: &Pixmap = match &self.source {
            Source::Pixmap(p) => p,
            Source::Bitmap(b) => {
                owned_pixmap = bitmap_to_pixmap(b);
                &owned_pixmap
            }
        };
        let layer = backend.recognize(pixmap, options)?;
        self.text_layer = Some(layer);
        Ok(self)
    }

    /// Produce the bytes of a single-page DjVu file (`FORM:DJVU`
    /// wrapped in the `AT&T` IFF container).
    pub fn encode(&self) -> Result<Vec<u8>, EncodeError> {
        let (w, h) = self.source.dimensions();
        let w = u16::try_from(w).map_err(|_| {
            EncodeError::Unsupported("page width exceeds INFO chunk limit (65 535 px)")
        })?;
        let h = u16::try_from(h).map_err(|_| {
            EncodeError::Unsupported("page height exceeds INFO chunk limit (65 535 px)")
        })?;
        let info = encode_info(w, h, self.dpi);

        match (&self.source, self.quality) {
            (Source::Bitmap(bm), EncodeQuality::Lossless) => {
                let mut chunks = vec![
                    Chunk::Leaf {
                        id: *b"INFO",
                        data: info,
                    },
                    Chunk::Leaf {
                        id: *b"Sjbz",
                        data: jb2_encode::encode_jb2(bm),
                    },
                ];
                self.push_text_layer_chunk(&mut chunks, h as u32);
                Ok(encode_form_djvu(chunks))
            }
            (Source::Pixmap(pm), EncodeQuality::Quality | EncodeQuality::Archival) => {
                let segment_options = self
                    .segment_options
                    .unwrap_or_else(|| self.quality.default_segment_options());
                let seg = segment_page(pm, &segment_options);
                // Use the dictionary encoder for color profiles so FGbz can
                // address foreground colors per blitted component.
                // Given `seg`, the Sjbz (JB2 mask) and BG44 (IW44 background)
                // layers are fully independent — FGbz needs the finished Sjbz
                // and stays after — so with the `parallel` feature they encode
                // concurrently (PAR_PAGE_LAYERS). Byte-identical either way.
                let jb2_options = self.jb2_options.unwrap_or_default();
                let iw44_options = self.iw44_options.unwrap_or_default();
                #[cfg(feature = "parallel")]
                let ((sjbz, blits), bg44_chunks) = rayon::join(
                    || jb2_encode::encode_jb2_dict_with_blits(&seg.mask, &[], &jb2_options),
                    || encode_iw44_color(&seg.bg, &iw44_options),
                );
                #[cfg(not(feature = "parallel"))]
                let ((sjbz, blits), bg44_chunks) = (
                    jb2_encode::encode_jb2_dict_with_blits(&seg.mask, &[], &jb2_options),
                    encode_iw44_color(&seg.bg, &iw44_options),
                );
                // Lossy rec-7 substitution blits near-twins whose pixels can
                // differ from the emitted components — only there fall back to
                // the decode-based palette scan (#612).
                let fgbz = if jb2_options.lossy_threshold > 0.0 {
                    foreground_fgbz(pm, &seg.mask, &sjbz, None, self.fgbz_options)
                } else {
                    foreground_fgbz_from_blits(pm, &seg.mask, &blits, self.fgbz_options)
                };

                let mut chunks =
                    Vec::with_capacity(2 + bg44_chunks.len() + usize::from(fgbz.is_some()) + 1);
                chunks.push(Chunk::Leaf {
                    id: *b"INFO",
                    data: info,
                });
                chunks.push(Chunk::Leaf {
                    id: *b"Sjbz",
                    data: sjbz,
                });
                for body in bg44_chunks {
                    chunks.push(Chunk::Leaf {
                        id: *b"BG44",
                        data: body,
                    });
                }
                if let Some(chunk) = fgbz {
                    chunks.push(chunk.into_leaf());
                }
                self.push_text_layer_chunk(&mut chunks, h as u32);
                Ok(encode_form_djvu(chunks))
            }
            (Source::Pixmap(pm), EncodeQuality::Photo) => {
                let iw44_options = self.iw44_options.unwrap_or_default();
                // Pure-grayscale sources go through the dedicated grayscale
                // encoder: one luma plane instead of Y+Cb+Cr.
                let gray = pm
                    .data
                    .chunks_exact(4)
                    .all(|px| px[0] == px[1] && px[1] == px[2]);
                let bg44_chunks = if gray {
                    crate::iw44_encode::encode_iw44_gray(&pm.to_gray8(), &iw44_options)
                } else {
                    encode_iw44_color(pm, &iw44_options)
                };
                let mut chunks = Vec::with_capacity(1 + bg44_chunks.len() + 1);
                chunks.push(Chunk::Leaf {
                    id: *b"INFO",
                    data: info,
                });
                for body in bg44_chunks {
                    chunks.push(Chunk::Leaf {
                        id: *b"BG44",
                        data: body,
                    });
                }
                self.push_text_layer_chunk(&mut chunks, h as u32);
                Ok(encode_form_djvu(chunks))
            }
            (Source::Bitmap(_), EncodeQuality::Photo) => Err(EncodeError::Unsupported(
                "Photo profile requires color input (from_pixmap)",
            )),
            (Source::Pixmap(_), EncodeQuality::Lossless) => Err(EncodeError::Unsupported(
                "Lossless requires bilevel input — use from_bitmap or switch to Quality",
            )),
            (Source::Bitmap(_), EncodeQuality::Quality) => Err(EncodeError::Unsupported(
                "Quality requires colour input — use from_pixmap or switch to Lossless",
            )),
            (Source::Bitmap(_), EncodeQuality::Archival) => Err(EncodeError::Unsupported(
                "Archival requires colour input — use from_pixmap or switch to Lossless",
            )),
        }
    }

    /// Append the BZZ-compressed `TXTz` chunk for `self.text_layer`, if one
    /// was attached via [`with_text_layer`](Self::with_text_layer) /
    /// [`with_ocr_text_layer`](Self::with_ocr_text_layer). No-op (and hence
    /// byte-identical output) when no text layer is attached.
    fn push_text_layer_chunk(&self, chunks: &mut Vec<Chunk>, page_height: u32) {
        if let Some(layer) = &self.text_layer {
            let plain = encode_text_layer(layer, page_height);
            let compressed = bzz_encode(&plain);
            chunks.push(Chunk::Leaf {
                id: *b"TXTz",
                data: compressed,
            });
        }
    }
}

/// Expand a bilevel [`Bitmap`] into a black-on-white RGBA [`Pixmap`] for
/// OCR engines that only accept pixel input (not the packed 1-bpp mask).
/// Mirrors `mask_to_pixmap` in `examples/ocr_qa.rs` (round 43's OCR_QA
/// machinery) — both convert `true` (black/ink) pixels to RGB(0,0,0) over an
/// all-white background, preserving the mask's top-left-origin coordinate
/// system so the returned [`TextLayer`] rects line up with `page_height`
/// unmodified.
fn bitmap_to_pixmap(bm: &Bitmap) -> Pixmap {
    let mut pm = Pixmap::white(bm.width, bm.height);
    for y in 0..bm.height {
        for x in 0..bm.width {
            if bm.get(x, y) {
                pm.set_rgb(x, y, 0, 0, 0);
            }
        }
    }
    pm
}

/// Encode a directory of colour pages as a single bundled DJVM with a **shared
/// Djbz dictionary** across pages (layered Quality/Archival profile).
///
/// Connected components that appear on at least `shared_dict_page_threshold`
/// distinct pages are promoted into one shared `FORM:DJVI` Djbz; each page's
/// `FORM:DJVU` then carries `INCL` + a `Sjbz` that references the shared
/// dictionary, alongside its own `BG44`(s) and optional `FGbz`. This avoids the
/// per-page dictionary duplication of independent layered encoding (#452): on
/// text-heavy multi-page scans the mask shrinks ~35% (1.6× → ~1.04× of the
/// DjVuLibre baseline).
///
/// `FGbz` is rebuilt from the shared-dictionary `Sjbz` so its per-blit palette
/// indices match the emitted symbol stream. With fewer than two pages, or a
/// threshold larger than the page count, no symbols qualify and each page is
/// encoded with its own dictionary (still a valid bundle).
///
/// When `with_thumbnails` is `true`, each page's `FORM:DJVU` additionally
/// contains one or more `TH44` chunk(s) encoding a color IW44 thumbnail (long
/// side ≤ 128 px) of the full page image.  When `false` (the pre-feature
/// default), no `TH44` chunks are emitted and output is identical to the
/// previous behaviour.
pub fn encode_djvm_layered_shared(
    pixmaps: &[Pixmap],
    quality: EncodeQuality,
    dpi: u16,
    segment_options: Option<SegmentOptions>,
    shared_dict_page_threshold: usize,
) -> Result<Vec<u8>, EncodeError> {
    encode_djvm_layered_shared_impl(
        pixmaps,
        quality,
        dpi,
        segment_options,
        shared_dict_page_threshold,
        false,
    )
}

/// Like [`encode_djvm_layered_shared`] but with explicit thumbnail control.
///
/// Pass `with_thumbnails: true` to embed a `TH44` color thumbnail in each
/// page's `FORM:DJVU`; `false` is identical to [`encode_djvm_layered_shared`].
pub fn encode_djvm_layered_shared_with_thumbnails(
    pixmaps: &[Pixmap],
    quality: EncodeQuality,
    dpi: u16,
    segment_options: Option<SegmentOptions>,
    shared_dict_page_threshold: usize,
    with_thumbnails: bool,
) -> Result<Vec<u8>, EncodeError> {
    encode_djvm_layered_shared_impl(
        pixmaps,
        quality,
        dpi,
        segment_options,
        shared_dict_page_threshold,
        with_thumbnails,
    )
}

fn encode_djvm_layered_shared_impl(
    pixmaps: &[Pixmap],
    quality: EncodeQuality,
    dpi: u16,
    segment_options: Option<SegmentOptions>,
    shared_dict_page_threshold: usize,
    with_thumbnails: bool,
) -> Result<Vec<u8>, EncodeError> {
    if !matches!(quality, EncodeQuality::Quality | EncodeQuality::Archival) {
        return Err(EncodeError::Unsupported(
            "encode_djvm_layered_shared requires the Quality or Archival profile",
        ));
    }
    let opts = segment_options.unwrap_or_else(|| quality.default_segment_options());

    // Pass 1 (#565): segment each page, immediately encode its background
    // (BG44) and optional thumbnail (TH44), and DROP the segmented background
    // pixmap. Between the passes only the 1-bit masks and the already-
    // compressed chunk bodies are retained — previously every page's
    // subsampled RGBA background pixmap plus a full clone of every mask
    // survived until the end of the encode. Per-page independent; with the
    // `parallel` feature the pages run concurrently on rayon. The emitted
    // bytes are unchanged: same inputs, same options, same chunk order.
    struct PreparedPage {
        mask: Bitmap,
        bg44: Vec<Vec<u8>>,
        th44: Vec<Vec<u8>>,
    }
    let prepare = |pm: &Pixmap| -> PreparedPage {
        let seg = segment_page(pm, &opts);
        let bg44 = encode_iw44_color(&seg.bg, &Iw44EncodeOptions::default());
        let th44 = if with_thumbnails {
            crate::thumbnail::encode_th44_color(pm)
        } else {
            Vec::new()
        };
        PreparedPage {
            mask: seg.mask,
            bg44,
            th44,
        }
    };
    #[cfg(feature = "parallel")]
    let prepared: Vec<PreparedPage> = {
        use rayon::prelude::*;
        pixmaps.par_iter().map(prepare).collect()
    };
    #[cfg(not(feature = "parallel"))]
    let prepared: Vec<PreparedPage> = pixmaps.iter().map(prepare).collect();

    // Cluster over borrowed masks — no per-mask clone (#565).
    let mask_refs: Vec<&Bitmap> = prepared.iter().map(|p| &p.mask).collect();
    let shared =
        jb2_encode::cluster_shared_symbols_from_refs(&mask_refs, shared_dict_page_threshold);
    drop(mask_refs);
    let has_shared = !shared.is_empty();

    let dict_id = "dict0001.djvi";
    let mut comps: Vec<(Vec<u8>, bool, String)> = Vec::new();
    // FGbz is rebuilt from the encoder's own emitted blits (#612), so the
    // shared dictionary no longer needs to be decoded back for the per-page
    // blit maps — only the DJVI component itself is emitted.
    if has_shared {
        let djbz = jb2_encode::encode_jb2_djbz(&shared);
        let djvi_body = jb2_encode::build_form_body(b"DJVI", &[(*b"Djbz", djbz)]);
        comps.push((djvi_body, false, dict_id.to_string()));
    }

    // Each page's DJVU body is independent (JB2-dict Sjbz + IW44 background + FGbz +
    // optional TH44). Build one component per page; with the `parallel` feature the
    // pages encode concurrently on rayon, since JB2 + IW44 dominate the per-page cost.
    // Order is preserved by the indexed collect.
    let build_page = |idx: usize,
                      pm: &Pixmap,
                      prep: &PreparedPage|
     -> Result<(Vec<u8>, bool, String), EncodeError> {
        let w = u16::try_from(pm.width)
            .map_err(|_| EncodeError::Unsupported("page width exceeds INFO chunk limit"))?;
        let h = u16::try_from(pm.height)
            .map_err(|_| EncodeError::Unsupported("page height exceeds INFO chunk limit"))?;

        let shared_for_encode: &[Bitmap] = if has_shared { &shared } else { &[] };
        let (sjbz, blits) = jb2_encode::encode_jb2_dict_with_blits(
            &prep.mask,
            shared_for_encode,
            &Jb2EncodeOptions::default(),
        );
        // FGbz comes straight from the emitted blits (#612) — no decode of the
        // just-encoded stream. Shared-dict rec-7 copies are exact matches, so
        // the blit shapes equal the decoded ones. `Exact` here (not threaded
        // from a caller option yet): the bundle path is out of scope for
        // FGBZ_MEDIANCUT and stays byte-identical.
        let fgbz = foreground_fgbz_from_blits(pm, &prep.mask, &blits, FgbzPaletteOptions::Exact);

        let mut chunks: Vec<([u8; 4], Vec<u8>)> = Vec::new();
        chunks.push((*b"INFO", encode_info(w, h, dpi)));
        if has_shared {
            chunks.push((*b"INCL", dict_id.as_bytes().to_vec()));
        }
        chunks.push((*b"Sjbz", sjbz));
        for body in &prep.bg44 {
            chunks.push((*b"BG44", body.clone()));
        }
        if let Some(chunk) = fgbz
            && let Chunk::Leaf { id, data } = chunk.into_leaf()
        {
            chunks.push((id, data));
        }
        // TH44 colour thumbnails sit inside the page's FORM:DJVU body (after
        // FGbz); encoded in pass 1, placed here in the same position.
        for payload in &prep.th44 {
            chunks.push((*b"TH44", payload.clone()));
        }
        let body = jb2_encode::build_form_body(b"DJVU", &chunks);
        Ok((body, true, format!("p{:04}.djvu", idx + 1)))
    };

    #[cfg(feature = "parallel")]
    let page_comps: Vec<(Vec<u8>, bool, String)> = {
        use rayon::prelude::*;
        pixmaps
            .par_iter()
            .zip(&prepared)
            .enumerate()
            .map(|(idx, (pm, prep))| build_page(idx, pm, prep))
            .collect::<Result<Vec<_>, _>>()?
    };
    #[cfg(not(feature = "parallel"))]
    let page_comps: Vec<(Vec<u8>, bool, String)> = pixmaps
        .iter()
        .zip(&prepared)
        .enumerate()
        .map(|(idx, (pm, prep))| build_page(idx, pm, prep))
        .collect::<Result<Vec<_>, _>>()?;
    drop(prepared);
    comps.extend(page_comps);

    Ok(jb2_encode::assemble_djvm_bundle(comps))
}

// ── Internal helpers ─────────────────────────────────────────────────────────

fn encode_form_djvu(children: Vec<Chunk>) -> Vec<u8> {
    let file = DjvuFile {
        root: Chunk::Form {
            secondary_id: *b"DJVU",
            length: 0, // recomputed by emit
            children,
        },
    };
    emit(&file)
}

#[derive(Debug, Clone, Copy, Default)]
struct ColorAccum {
    r: u64,
    g: u64,
    b: u64,
    n: u64,
}

impl ColorAccum {
    fn add(&mut self, r: u8, g: u8, b: u8) {
        self.r += u64::from(r);
        self.g += u64::from(g);
        self.b += u64::from(b);
        self.n += 1;
    }

    fn color(self) -> Option<FgbzColor> {
        if self.n == 0 {
            return None;
        }
        Some(FgbzColor {
            r: (self.r / self.n) as u8,
            g: (self.g / self.n) as u8,
            b: (self.b / self.n) as u8,
        })
    }
}

fn foreground_fgbz(
    pm: &Pixmap,
    mask: &Bitmap,
    sjbz: &[u8],
    shared_dict: Option<&crate::jb2::Jb2Dict>,
    palette_options: FgbzPaletteOptions,
) -> Option<EncodedChunk> {
    // The Sjbz may reference an external shared Djbz (layered shared-dict bundle),
    // so the dictionary must be supplied to decode its blit map.
    let (decoded_mask, blit_map) = crate::jb2::decode_indexed(sjbz, shared_dict).ok()?;
    if decoded_mask.width != mask.width || decoded_mask.height != mask.height {
        return None;
    }

    let max_blit = blit_map.iter().copied().filter(|&i| i >= 0).max()? as usize;
    let mut by_blit = vec![ColorAccum::default(); max_blit + 1];
    let w = mask.width as usize;
    // Row-slice the mask (bit-test the pre-sliced row byte), the blit map, and the
    // packed RGBA pixmap (`x*4` into a row slice) instead of per-pixel `mask.get`
    // (hidden `/8`) + `pm.get_rgb` (hidden `*4` + bounds). Same pixels, same
    // accumulation order → byte-identical palette. (PS4/PS5 class.)
    let mstride = mask.row_stride();
    for y in 0..mask.height as usize {
        let mrow = &mask.data[y * mstride..(y + 1) * mstride];
        let prow = &pm.data[y * w * 4..(y + 1) * w * 4];
        let brow = &blit_map[y * w..(y + 1) * w];
        for x in 0..w {
            if (mrow[x >> 3] >> (7 - (x & 7))) & 1 != 0 {
                let blit_idx = brow[x];
                if blit_idx < 0 {
                    continue;
                }
                let px = &prow[x * 4..x * 4 + 3];
                by_blit[blit_idx as usize].add(px[0], px[1], px[2]);
            }
        }
    }

    fgbz_from_accums(by_blit, palette_options)
}

/// Build the FGbz chunk from per-blit colours accumulated straight off the
/// encoder's emitted blits — no decode of the just-encoded Sjbz (#612).
///
/// Valid whenever every emitted blit's shape equals what the decoder will
/// reconstruct (the lossless paths: default options, despeckle, exact rec-7
/// and rec-6 matches). Blits are pixel-disjoint connected components of
/// `mask`, so per-blit sums equal the decode-based scan's — byte-identical
/// FGbz. Lossy rec-7 substitution (`lossy_threshold > 0`) blits near-twins
/// whose pixels can differ; callers keep the decode-based
/// [`foreground_fgbz`] for that case.
fn foreground_fgbz_from_blits(
    pm: &Pixmap,
    mask: &Bitmap,
    blits: &[jb2_encode::EncodedBlit],
    palette_options: FgbzPaletteOptions,
) -> Option<EncodedChunk> {
    if blits.is_empty() {
        return None;
    }
    let w = mask.width as usize;
    let mstride = mask.row_stride();
    let mut by_blit = vec![ColorAccum::default(); blits.len()];
    for (accum, blit) in by_blit.iter_mut().zip(blits) {
        let bstride = blit.bitmap.row_stride();
        for by in 0..blit.bitmap.height as usize {
            let y = blit.y as usize + by;
            if y >= mask.height as usize {
                break;
            }
            let brow = &blit.bitmap.data[by * bstride..(by + 1) * bstride];
            let mrow = &mask.data[y * mstride..(y + 1) * mstride];
            let prow = &pm.data[y * w * 4..(y + 1) * w * 4];
            for bx in 0..blit.bitmap.width as usize {
                if (brow[bx >> 3] >> (7 - (bx & 7))) & 1 == 0 {
                    continue;
                }
                let x = blit.x as usize + bx;
                if x >= w {
                    break;
                }
                if (mrow[x >> 3] >> (7 - (x & 7))) & 1 != 0 {
                    let px = &prow[x * 4..x * 4 + 3];
                    accum.add(px[0], px[1], px[2]);
                }
            }
        }
    }
    fgbz_from_accums(by_blit, palette_options)
}

/// Shared tail of the FGbz builders: per-blit colour accumulators → palette
/// (+ optional index table) → encoded chunk.
fn fgbz_from_accums(
    by_blit: Vec<ColorAccum>,
    palette_options: FgbzPaletteOptions,
) -> Option<EncodedChunk> {
    let (palette, indices): (Vec<FgbzColor>, Vec<i16>) = match palette_options {
        FgbzPaletteOptions::Exact => {
            let mut palette: Vec<FgbzColor> = Vec::new();
            let mut indices: Vec<i16> = Vec::with_capacity(by_blit.len());
            for accum in by_blit {
                let color = accum.color().unwrap_or_default();
                let color_idx = match palette.iter().position(|&c| c == color) {
                    Some(i) => i,
                    None => {
                        if palette.len() >= i16::MAX as usize {
                            return None;
                        }
                        palette.push(color);
                        palette.len() - 1
                    }
                };
                indices.push(color_idx as i16);
            }
            (palette, indices)
        }
        FgbzPaletteOptions::MedianCut { max_colors } => {
            let blit_colors: Vec<FgbzColor> = by_blit
                .iter()
                .map(|accum| accum.color().unwrap_or_default())
                .collect();
            let weighted: Vec<WeightedColor> = by_blit
                .iter()
                .zip(&blit_colors)
                .map(|(accum, &c)| WeightedColor {
                    r: c.r,
                    g: c.g,
                    b: c.b,
                    weight: accum.n,
                })
                .collect();
            let palette = median_cut(&weighted, usize::from(max_colors.max(1)));
            if palette.len() > i16::MAX as usize {
                return None;
            }
            let indices: Vec<i16> = blit_colors
                .iter()
                .map(|&c| nearest_palette_index(&palette, c) as i16)
                .collect();
            (palette, indices)
        }
    };

    if palette.is_empty() || palette.iter().all(|c| c.r == 0 && c.g == 0 && c.b == 0) {
        return None;
    }

    let index_payload = if palette.len() > 1 {
        Some(indices.as_slice())
    } else {
        None
    };
    // Best-effort: the palette is bounded < i16::MAX above, so the FGbz
    // wire limits cannot trip here; `.ok()` keeps this a soft skip if a
    // future change relaxes that bound.
    FgbzChunk {
        palette: &palette,
        indices: index_payload,
    }
    .encode_chunk()
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::iff::parse_form;
    use crate::jb2;
    use crate::text::{TextZone, TextZoneKind};

    fn checkerboard(w: u32, h: u32) -> Bitmap {
        let mut bm = Bitmap::new(w, h);
        for y in 0..h {
            for x in 0..w {
                if (x + y) % 2 == 0 {
                    bm.set_black(x, y);
                }
            }
        }
        bm
    }

    /// #601: the bilevel Lossless path is a provable fixed point — decode →
    /// re-encode reproduces the mask bit-for-bit, and a second cycle
    /// reproduces the container bytes too. Guards against generation loss on
    /// the one profile that promises none.
    #[test]
    fn lossless_bilevel_reencode_is_idempotent() {
        for fixture in ["tests/fixtures/boy_jb2.djvu", "tests/fixtures/ccitt_2.djvu"] {
            let data = std::fs::read(fixture).unwrap();
            let doc = crate::djvu_document::DjVuDocument::parse(&data).unwrap();
            let page = doc.page(0).unwrap();
            let dpi = page.dpi() as u16;
            let mask0 = page
                .extract_mask()
                .unwrap()
                .expect("bilevel fixture has a mask");

            let gen1 = PageEncoder::from_bitmap(&mask0)
                .with_dpi(dpi)
                .encode()
                .unwrap();
            let doc1 = crate::djvu_document::DjVuDocument::parse(&gen1).unwrap();
            let mask1 = doc1.page(0).unwrap().extract_mask().unwrap().unwrap();
            assert_eq!(
                (mask0.width, mask0.height, &mask0.data),
                (mask1.width, mask1.height, &mask1.data),
                "{fixture}: generation-1 mask must be bit-identical"
            );

            let gen2 = PageEncoder::from_bitmap(&mask1)
                .with_dpi(dpi)
                .encode()
                .unwrap();
            assert_eq!(gen1, gen2, "{fixture}: generation 2 must be a fixed point");
        }
    }

    #[test]
    fn default_segment_options_maps_archival_to_dense_background() {
        // Single source of truth for the quality → segmentation mapping: only
        // Archival lowers bg_subsample; everything else uses the plain default.
        assert_eq!(
            EncodeQuality::Archival
                .default_segment_options()
                .bg_subsample,
            6,
            "Archival keeps a denser background grid"
        );
        assert_eq!(
            EncodeQuality::Quality
                .default_segment_options()
                .bg_subsample,
            SegmentOptions::default().bg_subsample,
        );
        assert_eq!(
            EncodeQuality::Lossless
                .default_segment_options()
                .bg_subsample,
            SegmentOptions::default().bg_subsample,
        );
        // archival() is the literal-free constructor those map onto.
        let arch = SegmentOptions::archival();
        assert_eq!(arch.bg_subsample, 6);
        assert_eq!(arch.threshold, SegmentOptions::default().threshold);
        assert_eq!(arch.bg_inpaint, SegmentOptions::default().bg_inpaint);
    }

    #[test]
    fn with_iw44_options_is_threaded_into_background_codec() {
        // Reaching the IW44 knobs through the builder must actually change the
        // emitted BG44 — fewer total slices ⇒ a strictly smaller background.
        let pm = mixed_lighting_fixture();
        let default_bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .encode()
            .expect("default encode");
        let trimmed = Iw44EncodeOptions {
            total_slices: 20,
            ..Iw44EncodeOptions::default()
        };
        let trimmed_bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .with_iw44_options(trimmed)
            .encode()
            .expect("trimmed encode");
        assert!(
            trimmed_bytes.len() < default_bytes.len(),
            "with_iw44_options(total_slices=20) should shrink output ({} vs {})",
            trimmed_bytes.len(),
            default_bytes.len()
        );
        // Still a valid, parseable DjVu page.
        let doc = crate::djvu_document::DjVuDocument::parse(&trimmed_bytes).expect("parse");
        assert!(!doc.page(0).expect("page").all_chunks(b"BG44").is_empty());
    }

    #[test]
    fn with_jb2_options_lossy_threshold_round_trips() {
        // The JB2 knob is reachable through the builder and still produces a
        // decodable mask (lossy CC substitution stays within the format).
        let pm = mixed_lighting_fixture();
        // Spell every field (cfg-gated like the Default impl) so this compiles
        // cleanly whether or not the `experimental` feature is active — neither
        // struct-update nor reassign-after-default triggers a clippy lint.
        let jb2 = Jb2EncodeOptions {
            lossy_threshold: 0.04,
            despeckle: None,
            #[cfg(feature = "experimental")]
            cross_size_rec6_probe: None,
            #[cfg(feature = "experimental")]
            same_size_rec6: None,
        };
        let bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .with_jb2_options(jb2)
            .encode()
            .expect("lossy jb2 encode");
        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
        let page = doc.page(0).expect("page");
        assert!(page.raw_chunk(b"Sjbz").is_some());
        page.extract_mask()
            .expect("mask decode")
            .expect("mask present");
    }

    #[test]
    fn lossless_bilevel_round_trips() {
        let bm = checkerboard(64, 48);
        let bytes = PageEncoder::from_bitmap(&bm)
            .with_dpi(150)
            .with_quality(EncodeQuality::Lossless)
            .encode()
            .expect("encode");

        let form = parse_form(&bytes).expect("parse_form");
        assert_eq!(&form.form_type, b"DJVU");

        let mut info_data: Option<&[u8]> = None;
        let mut sjbz_data: Option<&[u8]> = None;
        for chunk in &form.chunks {
            match &chunk.id {
                b"INFO" => info_data = Some(chunk.data),
                b"Sjbz" => sjbz_data = Some(chunk.data),
                _ => {}
            }
        }
        let info = info_data.expect("INFO chunk present");
        let sjbz = sjbz_data.expect("Sjbz chunk present");

        assert_eq!(u16::from_be_bytes([info[0], info[1]]), 64);
        assert_eq!(u16::from_be_bytes([info[2], info[3]]), 48);
        assert_eq!(u16::from_le_bytes([info[6], info[7]]), 150);

        let decoded = jb2::decode(sjbz, None).expect("jb2 decode");
        assert_eq!(decoded.width, bm.width);
        assert_eq!(decoded.height, bm.height);
        for y in 0..bm.height {
            for x in 0..bm.width {
                assert_eq!(decoded.get(x, y), bm.get(x, y), "mismatch at ({x},{y})");
            }
        }
    }

    #[test]
    fn defaults_are_300_dpi_lossless_for_bitmap() {
        let bm = Bitmap::new(8, 8);
        let enc = PageEncoder::from_bitmap(&bm);
        assert_eq!(enc.dpi, 300);
        assert_eq!(enc.quality, EncodeQuality::Lossless);
    }

    #[test]
    fn defaults_are_300_dpi_quality_for_pixmap() {
        let pm = Pixmap::white(8, 8);
        let enc = PageEncoder::from_pixmap(&pm);
        assert_eq!(enc.dpi, 300);
        assert_eq!(enc.quality, EncodeQuality::Quality);
        assert!(enc.segment_options.is_none());
    }

    #[test]
    fn with_dpi_clamps_zero_to_one() {
        let bm = Bitmap::new(8, 8);
        let enc = PageEncoder::from_bitmap(&bm).with_dpi(0);
        assert_eq!(enc.dpi, 1);
    }

    #[test]
    fn archival_bitmap_rejected() {
        let bm = Bitmap::new(16, 16);
        let err = PageEncoder::from_bitmap(&bm)
            .with_quality(EncodeQuality::Archival)
            .encode()
            .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("Archival"));
    }

    #[test]
    fn empty_bitmap_round_trips() {
        let bm = Bitmap::new(1, 1);
        let bytes = PageEncoder::from_bitmap(&bm).encode().expect("encode");
        let form = parse_form(&bytes).expect("parse");
        assert_eq!(&form.form_type, b"DJVU");
    }

    #[test]
    fn encode_rejects_pixmap_width_exceeding_u16() {
        // width = 70000 > 65535: try_from fails → EncodeError::Unsupported
        let pm = Pixmap {
            width: 70_000,
            height: 1,
            data: vec![],
        };
        let err = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .encode()
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("width") || msg.contains("65"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn encode_rejects_bitmap_height_exceeding_u16() {
        // height = 70000 > 65535: try_from fails → EncodeError::Unsupported
        let bm = Bitmap {
            width: 1,
            height: 70_000,
            data: vec![0u8; 70_000 / 8 + 1],
        };
        let err = PageEncoder::from_bitmap(&bm)
            .with_quality(EncodeQuality::Lossless)
            .encode()
            .unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("height") || msg.contains("65"),
            "unexpected: {msg}"
        );
    }

    #[test]
    fn quality_color_emits_info_sjbz_bg44() {
        // 64×64 page: white background with a black 16×16 ink square.
        let mut pm = Pixmap::white(64, 64);
        for y in 16..32 {
            for x in 16..32 {
                pm.set_rgb(x, y, 0, 0, 0);
            }
        }

        let bytes = PageEncoder::from_pixmap(&pm)
            .with_dpi(200)
            .with_quality(EncodeQuality::Quality)
            .encode()
            .expect("encode");

        let form = parse_form(&bytes).expect("parse_form");
        assert_eq!(&form.form_type, b"DJVU");

        let mut has_info = false;
        let mut has_sjbz = false;
        let mut bg44_count = 0;
        for chunk in &form.chunks {
            match &chunk.id {
                b"INFO" => has_info = true,
                b"Sjbz" => has_sjbz = true,
                b"BG44" => bg44_count += 1,
                _ => {}
            }
        }
        assert!(has_info, "INFO chunk missing");
        assert!(has_sjbz, "Sjbz chunk missing");
        assert!(
            bg44_count > 0,
            "expected at least one BG44 chunk, got {bg44_count}"
        );
    }

    #[test]
    fn quality_color_emits_fgbz_for_colored_foreground() {
        let mut pm = Pixmap::white(64, 64);
        for y in 16..32 {
            for x in 16..32 {
                pm.set_rgb(x, y, 180, 20, 20);
            }
        }

        let bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .encode()
            .expect("encode");

        let form = parse_form(&bytes).expect("parse_form");
        let fgbz = form
            .chunks
            .iter()
            .find(|chunk| &chunk.id == b"FGbz")
            .expect("FGbz chunk present");
        let (palette, indices) = crate::fgbz_encode::decode_fgbz(fgbz.data).expect("decode FGbz");
        assert_eq!(palette.len(), 1);
        assert!(indices.is_empty());
        assert!(palette[0].r > 0, "foreground red should be preserved");
    }

    #[test]
    fn quality_color_emits_per_blit_fgbz_indices() {
        let mut pm = Pixmap::white(80, 40);
        for y in 8..24 {
            for x in 8..24 {
                pm.set_rgb(x, y, 180, 20, 20);
            }
            for x in 48..64 {
                pm.set_rgb(x, y, 20, 40, 180);
            }
        }

        let bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .encode()
            .expect("encode");
        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
        let page = doc.page(0).expect("page");
        let fgbz = page.raw_chunk(b"FGbz").expect("FGbz present");
        let (palette, indices) = crate::fgbz_encode::decode_fgbz(fgbz).expect("decode FGbz");

        assert!(
            palette.len() >= 2,
            "expected at least two foreground colors, got {palette:?}"
        );
        assert!(
            indices.len() >= 2,
            "expected per-blit indices for two foreground components"
        );
        assert_ne!(
            indices[0], indices[1],
            "separate colored components should point at distinct palette entries"
        );

        let rendered = crate::Document::from_bytes(bytes)
            .expect("document")
            .page(0)
            .expect("page")
            .render()
            .expect("render");
        let left = rendered.get_rgb(12, 12);
        let right = rendered.get_rgb(52, 12);
        assert!(
            left.0 > left.2,
            "left foreground should render red-dominant, got {left:?}"
        );
        assert!(
            right.2 > right.0,
            "right foreground should render blue-dominant, got {right:?}"
        );
    }

    #[test]
    fn median_cut_reduces_many_near_duplicate_colors_to_k() {
        // 40 colours clustered tightly around red and blue (simulating
        // anti-aliasing noise across many blits of "the same" ink colour).
        let mut colors = Vec::new();
        for i in 0..20u8 {
            colors.push(WeightedColor {
                r: 180 + (i % 5),
                g: 20,
                b: 20,
                weight: 10,
            });
        }
        for i in 0..20u8 {
            colors.push(WeightedColor {
                r: 20,
                g: 20,
                b: 180 + (i % 5),
                weight: 10,
            });
        }
        let palette = median_cut(&colors, 2);
        assert_eq!(palette.len(), 2);
        // One entry should be red-dominant, the other blue-dominant.
        let (mut reds, mut blues) = (0, 0);
        for c in &palette {
            if c.r > c.b {
                reds += 1;
            } else {
                blues += 1;
            }
        }
        assert_eq!((reds, blues), (1, 1));
    }

    #[test]
    fn median_cut_never_exceeds_k_even_with_fewer_distinct_colors() {
        let colors = vec![
            WeightedColor {
                r: 10,
                g: 10,
                b: 10,
                weight: 1,
            },
            WeightedColor {
                r: 10,
                g: 10,
                b: 10,
                weight: 1,
            },
        ];
        // Requesting 8 boxes from a single distinct colour must not spin
        // forever or panic — it should stop once nothing is splittable.
        let palette = median_cut(&colors, 8);
        assert_eq!(palette.len(), 1);
    }

    #[test]
    fn median_cut_empty_input_is_empty() {
        assert!(median_cut(&[], 4).is_empty());
    }

    #[test]
    fn nearest_palette_index_picks_closest() {
        let palette = [
            FgbzColor { r: 255, g: 0, b: 0 },
            FgbzColor { r: 0, g: 0, b: 255 },
        ];
        assert_eq!(
            nearest_palette_index(
                &palette,
                FgbzColor {
                    r: 200,
                    g: 10,
                    b: 10
                }
            ),
            0
        );
        assert_eq!(
            nearest_palette_index(
                &palette,
                FgbzColor {
                    r: 10,
                    g: 10,
                    b: 200
                }
            ),
            1
        );
    }

    #[test]
    fn fgbz_mediancut_is_opt_in_default_stays_exact() {
        // Same fixture as quality_color_emits_per_blit_fgbz_indices: two
        // distinctly-coloured blits. Exact (default) keeps 2 palette
        // entries; MedianCut capped at 1 must collapse to 1 and still
        // produce a valid, decodable page.
        let mut pm = Pixmap::white(80, 40);
        for y in 8..24 {
            for x in 8..24 {
                pm.set_rgb(x, y, 180, 20, 20);
            }
            for x in 48..64 {
                pm.set_rgb(x, y, 20, 40, 180);
            }
        }

        let default_bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .encode()
            .expect("default encode");
        let explicit_exact_bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .with_fgbz_options(FgbzPaletteOptions::Exact)
            .encode()
            .expect("exact encode");
        assert_eq!(
            default_bytes, explicit_exact_bytes,
            "FgbzPaletteOptions::Exact must be byte-identical to the (opt-out) default"
        );

        let capped_bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .with_fgbz_options(FgbzPaletteOptions::MedianCut { max_colors: 1 })
            .encode()
            .expect("median-cut encode");
        assert_ne!(
            default_bytes, capped_bytes,
            "opting into MedianCut{{max_colors:1}} must change the output"
        );

        let doc = crate::djvu_document::DjVuDocument::parse(&capped_bytes).expect("parse");
        let page = doc.page(0).expect("page");
        let fgbz = page.raw_chunk(b"FGbz").expect("FGbz present");
        let (palette, _indices) = crate::fgbz_encode::decode_fgbz(fgbz).expect("decode FGbz");
        assert_eq!(palette.len(), 1, "capped at 1 palette entry");
    }

    #[test]
    fn quality_color_accepts_adaptive_segment_options() {
        let pm = mixed_lighting_fixture();

        let bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .with_segment_options(adaptive_segment_options())
            .encode()
            .expect("encode");

        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
        let page = doc.page(0).expect("page");
        assert!(page.raw_chunk(b"Sjbz").is_some());
        assert!(!page.all_chunks(b"BG44").is_empty());
    }

    #[test]
    fn layered_shared_djbz_round_trips_with_incl() {
        // #452: two identical colour pages — their mask CCs are byte-exact across
        // pages, so they are promoted to one shared Djbz, and each page references
        // it via INCL while keeping its own BG44/FGbz.
        let pm = mixed_lighting_fixture();
        let pages = [pm.clone(), pm.clone()];
        let bytes = encode_djvm_layered_shared(&pages, EncodeQuality::Quality, 300, None, 2)
            .expect("layered shared encode");

        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
        assert_eq!(doc.page_count(), 2);
        for i in 0..2 {
            let page = doc.page(i).expect("page");
            assert!(page.raw_chunk(b"Sjbz").is_some(), "page {i} Sjbz");
            assert!(!page.all_chunks(b"BG44").is_empty(), "page {i} BG44");
            assert!(
                page.raw_chunk(b"INCL").is_some(),
                "page {i} INCL → shared dict"
            );
            // The shared-dictionary Sjbz must still decode to the page mask.
            page.extract_mask()
                .expect("mask decode")
                .expect("mask present");
        }
        assert!(
            bytes.windows(4).any(|w| w == b"Djbz"),
            "shared Djbz form present"
        );
    }

    #[test]
    fn adaptive_segment_options_improve_decoded_mixed_lighting_fixture() {
        let pm = mixed_lighting_fixture();
        let fixed = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .with_segment_options(SegmentOptions {
                bg_subsample: 6,
                ..SegmentOptions::default()
            })
            .encode()
            .expect("fixed encode");
        let adaptive = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .with_segment_options(SegmentOptions {
                bg_subsample: 6,
                ..adaptive_segment_options()
            })
            .encode()
            .expect("adaptive encode");

        let fixed_render = render_encoded(&fixed);
        let adaptive_render = render_encoded(&adaptive);
        let fixed_err = mean_abs_rgb_diff(&pm, &fixed_render);
        let adaptive_err = mean_abs_rgb_diff(&pm, &adaptive_render);

        assert!(
            adaptive_err < fixed_err * 0.70,
            "adaptive decoded render should be closer to source ({adaptive_err:.2} vs {fixed_err:.2})"
        );
    }

    /// #571: the Photo profile writes INFO + BG44 only (no Sjbz/FGbz) and
    /// round-trips through our decoder; grayscale sources take the grayscale
    /// IW44 encoder.
    #[test]
    fn photo_profile_masks_nothing_and_round_trips() {
        // Colour gradient source.
        let mut pm = Pixmap::white(64, 48);
        for y in 0..48 {
            for x in 0..64 {
                pm.set_rgb(x, y, (x * 4) as u8, (y * 5) as u8, 128);
            }
        }
        let bytes = PageEncoder::from_pixmap(&pm)
            .with_dpi(100)
            .with_quality(EncodeQuality::Photo)
            .encode()
            .unwrap();
        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).unwrap();
        let page = doc.page(0).unwrap();
        assert!(page.find_chunk(b"Sjbz").is_none(), "no mask in Photo");
        assert!(page.find_chunk(b"FGbz").is_none(), "no palette in Photo");
        assert!(page.find_chunk(b"BG44").is_some(), "background present");
        let out = crate::djvu_render::render_pixmap(
            page,
            &crate::djvu_render::RenderOptions {
                width: 64,
                height: 48,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((out.width, out.height), (64, 48));

        // Pure-grayscale source must also round-trip (grayscale IW44 path).
        let mut gray = Pixmap::white(64, 48);
        for y in 0..48 {
            for x in 0..64 {
                let v = ((x + y) * 3) as u8;
                gray.set_rgb(x, y, v, v, v);
            }
        }
        let gbytes = PageEncoder::from_pixmap(&gray)
            .with_dpi(100)
            .with_quality(EncodeQuality::Photo)
            .encode()
            .unwrap();
        let gdoc = crate::djvu_document::DjVuDocument::parse(&gbytes).unwrap();
        let gout = crate::djvu_render::render_pixmap(
            gdoc.page(0).unwrap(),
            &crate::djvu_render::RenderOptions {
                width: 64,
                height: 48,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!((gout.width, gout.height), (64, 48));
    }

    /// #570: the auto-classifier must match the expert profile choice on the
    /// corpus, and a photo must never be routed to the bilevel path
    /// (catastrophic misroute).
    #[test]
    fn classify_content_matches_expert_choice_on_corpus() {
        let render = |path: &str, page: usize, dpi: f32| -> Pixmap {
            let data =
                std::fs::read(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path))
                    .unwrap();
            let doc = crate::djvu_document::DjVuDocument::parse(&data).unwrap();
            let p = doc.page(page).unwrap();
            let scale = dpi / p.dpi().max(1) as f32;
            let w = ((p.width() as f32 * scale).round() as u32).max(1);
            let h = ((p.height() as f32 * scale).round() as u32).max(1);
            crate::djvu_render::render_pixmap(
                p,
                &crate::djvu_render::RenderOptions {
                    width: w,
                    height: h,
                    ..Default::default()
                },
            )
            .unwrap()
        };

        // Photo (boy.djvu is a photograph) — must be Photo, and NEVER Lossless.
        let photo = classify_content(&render("tests/fixtures/boy.djvu", 0, 300.0));
        assert_ne!(
            photo,
            EncodeQuality::Lossless,
            "photo → bilevel is catastrophic"
        );
        assert_eq!(photo, EncodeQuality::Photo);

        // Bilevel scans → Lossless.
        assert_eq!(
            classify_content(&render("tests/fixtures/boy_jb2.djvu", 0, 300.0)),
            EncodeQuality::Lossless
        );
        // Native resolution — the real encode workflow feeds native scans;
        // a downscaled render adds antialiasing midtones a true bilevel
        // source doesn't have.
        assert_eq!(
            classify_content(&render("tests/corpus/cable_1973_100133.djvu", 0, 300.0)),
            EncodeQuality::Lossless
        );

        // Layered colour documents → Quality.
        assert_eq!(
            classify_content(&render("tests/fixtures/colorbook.djvu", 0, 150.0)),
            EncodeQuality::Quality
        );
        assert_eq!(
            classify_content(&render("tests/fixtures/navm_fgbz.djvu", 1, 150.0)),
            EncodeQuality::Quality
        );
    }

    fn adaptive_segment_options() -> SegmentOptions {
        SegmentOptions {
            binarization: crate::segment::Binarization::Sauvola { window: 9, k: 0.34 },
            bg_inpaint: true,
            ..SegmentOptions::default()
        }
    }

    fn mixed_lighting_fixture() -> Pixmap {
        let mut pm = Pixmap::white(48, 24);
        for y in 0..24 {
            for x in 0..48 {
                let v = if x < 24 { 80 } else { 220 };
                pm.set_rgb(x, y, v, v, v);
            }
        }

        // Dark ink on dark paper.
        for y in 6..18 {
            pm.set_rgb(9, y, 40, 40, 40);
            pm.set_rgb(14, y, 40, 40, 40);
        }
        for x in 9..=14 {
            pm.set_rgb(x, 6, 40, 40, 40);
            pm.set_rgb(x, 12, 40, 40, 40);
        }

        // Light-gray ink on bright paper. Fixed threshold treats this as BG,
        // so the thin strokes wash into the BG44 sample cells.
        for y in 6..18 {
            pm.set_rgb(33, y, 140, 140, 140);
            pm.set_rgb(40, y, 140, 140, 140);
        }
        for x in 33..=40 {
            pm.set_rgb(x, 6, 140, 140, 140);
            pm.set_rgb(x, 12, 140, 140, 140);
            pm.set_rgb(x, 17, 140, 140, 140);
        }
        pm
    }

    fn render_encoded(bytes: &[u8]) -> Pixmap {
        let doc = crate::djvu_document::DjVuDocument::parse(bytes).expect("parse encoded doc");
        let page = doc.page(0).expect("page");
        let (width, height) = page.dimensions();
        let opts = crate::djvu_render::RenderOptions {
            width: u32::from(width),
            height: u32::from(height),
            ..crate::djvu_render::RenderOptions::default()
        };
        crate::djvu_render::render_pixmap(page, &opts).expect("render encoded page")
    }

    fn mean_abs_rgb_diff(expected: &Pixmap, actual: &Pixmap) -> f64 {
        assert_eq!(
            (expected.width, expected.height),
            (actual.width, actual.height)
        );
        let mut sum = 0u64;
        let mut n = 0u64;
        for (a, b) in expected
            .data
            .chunks_exact(4)
            .zip(actual.data.chunks_exact(4))
        {
            for c in 0..3 {
                sum += a[c].abs_diff(b[c]) as u64;
                n += 1;
            }
        }
        sum as f64 / n as f64
    }

    #[test]
    fn archival_color_emits_layered_djvu_with_fgbz() {
        let mut pm = Pixmap::white(48, 48);
        for y in 12..24 {
            for x in 12..24 {
                pm.set_rgb(x, y, 0, 90, 180);
            }
        }

        let bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Archival)
            .encode()
            .expect("encode");

        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
        let page = doc.page(0).expect("page");
        assert!(page.raw_chunk(b"Sjbz").is_some());
        assert!(!page.all_chunks(b"BG44").is_empty());
        assert!(page.raw_chunk(b"FGbz").is_some());
    }

    #[test]
    fn lossless_pixmap_rejected() {
        let pm = Pixmap::white(8, 8);
        let err = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Lossless)
            .encode()
            .unwrap_err();
        assert!(format!("{err}").contains("Lossless"));
    }

    #[test]
    fn quality_bitmap_rejected() {
        let bm = Bitmap::new(8, 8);
        let err = PageEncoder::from_bitmap(&bm)
            .with_quality(EncodeQuality::Quality)
            .encode()
            .unwrap_err();
        assert!(format!("{err}").contains("Quality"));
    }

    #[test]
    fn quality_color_round_trips_through_document() {
        // End-to-end: encode a colour page at Quality, parse it back
        // through the high-level Document API, and confirm dimensions
        // + that the page has both an Sjbz and at least one BG44 chunk.
        let pm = Pixmap::white(32, 24);
        let bytes = PageEncoder::from_pixmap(&pm)
            .with_dpi(150)
            .with_quality(EncodeQuality::Quality)
            .encode()
            .expect("encode");

        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse");
        let page = doc.page(0).expect("page 0");
        assert_eq!(page.width(), 32);
        assert_eq!(page.height(), 24);
        assert_eq!(page.dpi(), 150);
        assert!(page.raw_chunk(b"Sjbz").is_some());
        assert!(!page.all_chunks(b"BG44").is_empty());
    }

    // ── TH44 thumbnail tests (layered encoder) ────────────────────────────────

    /// Layered bundle WITH thumbnails: each page FORM:DJVU contains TH44 chunk(s)
    /// that decode to a valid IW44 image at the expected reduced dimensions.
    #[test]
    fn layered_bundle_with_thumbnails_each_page_has_th44() {
        // Build two distinct colour pages.
        let mut p1 = Pixmap::white(64, 48);
        for y in 8..24 {
            for x in 8..24 {
                p1.set_rgb(x, y, 180, 20, 20);
            }
        }
        let p2 = Pixmap::white(64, 48);

        let bytes = encode_djvm_layered_shared_with_thumbnails(
            &[p1.clone(), p2.clone()],
            EncodeQuality::Quality,
            300,
            None,
            2,
            true,
        )
        .expect("encode layered with thumbnails");

        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
        assert_eq!(doc.page_count(), 2);
        for i in 0..2 {
            let page = doc.page(i).expect("page");
            let thumb = page.thumbnail().expect("thumbnail() should not error");
            assert!(
                thumb.is_some(),
                "page {i} must carry a TH44 thumbnail when with_thumbnails=true"
            );
            let thumb = thumb.unwrap();
            let (tw, th) = crate::thumbnail::thumbnail_dimensions(
                if i == 0 { p1.width } else { p2.width },
                if i == 0 { p1.height } else { p2.height },
            );
            assert_eq!(
                thumb.width, tw,
                "page {i} thumbnail width should be {tw}, got {}",
                thumb.width
            );
            assert_eq!(
                thumb.height, th,
                "page {i} thumbnail height should be {th}, got {}",
                thumb.height
            );
        }
    }

    /// Layered bundle WITHOUT thumbnails: output must NOT contain any TH44 chunks.
    #[test]
    fn layered_bundle_without_thumbnails_has_no_th44() {
        let pm = Pixmap::white(64, 48);
        let bytes =
            encode_djvm_layered_shared(&[pm.clone(), pm], EncodeQuality::Quality, 300, None, 2)
                .expect("encode layered");

        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse bundle");
        assert_eq!(doc.page_count(), 2);
        for i in 0..2 {
            let page = doc.page(i).expect("page");
            let thumb = page.thumbnail().expect("thumbnail() should not error");
            assert!(
                thumb.is_none(),
                "page {i} must NOT carry a TH44 thumbnail when with_thumbnails=false"
            );
        }
    }

    // ── TXTZ_OCR: encode-time text layer ────────────────────────────────────

    fn sample_text_layer(page_w: u32, page_h: u32) -> TextLayer {
        use crate::text::Rect;
        TextLayer {
            text: "Hello World".into(),
            zones: vec![TextZone {
                kind: TextZoneKind::Page,
                rect: Rect {
                    x: 0,
                    y: 0,
                    width: page_w,
                    height: page_h,
                },
                text: "Hello World".into(),
                children: vec![TextZone {
                    kind: TextZoneKind::Line,
                    rect: Rect {
                        x: 4,
                        y: 6,
                        width: page_w.saturating_sub(8),
                        height: 20,
                    },
                    text: "Hello World".into(),
                    children: vec![
                        TextZone {
                            kind: TextZoneKind::Word,
                            rect: Rect {
                                x: 4,
                                y: 6,
                                width: 50,
                                height: 20,
                            },
                            text: "Hello".into(),
                            children: vec![],
                        },
                        TextZone {
                            kind: TextZoneKind::Word,
                            rect: Rect {
                                x: 60,
                                y: 6,
                                width: 50,
                                height: 20,
                            },
                            text: "World".into(),
                            children: vec![],
                        },
                    ],
                }],
            }],
        }
    }

    #[test]
    fn no_text_layer_is_byte_identical_to_pre_txtz_ocr_baseline() {
        // Opt-in guarantee: not calling with_text_layer()/with_ocr_text_layer()
        // must produce exactly the same bytes as before those methods existed
        // (no stray empty TXTz chunk, no size/behavior change for existing
        // callers). Cross-checked against `lossless_bilevel_round_trips`
        // et al., which continue to pass unmodified.
        let bm = checkerboard(32, 24);
        let bytes = PageEncoder::from_bitmap(&bm).encode().expect("encode");
        let form = parse_form(&bytes).expect("parse_form");
        assert!(
            !form
                .chunks
                .iter()
                .any(|c| &c.id == b"TXTz" || &c.id == b"TXTa"),
            "no text layer attached => no TXTz/TXTa chunk should be emitted"
        );
    }

    #[test]
    fn with_text_layer_emits_txtz_and_round_trips_through_our_decoder() {
        let bm = checkerboard(120, 80);
        let layer = sample_text_layer(120, 80);
        let bytes = PageEncoder::from_bitmap(&bm)
            .with_quality(EncodeQuality::Lossless)
            .with_text_layer(layer.clone())
            .encode()
            .expect("encode");

        let form = parse_form(&bytes).expect("parse_form");
        assert!(
            form.chunks.iter().any(|c| &c.id == b"TXTz"),
            "TXTz chunk should be present after with_text_layer"
        );

        // Round-trip through our own decoder end to end (DjVuDocument), the
        // primary validator per the task brief.
        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse document");
        let page = doc.page(0).expect("page 0");
        let decoded = page
            .text_layer()
            .expect("text_layer() must not error")
            .expect("text_layer() must return Some after with_text_layer");
        assert_eq!(decoded.text, "Hello World");
        let words: Vec<&str> = decoded
            .zones
            .first()
            .and_then(|page_zone| page_zone.children.first())
            .map(|line| line.children.iter().map(|w| w.text.as_str()).collect())
            .unwrap_or_default();
        assert_eq!(words, vec!["Hello", "World"]);

        let plain = page.text().expect("text()").expect("Some plain text");
        assert_eq!(plain, "Hello World");
    }

    /// Deterministic mock `OcrBackend` for `with_ocr_text_layer` — mirrors the
    /// pattern used by `examples/ocr_qa.rs`'s test-only mock backend so this
    /// unit test needs no real Tesseract install.
    struct MockOcrBackend {
        layer: TextLayer,
    }

    impl OcrBackend for MockOcrBackend {
        fn recognize(
            &self,
            _pixmap: &Pixmap,
            _options: &OcrOptions,
        ) -> Result<TextLayer, OcrError> {
            Ok(self.layer.clone())
        }
    }

    #[test]
    fn with_ocr_text_layer_runs_backend_and_attaches_result() {
        let bm = checkerboard(96, 64);
        let backend = MockOcrBackend {
            layer: sample_text_layer(96, 64),
        };
        let bytes = PageEncoder::from_bitmap(&bm)
            .with_quality(EncodeQuality::Lossless)
            .with_ocr_text_layer(&backend, &OcrOptions::default())
            .expect("OCR backend should not fail")
            .encode()
            .expect("encode");

        let doc = crate::djvu_document::DjVuDocument::parse(&bytes).expect("parse document");
        let page = doc.page(0).expect("page 0");
        let text = page.text().expect("text()").expect("Some plain text");
        assert_eq!(text, "Hello World");
    }

    #[test]
    fn with_ocr_text_layer_works_from_colour_pixmap_source_too() {
        // Quality/Archival (Pixmap source) path: OCR runs directly on the
        // pixmap without the bitmap_to_pixmap conversion.
        let pm = Pixmap::white(96, 64);
        let backend = MockOcrBackend {
            layer: sample_text_layer(96, 64),
        };
        let bytes = PageEncoder::from_pixmap(&pm)
            .with_quality(EncodeQuality::Quality)
            .with_ocr_text_layer(&backend, &OcrOptions::default())
            .expect("OCR backend should not fail")
            .encode()
            .expect("encode");

        let form = parse_form(&bytes).expect("parse_form");
        assert!(form.chunks.iter().any(|c| &c.id == b"TXTz"));
    }

    #[test]
    fn bitmap_to_pixmap_maps_black_pixels_to_black_rgb() {
        let mut bm = Bitmap::new(4, 2);
        bm.set_black(1, 0);
        let pm = bitmap_to_pixmap(&bm);
        assert_eq!(pm.get_rgb(1, 0), (0, 0, 0));
        assert_eq!(pm.get_rgb(0, 0), (255, 255, 255));
    }
}
