//! `djvu encode`: images, image directories and TIFF to DjVu.

use super::*;

/// Wraps a page-ingestion error's rendered message so it satisfies
/// `std::error::Error + Send + Sync + 'static`, the bound
/// `encode_djvm_layered_shared_streaming`'s page-source closure requires.
///
/// Carries only the original error's `Display` text, not the error value
/// itself: `png_io`'s decode errors (from the `png`/`zune-jpeg`/`tiff`
/// crates, boxed as `Box<dyn std::error::Error>`) aren't guaranteed
/// `Send + Sync`, but that rendered message is all the CLI ever showed the
/// user for a failed page anyway (see `describe_layered_encode_error`,
/// below, for how it's unwrapped back out on the way to the user).
#[derive(Debug)]
pub(super) struct PageDecodeError(String);

impl std::fmt::Display for PageDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PageDecodeError {}

/// Format an [`djvu_rs::djvu_encode::EncodeError`] from the streaming entry
/// point the way the eager per-path decode loop it replaces reported page
/// failures: [`EncodeError::PageSource`](djvu_rs::djvu_encode::EncodeError::PageSource)
/// (a page that failed to decode) surfaces the original ingestion message
/// verbatim, with no extra prefix — that decode error used to propagate
/// straight out of the eager loop via `?`, before any encode call existed to
/// prefix it. Any other `EncodeError` keeps the "layered encode: " prefix
/// the eager entry points' own error already used.
pub(super) fn describe_layered_encode_error(e: djvu_rs::djvu_encode::EncodeError) -> String {
    match e {
        djvu_rs::djvu_encode::EncodeError::PageSource(inner) => inner.to_string(),
        other => format!("layered encode: {other}"),
    }
}

pub(super) fn cmd_encode(
    input: &Path,
    output: &Path,
    dpi: Option<u16>,
    profile_args: EncodeProfileArgs,
    segment_args: EncodeSegmentArgs,
    bg_bpp: Option<f32>,
    bundle_args: EncodeBundleArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    // #694: without an explicit --dpi, a TIFF input's X/YResolution tags set
    // the INFO dpi; everything else keeps the historical 300 default.
    #[cfg(feature = "tiff")]
    let dpi = match dpi {
        Some(d) => d,
        None => {
            let tag_dpi = if input.is_file() && input_is_tiff(input) {
                djvu_rs::png_io::tiff_file_dpi(input)?
            } else {
                None
            };
            match tag_dpi {
                Some(d) => {
                    eprintln!("dpi {d} (from TIFF resolution tags)");
                    d
                }
                None => 300,
            }
        }
    };
    #[cfg(not(feature = "tiff"))]
    let dpi = dpi.unwrap_or(300);
    let EncodeProfileArgs {
        quality,
        bilevel_codec,
        background,
        icc,
    } = profile_args;
    // #694: --background composites transparent PNG/TIFF pixels onto a solid
    // colour at decode time (the default preserves the alpha channel);
    // --icc reject refuses ICC-profiled input instead of dropping the profile.
    let policy = djvu_rs::ingest::IngestPolicy {
        alpha: match background {
            Some((red, green, blue)) => {
                djvu_rs::ingest::AlphaCompositing::CompositeOnBackground { red, green, blue }
            }
            None => djvu_rs::ingest::AlphaCompositing::Preserve,
        },
        icc: match icc {
            IccArg::Ignore => djvu_rs::ingest::IccHandling::Ignore,
            IccArg::Reject => djvu_rs::ingest::IccHandling::Reject,
        },
        ..Default::default()
    };
    let EncodeBundleArgs {
        shared_dict_pages,
        thumbnails,
    } = bundle_args;
    use djvu_rs::djvu_encode::{BilevelCodec, EncodeQuality, PageEncoder};
    use djvu_rs::iw44_encode::{Iw44EncodeOptions, Iw44Target};
    use djvu_rs::jb2_encode::encode_djvm_bundle_jb2;
    use djvu_rs::segment::{SegmentOptions, segment_page};

    let q = match quality {
        EncodeQualityArg::Lossless => EncodeQuality::Lossless,
        EncodeQualityArg::Quality | EncodeQualityArg::Auto => EncodeQuality::Quality,
        EncodeQualityArg::Archival => EncodeQuality::Archival,
        EncodeQualityArg::Photo => EncodeQuality::Photo,
    };
    let segment_options = segment_args.to_options(q)?;

    if input.is_dir() && bilevel_codec != BilevelCodecArg::Jb2 {
        return Err("--bilevel-codec smmr is supported only for single-image input".into());
    }

    if input.is_dir() {
        let entries = directory_image_entries(input)?;

        // --quality auto on a directory (#570): classify every page; the
        // bundle writer supports lossless-bilevel or layered bundles, so the
        // decision is bundle-wide — all pages bilevel → Lossless, anything
        // else → Quality (a Photo-classified page inside a bundle also goes
        // layered; per-page mixed bundles are the recorded follow-up).
        let quality = if matches!(quality, EncodeQualityArg::Auto) {
            let mut all_bilevel = true;
            for path in &entries {
                let pm = djvu_rs::png_io::decode_image_to_pixmap_with_policy(path, policy)?;
                if djvu_rs::djvu_encode::classify_content(&pm)
                    != djvu_rs::djvu_encode::EncodeQuality::Lossless
                {
                    all_bilevel = false;
                    break;
                }
            }
            let picked = if all_bilevel {
                EncodeQualityArg::Lossless
            } else {
                EncodeQualityArg::Quality
            };
            eprintln!("auto profile (bundle): {picked:?}");
            picked
        } else {
            quality
        };

        if matches!(quality, EncodeQualityArg::Lossless) {
            if thumbnails {
                eprintln!("--thumbnails is ignored for lossless (JB2-only) bundles");
            }
            let mut masks = Vec::with_capacity(entries.len());
            for path in &entries {
                let pixmap = djvu_rs::png_io::decode_image_to_pixmap_with_policy(path, policy)?;
                let seg = segment_page(&pixmap, &SegmentOptions::default());
                masks.push(seg.mask);
            }
            let bytes = encode_djvm_bundle_jb2(&masks, shared_dict_pages, dpi);
            std::fs::write(output, &bytes)?;
            eprintln!(
                "{} pages → {} ({} bytes, shared-dict threshold = {})",
                entries.len(),
                output.display(),
                bytes.len(),
                shared_dict_pages,
            );
            return Ok(());
        }

        // #452: layered multi-page now shares a Djbz dictionary across pages,
        // honoring --shared-dict-pages (was: per-page independent masks).
        //
        // Step 5 (encoder peak-memory plan): pages decode lazily, one window
        // at a time, through the streaming entry point instead of building
        // the whole `Vec<Pixmap>` up front — a directory of scanned pages is
        // exactly the case that plan's RSS-scaling numbers targeted.
        let bytes = djvu_rs::djvu_encode::encode_djvm_layered_shared_streaming(
            entries.len(),
            |idx| {
                djvu_rs::png_io::decode_image_to_pixmap_with_policy(&entries[idx], policy)
                    .map_err(|e| PageDecodeError(e.to_string()))
            },
            q,
            dpi,
            segment_options,
            shared_dict_pages,
            thumbnails,
            None,
            None,
        )
        .map_err(describe_layered_encode_error)?;
        std::fs::write(output, &bytes)?;
        eprintln!(
            "{} pages → {} ({} bytes, layered {:?}, shared-dict threshold = {}, thumbnails = {})",
            entries.len(),
            output.display(),
            bytes.len(),
            q,
            shared_dict_pages,
            thumbnails,
        );
        return Ok(());
    }

    // #694: bilevel TIFF fast path — 1-bit pages decode straight to packed
    // Bitmap masks, skipping RGBA expansion and segmentation. A 1-bit TIFF is
    // bilevel by construction, so --quality auto resolves to Lossless without
    // pixel statistics.
    #[cfg(feature = "tiff")]
    if input_is_tiff(input)
        && matches!(quality, EncodeQualityArg::Auto | EncodeQualityArg::Lossless)
        && let Some(bitmaps) = djvu_rs::png_io::decode_tiff_file_to_bitmaps(input, policy)?
    {
        if matches!(quality, EncodeQualityArg::Auto) {
            eprintln!("auto profile: Lossless (1-bit TIFF)");
        }
        if bitmaps.len() > 1 {
            if bilevel_codec != BilevelCodecArg::Jb2 {
                return Err("--bilevel-codec smmr is supported only for single-image input".into());
            }
            if thumbnails {
                eprintln!("--thumbnails is ignored for lossless (JB2-only) bundles");
            }
            let bytes = encode_djvm_bundle_jb2(&bitmaps, shared_dict_pages, dpi);
            std::fs::write(output, &bytes)?;
            eprintln!(
                "{} ({} TIFF pages) → {} ({} bytes, shared-dict threshold = {})",
                input.display(),
                bitmaps.len(),
                output.display(),
                bytes.len(),
                shared_dict_pages,
            );
            return Ok(());
        }
        if thumbnails {
            eprintln!("--thumbnails applies to multi-page bundles only — ignored");
        }
        let codec = match bilevel_codec {
            BilevelCodecArg::Jb2 => BilevelCodec::Jb2,
            BilevelCodecArg::Smmr => BilevelCodec::Smmr,
        };
        let bm = &bitmaps[0];
        let bytes = PageEncoder::from_bitmap(bm)
            .with_dpi(dpi)
            .with_quality(EncodeQuality::Lossless)
            .with_bilevel_codec(codec)
            .encode()
            .map_err(|e| format!("encode: {e}"))?;
        std::fs::write(output, &bytes)?;
        eprintln!(
            "{} → {} ({}×{} px, {} bytes)",
            input.display(),
            output.display(),
            bm.width,
            bm.height,
            bytes.len(),
        );
        return Ok(());
    }

    // #694 slice 2: a multipage TIFF file maps to a multi-page bundle — one
    // DjVu page per TIFF page (IFD), in stored order, same bundle rules as a
    // directory input.
    //
    // Step 5 (encoder peak-memory plan): learn the page count from a cheap
    // IFD-only pass (no pixel decode) instead of eagerly decoding every page
    // just to check `len() > 1` — the actual pixel decoding happens lazily,
    // one page at a time, in `encode_tiff_page_bundle` / below.
    //
    // The file's bytes are read into `tiff_bytes` exactly once here (not
    // once per `LazyTiffPages` pass): `count_pages` and every
    // `LazyTiffPages` reader `encode_tiff_page_bundle` opens borrow the same
    // `Vec<u8>`, which this function keeps alive on the stack for the whole
    // encode and frees on return — no leak, unlike an earlier version of
    // this change.
    #[cfg(feature = "tiff")]
    let tiff_bytes: Option<Vec<u8>> = if input_is_tiff(input) {
        Some(std::fs::read(input).map_err(|e| format!("{}: {e}", input.display()))?)
    } else {
        None
    };
    #[cfg(feature = "tiff")]
    let tiff_page_count: Option<usize> = match &tiff_bytes {
        Some(bytes) => Some(djvu_rs::png_io::tiff_file_page_count(bytes, input)?),
        None => None,
    };
    #[cfg(feature = "tiff")]
    if tiff_page_count.is_some_and(|n| n > 1) {
        if bilevel_codec != BilevelCodecArg::Jb2 {
            return Err("--bilevel-codec smmr is supported only for single-image input".into());
        }
        return encode_tiff_page_bundle(
            tiff_bytes.as_deref().unwrap(),
            input,
            output,
            dpi,
            quality,
            q,
            segment_options,
            shared_dict_pages,
            thumbnails,
            policy,
            tiff_page_count.unwrap(),
        );
    }

    if thumbnails {
        eprintln!("--thumbnails applies to multi-page bundles only — ignored");
    }
    #[cfg(feature = "tiff")]
    let pixmap = match tiff_page_count {
        Some(_) => djvu_rs::png_io::decode_tiff_file_to_pixmap_with_policy(input, policy)?,
        None => djvu_rs::png_io::decode_image_to_pixmap_with_policy(input, policy)?,
    };
    #[cfg(not(feature = "tiff"))]
    let pixmap = djvu_rs::png_io::decode_image_to_pixmap_with_policy(input, policy)?;

    // --quality auto (#570): pick the profile from cheap pixel statistics.
    let q = if matches!(quality, EncodeQualityArg::Auto) {
        let detected = djvu_rs::djvu_encode::classify_content(&pixmap);
        eprintln!("auto profile: {detected:?}");
        detected
    } else {
        q
    };

    let bytes = match q {
        EncodeQuality::Lossless => {
            let seg = segment_page(&pixmap, &SegmentOptions::default());
            let codec = match bilevel_codec {
                BilevelCodecArg::Jb2 => BilevelCodec::Jb2,
                BilevelCodecArg::Smmr => BilevelCodec::Smmr,
            };
            PageEncoder::from_bitmap(&seg.mask)
                .with_dpi(dpi)
                .with_quality(EncodeQuality::Lossless)
                .with_bilevel_codec(codec)
                .encode()
        }
        EncodeQuality::Quality | EncodeQuality::Archival | EncodeQuality::Photo => {
            let mut encoder = PageEncoder::from_pixmap(&pixmap)
                .with_dpi(dpi)
                .with_quality(q);
            if let Some(opts) = segment_options {
                encoder = encoder.with_segment_options(opts);
            }
            if let Some(bpp) = bg_bpp {
                let iw44_opts = Iw44EncodeOptions {
                    target: Iw44Target::Bpp(bpp),
                    ..Iw44EncodeOptions::default()
                };
                encoder = encoder.with_iw44_options(iw44_opts);
            }
            encoder.encode()
        }
    }
    .map_err(|e| format!("encode: {e}"))?;

    std::fs::write(output, &bytes)?;
    eprintln!(
        "{} → {} ({}×{} px, {} bytes)",
        input.display(),
        output.display(),
        pixmap.width,
        pixmap.height,
        bytes.len(),
    );
    Ok(())
}

/// True when `path` looks like a TIFF input: `.tif`/`.tiff` extension, or a
/// TIFF magic header for extension-less paths (mirrors `decode_image_to_pixmap`).
#[cfg(feature = "tiff")]
pub(super) fn input_is_tiff(path: &Path) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    match ext.as_deref() {
        Some("tif") | Some("tiff") => return true,
        Some("png") | Some("jpg") | Some("jpeg") => return false,
        _ => {}
    }
    let mut header = [0u8; 4];
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    use std::io::Read;
    if f.read_exact(&mut header).is_err() {
        return false;
    }
    header.starts_with(b"II\x2A\x00") || header.starts_with(b"MM\x00\x2A")
}

/// Encode a multipage TIFF file's pages as a multi-page bundle, mirroring
/// the directory-input rules of `cmd_encode` (auto classification, lossless
/// JB2 bundle, or layered bundle with a shared dictionary).
///
/// Step 5 (encoder peak-memory plan): pages decode lazily via
/// [`djvu_rs::png_io::LazyTiffPages`] instead of the caller holding every
/// page's `Pixmap` in one `Vec` — `page_count` is already known (a cheap
/// IFD-only pass; see the call site) so this never needs to buffer more than
/// the current page.
///
/// `--quality auto` reads every page once to classify, then (for a lossless
/// or layered bundle) a second `LazyTiffPages` pass re-reads the file for the
/// actual encode — `LazyTiffPages` is strictly forward-only and cannot
/// rewind, so a fresh reader is opened per pass rather than trying to buffer
/// pages across two consumers. Unlike the directory-input path (which reused
/// its one already-decoded `Pixmap` per page for both classification and
/// encoding before this step, and still does), **this is a genuine second
/// full pixel decode of every page for TIFF input specifically** — the
/// eager `Vec<Pixmap>` this step replaced decoded each TIFF page once and
/// reused it for both passes. Measured worst case (an all-bilevel multipage
/// TIFF, where the classify loop cannot break early and both passes run to
/// completion): 8 pages 649.8ms → 651.0ms (+0.18%), 24 pages 1.948s → 1.948s
/// (~0%) — within measurement noise both times, because JB2 mask encoding
/// dominates total time far more than TIFF decode does. Accepted as a
/// deliberate trade, not fixed: TIFF `-q auto`'s doubled decode is real but
/// currently unmeasurable in wall clock, against a 94%-plus memory win: see
/// `PERF_EXPERIMENTS.md`'s "Stream pages in the CLI encoder" entry.
///
/// All readers borrow the same `file_bytes` (read once by the caller), so
/// re-opening a reader for a second pass costs nothing beyond re-parsing the
/// TIFF header (the IFD directory), not another disk
/// read.
#[cfg(feature = "tiff")]
#[allow(clippy::too_many_arguments)]
pub(super) fn encode_tiff_page_bundle(
    file_bytes: &[u8],
    input: &Path,
    output: &Path,
    dpi: u16,
    quality: EncodeQualityArg,
    q: djvu_rs::djvu_encode::EncodeQuality,
    segment_options: Option<djvu_rs::segment::SegmentOptions>,
    shared_dict_pages: usize,
    thumbnails: bool,
    policy: djvu_rs::ingest::IngestPolicy,
    page_count: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    use djvu_rs::jb2_encode::encode_djvm_bundle_jb2;
    use djvu_rs::png_io::LazyTiffPages;
    use djvu_rs::segment::{SegmentOptions, segment_page};

    let quality = if matches!(quality, EncodeQualityArg::Auto) {
        let mut reader = LazyTiffPages::new(file_bytes, input, policy)?;
        let mut all_bilevel = true;
        for _ in 0..page_count {
            let pm = reader.next_page()?;
            if djvu_rs::djvu_encode::classify_content(&pm)
                != djvu_rs::djvu_encode::EncodeQuality::Lossless
            {
                all_bilevel = false;
                break;
            }
        }
        let picked = if all_bilevel {
            EncodeQualityArg::Lossless
        } else {
            EncodeQualityArg::Quality
        };
        eprintln!("auto profile (bundle): {picked:?}");
        picked
    } else {
        quality
    };

    let bytes = if matches!(quality, EncodeQualityArg::Lossless) {
        if thumbnails {
            eprintln!("--thumbnails is ignored for lossless (JB2-only) bundles");
        }
        let mut reader = LazyTiffPages::new(file_bytes, input, policy)?;
        let mut masks = Vec::with_capacity(page_count);
        for _ in 0..page_count {
            let pm = reader.next_page()?;
            masks.push(segment_page(&pm, &SegmentOptions::default()).mask);
        }
        encode_djvm_bundle_jb2(&masks, shared_dict_pages, dpi)
    } else {
        let mut reader = LazyTiffPages::new(file_bytes, input, policy)?;
        djvu_rs::djvu_encode::encode_djvm_layered_shared_streaming(
            page_count,
            |_idx| {
                reader
                    .next_page()
                    .map_err(|e| PageDecodeError(e.to_string()))
            },
            q,
            dpi,
            segment_options,
            shared_dict_pages,
            thumbnails,
            None,
            None,
        )
        .map_err(describe_layered_encode_error)?
    };
    std::fs::write(output, &bytes)?;
    eprintln!(
        "{} ({page_count} TIFF pages) → {} ({} bytes, shared-dict threshold = {})",
        input.display(),
        output.display(),
        bytes.len(),
        shared_dict_pages,
    );
    Ok(())
}

/// Multi-page-bundle options of `djvu encode` (single-page paths ignore them).
#[derive(Clone, Copy)]
pub(super) struct EncodeBundleArgs {
    pub(super) shared_dict_pages: usize,
    pub(super) thumbnails: bool,
}

#[derive(Clone)]
pub(super) struct EncodeProfileArgs {
    pub(super) quality: EncodeQualityArg,
    pub(super) bilevel_codec: BilevelCodecArg,
    pub(super) background: Option<(u8, u8, u8)>,
    pub(super) icc: IccArg,
}

/// Parse `--background`: `RRGGBB` hex with optional `#`, or `white`/`black`.
pub(super) fn parse_background_color(s: &str) -> Result<(u8, u8, u8), String> {
    match s.to_ascii_lowercase().as_str() {
        "white" => return Ok((255, 255, 255)),
        "black" => return Ok((0, 0, 0)),
        _ => {}
    }
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6
        && let Ok(v) = u32::from_str_radix(hex, 16)
    {
        return Ok((
            ((v >> 16) & 0xFF) as u8,
            ((v >> 8) & 0xFF) as u8,
            (v & 0xFF) as u8,
        ));
    }
    Err(format!(
        "invalid colour '{s}' (expected RRGGBB hex, 'white', or 'black')"
    ))
}

#[derive(Clone, Copy)]
pub(super) struct EncodeSegmentArgs {
    pub(super) binarization: BinarizationArg,
    pub(super) sauvola_window: u32,
    pub(super) sauvola_k: f32,
    pub(super) bg_inpaint: bool,
    pub(super) block_classify: bool,
    pub(super) adaptive_bg_subsample: bool,
}

impl EncodeSegmentArgs {
    pub(super) fn to_options(
        self,
        quality: djvu_rs::djvu_encode::EncodeQuality,
    ) -> Result<Option<djvu_rs::segment::SegmentOptions>, Box<dyn std::error::Error>> {
        use djvu_rs::djvu_encode::EncodeQuality;
        use djvu_rs::segment::Binarization;

        let has_segment_flags = self.binarization != BinarizationArg::Fixed
            || self.bg_inpaint
            || self.block_classify
            || self.adaptive_bg_subsample;
        if !has_segment_flags {
            return Ok(None);
        }
        if matches!(quality, EncodeQuality::Lossless) {
            return Err("--binarization, --bg-inpaint, --block-classify and \
                 --adaptive-bg-subsample require --quality quality or --quality archival"
                .into());
        }

        let mut opts = quality.default_segment_options();
        opts.binarization = match self.binarization {
            BinarizationArg::Fixed => Binarization::Fixed,
            BinarizationArg::Sauvola => Binarization::Sauvola {
                window: self.sauvola_window,
                k: self.sauvola_k,
            },
        };
        opts.bg_inpaint = self.bg_inpaint;
        opts.block_classify = self.block_classify;
        opts.adaptive_bg_subsample = self.adaptive_bg_subsample;
        if self.bg_inpaint {
            // `--bg-inpaint` explicitly selects the ring-average fill, so turn
            // off the colour profile's default harmonic diffusion (which would
            // otherwise take precedence and make the flag a no-op).
            opts.bg_diffuse = false;
        }
        Ok(Some(opts))
    }
}

pub(super) fn directory_image_entries(
    dir: &Path,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    matches!(
                        e.to_ascii_lowercase().as_str(),
                        "png" | "jpg" | "jpeg" | "tif" | "tiff"
                    )
                })
        })
        .collect();
    entries.sort();
    if entries.is_empty() {
        return Err(format!(
            "{}: no image files found in directory (supported: .png, .jpg, .jpeg, .tif, .tiff)",
            dir.display()
        )
        .into());
    }
    Ok(entries)
}
