//! The decoder's reduced-size TIERS: the fast paths `decode_preview_with_raw_order` tries
//! before the general image decode - JPEG 2000 reduced decode, WIC-scaled JPEG, video
//! frames, XCF, DjVu, container covers, and the PDF tier (`Windows.Data.Pdf`), where Adobe
//! Illustrator files get their own rules on top: every artboard on the tile, and the truth
//! about a file saved without PDF content. Split out of `decode.rs` on 2026-09-19 (issues
//! #44 and #45), which had grown past the oversized-file line.

use super::*;

/// Longest edge to rasterize a PDF's first page at, for a request whose target is `cx` — on
/// the ordinary path. The Illustrator path takes it as the exact WIDTH instead (see below).
///
/// Pure, so the rule is testable without the OS PDF engine. It exists as a named function
/// because the obvious one-liner has been wrong twice: a fixed 1024 upscales once the user's
/// ceiling can exceed it, and `settings::max_thumb_size()` reads the global CEILING rather than
/// what was asked for, so a 32 px icon request would rasterize a 2560 px page and discard it.
pub(crate) fn pdf_raster_edge(wic_thumbnail_cx: Option<u32>) -> u32 {
    // Floor at the historical 1024: a big source downscales cheaply and stays crisp, and this
    // guarantees the change can never render a PDF at LOWER quality than it used to.
    // ...and a ceiling at the crate-wide raster cap: an MCP/CLI caller can pass any `size`,
    // and `pdf::scaled_page_dims` clamps the page to exactly this number before asking WinRT
    // to rasterize it. The Illustrator paths instead call `PdfSession::render_to_width`, which
    // takes this as the requested WIDTH and fits the page to it (`pdf::width_fitted_dims`),
    // scaling BOTH edges down together when the derived height would pass this same cap.
    wic_thumbnail_cx
        .unwrap_or(1024)
        .clamp(1024, limits::MAX_DIM)
}

/// JPEG 2000 with a size cap: our own reduced-resolution decoder, which decodes ONLY
/// the wavelet levels the target needs. On the 76 MP corpus scan that is ~0.5s against
/// ~4s for a full ImageMagick decode, and the output is a true resolution level (often
/// SHARPER than decode-then-downscale). Gated on a cap on purpose: full-fidelity
/// callers (Convert, Image info) keep the established tiers, and ANY error here — the
/// declined coding styles, subsampled chroma, malformed data — falls through to those
/// same tiers, so no JP2 that rendered before can render worse. Correctness evidence:
/// bit-exact on every lossless corpus file (see decode/jp2 exactness tests), verified
/// against ImageMagick on the lossy ones.
///
/// The one uncapped case it takes is a codestream whose picture sits at an offset on the
/// reference grid: ImageMagick takes that offset off twice and crops the picture (the
/// corpus's real.j2k lost 150 columns and 300 rows in Quick preview and Convert), so there
/// this decodes it whole, at up to [`limits::MAX_DIM`].
pub(crate) fn try_jp2_reduced_tier(
    bytes: &[u8],
    wic_thumbnail_cx: Option<u32>,
) -> Option<DynamicImage> {
    if !jp2::is_jp2(bytes) {
        return None;
    }
    let cx = match wic_thumbnail_cx {
        Some(cx) => cx,
        None if jp2::has_image_offset(bytes) => limits::MAX_DIM,
        None => return None,
    };
    if let Ok((rgb, w, h)) = jp2::decode_reduced(bytes, cx) {
        if let Some(img) = image::RgbImage::from_raw(w, h, rgb) {
            // EXIF orientation, same as the final fallback tier applies. Applying it here
            // rather than deferring matters because a thumbnail that comes back rotated is
            // one Explorer then CACHES rotated.
            return Some(apply_exif_orientation(DynamicImage::ImageRgb8(img), bytes));
        }
    }
    st2k_base::safety::log_debug("decode: jp2 native reduced decode declined, using tiers");
    None
}

/// Large JPEG: decode DCT-SCALED instead of decoding every pixel and then throwing almost
/// all of them away. Exactly the same bargain as the JP2 tier above: ask the codec for a
/// reduced resolution level rather than the full image — and gated the same way, on a
/// caller that actually wants a thumbnail.
///
/// This is the difference between a 7680x2160 wallpaper costing ~4 s a tile and costing a
/// fraction of that. Measured on a real folder: 65 files, 1.3 GB of AI-upscaled JPEG and
/// PNG, took ~150 s to pre-build, of which the top seven files alone were ~55 s. Thread
/// count was NOT the cause (3 -> 16 workers moved it 6 %), nor the three size buckets; it
/// was that every tile decoded its source in full.
///
/// Only JPEG, and only above a size floor — see `wic_scaled_from_bytes_if_codec_scales` for
/// why widening it is a re-measurement rather than a one-line change. Any failure falls
/// straight through to the tiers below, so nothing that rendered before can stop rendering.
///
/// WIC does NOT apply EXIF orientation (it hands back the codec's stored pixels), and this
/// tier has to apply it itself rather than relying on the final fallback's own EXIF step.
/// Camera JPEGs are overwhelmingly the files that clear the 512 KiB floor AND carry a
/// non-identity orientation, which makes this tier the one place it matters most.
pub(crate) fn try_wic_scaled_jpeg_tier(
    bytes: &[u8],
    wic_thumbnail_cx: Option<u32>,
) -> Option<DynamicImage> {
    let cx = wic_thumbnail_cx?;
    let img = wic_scaled_from_bytes_if_codec_scales(bytes, cx)?;
    Some(apply_exif_orientation(img, bytes))
}

/// Video: grab a representative frame via the OS Media Foundation codecs (no bundled
/// bytes). Magic-gated, so only actual videos pay the MF cost (HEIC/AVIF share the
/// `ftyp` box but are excluded). Any decode failure falls through to the image tiers,
/// which then fail to the file's default icon — never worse than before.
pub(crate) fn try_video_tier(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    wic_thumbnail_cx: Option<u32>,
) -> Option<Result<DynamicImage>> {
    if !crate::video::is_video_magic(bytes) {
        return None;
    }
    // OPTION (`VideoCoverArt`, off by default): show the embedded poster instead of a
    // frame. Checked before the decode tiers so it costs nothing when a cover exists,
    // and falls straight through when one doesn't. Mirrors the provider in `streamsrc`.
    //
    // `tried_cover_art` remembers whether this pass ran (G123, mirroring streamsrc's
    // `tried_cover_art`): if it did and found nothing, the fallback rescue below (after
    // every frame tier also fails) must not call `vcodec::cover_art` a second time — the
    // bytes haven't changed, so it would just re-scan the same moov to the same null answer.
    let mut tried_cover_art = false;
    if st2k_base::settings::prefer_cover_art() {
        tried_cover_art = true;
        if let Some(cover) = crate::vcodec::cover_art(&mut std::io::Cursor::new(bytes)) {
            return Some(decode_image_with_raw_order(
                &cover,
                raw_preview,
                wic_thumbnail_cx,
            ));
        }
    }
    // Prefer the smart targeted read for a representative keyframe built from the
    // container's own index — MP4/MOV via the `moov` (`crate::mp4`), Matroska/WebM via the
    // Cues (`crate::mkv`). Each self-gates to its container and returns None otherwise (or
    // when the index can't be mapped), so we fall back to decoding a frame off the buffer.
    // The mark is the user's `VideoOffset` (30 % unless changed), read ONCE so every tier
    // below seeks to the same place.
    let at = st2k_base::settings::video_offset_frac();
    // The MP4/MKV tiers hand back the display rotation they already parsed out of the same
    // moov/Tracks they read for the mini-clip, so a tier that DID parse the
    // container never needs the standalone `display_rotation` probe below to re-read it.
    let mp4_clip = crate::mp4::keyframe_mini_mp4(&mut std::io::Cursor::new(bytes), at);
    let mkv_clip = if mp4_clip.is_none() {
        crate::mkv::keyframe_mini_mkv(&mut std::io::Cursor::new(bytes), at)
    } else {
        None
    };
    let (container_ran, container_rotation) =
        crate::streamsrc::container_facts(mp4_clip.as_ref(), mkv_clip.as_ref());
    let mini = mp4_clip
        .map(|(b, _)| b)
        .or_else(|| mkv_clip.map(|(b, _)| b));

    // ISSUE #35, the by-bytes twin of the gate in `streamsrc::try_video_source`: a track
    // whose H.264 profile Windows' decoder does not implement (4:4:4 / 4:2:2 / 10-bit) is
    // never handed to Media Foundation, on any tier. Read off the mini-clip already in RAM.
    let mf_refused = mini
        .as_deref()
        .and_then(|m| crate::vcodec::mf_undecodable_reason(&mut std::io::Cursor::new(m)));
    if let Some(reason) = &mf_refused {
        st2k_base::safety::log(&format!(
            "video: {reason}; every Media Foundation tier skipped (issue #35)"
        ));
    }
    let mf = mf_refused.is_none();

    if let Some(frame) = frame_by_bytes(bytes, mini, mf, at) {
        return Some(Ok(rotated_as_displayed(
            frame,
            bytes,
            container_ran,
            container_rotation,
        )));
    }
    // No decodable frame — usually a missing OS codec (HEVC/AV1 are Store add-ons).
    // An embedded cover (a Matroska attachment or an MP4 `covr` item, which library
    // rips and media managers routinely write) is still a faithful picture of the file,
    // and unlike a frame it needs no codec at all. Mirrors the provider's fallback in
    // `streamsrc`, so the CLI, the preview and Explorer all agree. Skipped when the
    // prefer-cover-art pass above already tried and found nothing.
    if !tried_cover_art {
        if let Some(cover) = crate::vcodec::cover_art(&mut std::io::Cursor::new(bytes)) {
            return Some(decode_image_with_raw_order(
                &cover,
                raw_preview,
                wic_thumbnail_cx,
            ));
        }
    }
    None
}

/// One representative frame from the whole capped buffer, trying every decoder tier in
/// order: the container's own keyframe mini-clip through Media Foundation (`mf`), then the
/// FLV remux, the out-of-process Flash decoder, MF over the raw buffer, and last the
/// out-of-process VP9 and MPEG-1/2 decoders. `at` is the user's `VideoOffset` mark.
fn frame_by_bytes(bytes: &[u8], mini: Option<Vec<u8>>, mf: bool, at: f64) -> Option<DynamicImage> {
    mini.filter(|_| mf)
        .and_then(crate::video::frame_from_owned_bytes)
        // FLV (H.264 only): MF has no FLV demuxer, so without this remux the container
        // never opens at all. No index to honour `at` with — first keyframe (see `flv`).
        .or_else(|| {
            if !mf {
                return None;
            }
            crate::flv::keyframe_mini_mp4(&mut std::io::Cursor::new(bytes))
                .and_then(crate::video::frame_from_owned_bytes)
        })
        // FLV, VP6/Sorenson (issue #26): NO Windows decoder exists for these, so the
        // frame is decoded out of process by the sibling st2k.exe (see `flv::flash_frame`
        // for why the pure-Rust Flash decoders must never run in THIS process). Self-gated
        // on the FLV magic + codec id, so every other container skips it for free.
        .or_else(|| crate::flv::flash_frame(&mut std::io::Cursor::new(bytes)))
        // Other containers (AVI/WMV/…): we hold the whole capped buffer in RAM, so let MF
        // seek its own index to the true ~30 % frame (no head-prefix depth cap).
        .or_else(|| {
            if !mf {
                return None;
            }
            crate::video::frame_from_bytes_repr(bytes)
        })
        // VP9 Profile 2/3 (10/12-bit HDR in webm/mkv, issue #26): Media Foundation's
        // VP9 decoder stops at Profile 0/1 even with the Store extension installed, so
        // when every MF tier above came back empty AND the container says V_VP9, the
        // keyframe is decoded out of process by the sibling st2k.exe (`crate::vp9` for
        // why the pure-Rust decoder must never run in THIS process). Deliberately LAST:
        // Profile 0 is the common case and MF is hardware-accelerated and in-process —
        // it must keep winning, and only otherwise-blank tiles pay for a spawn.
        .or_else(|| crate::vp9::vp9_frame(&mut std::io::Cursor::new(bytes), at))
        // MPEG-1 system streams, bare MPEG-1/2 elementary streams, and MPEG-2 program
        // streams on a machine without the Store extension: Media Foundation has no source
        // for the first two on any Windows, so when every MF tier above came back empty AND
        // the head is one of the two MPEG magics, our own demux cuts one intra picture and
        // the sibling st2k.exe decodes it out of process (`crate::mpeg12`). Last for the
        // same reason as VP9: a `.vob` with the Store extension, or a transport stream
        // named `.mpg`, keeps hitting the in-process MF path.
        .or_else(|| crate::mpeg12::mpeg_frame(&mut std::io::Cursor::new(bytes), at))
}

/// ISSUE #32, the by-bytes twin of the gate in `streamsrc::try_video_source`, and kept in
/// step with it deliberately: a clip rotated losslessly (metadata only, no re-encode) must
/// thumbnail the way it plays on every surface, or `st2k` and Explorer disagree about one
/// file. See `video::apply_display_rotation` for why this cannot double-rotate whichever
/// tier produced the frame.
///
/// Only falls back to the standalone probe when NEITHER container tier parsed the file
/// (`container_ran`) — a tier that did, already answered this exact question.
fn rotated_as_displayed(
    frame: DynamicImage,
    bytes: &[u8],
    container_ran: bool,
    container_rotation: Option<u32>,
) -> DynamicImage {
    let rotation = if container_ran {
        container_rotation
    } else {
        crate::mp4::display_rotation(&mut std::io::Cursor::new(bytes))
            .or_else(|| crate::mkv::display_rotation(&mut std::io::Cursor::new(bytes)))
    };
    match rotation {
        Some(deg) => {
            st2k_base::safety::log_debugf!("video: display matrix asks for {deg} deg");
            crate::video::apply_display_rotation(frame, deg)
        }
        None => frame,
    }
}

/// GIMP `.xcf` FIRST, and only when the caller told us how big a picture it can use.
/// `extract_cover` reaches the same decoder, but its signature carries no target, so it
/// flattens the full canvas — measured at 5.7 s of layer decode plus 4.6 s of compositing
/// for one 6000x4000 file with 15 layers, all of it to produce a 256 px tile. Handing the
/// target in drops that to milliseconds. Falls through to `extract_cover` below when there
/// is no target (the full-fidelity callers), so the picture they get is unchanged.
pub(crate) fn try_xcf_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if wic_thumbnail_cx.is_none() || !crate::container::looks_like_xcf(bytes) {
        return None;
    }
    crate::container::xcf_from_bytes_scaled(bytes, wic_thumbnail_cx)
}

/// DjVu, for a related but narrower reason than the XCF tier above. It does NOT render
/// smaller for a smaller tile - a DjVu costs what its JB2 mask and IW44 background cost
/// regardless, and shrinking the render only coarsens the picture. What the target decides
/// is whether the file's baked TH44 thumbnail can serve this request: encoders cap it at
/// 128 px, so it answers Explorer's icon and list views (16/32/48/96) in about two
/// milliseconds against nearly two hundred for a render, and must be rendered past for
/// anything bigger. `extract_cover` carries no target and so has to assume the largest.
/// Falls through to it when there is no target, which is what Convert wants anyway.
pub(crate) fn try_djvu_tier(bytes: &[u8], wic_thumbnail_cx: Option<u32>) -> Option<DynamicImage> {
    if wic_thumbnail_cx.is_none() || !crate::container::looks_like_djvu(bytes) {
        return None;
    }
    crate::container::djvu_from_bytes_scaled(bytes, wic_thumbnail_cx)
}

/// Ebook / comic-archive cover extraction (EPUB, CBZ, MOBI, FB2, CB7, CBR,
/// DjVu…). If this is a container, pull the cover and decode THAT. The cover
/// bytes go through `decode_image` (not back through here) so a maliciously
/// nested container can't recurse — depth is capped at 1.
pub(crate) fn try_container_cover_tier(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    wic_thumbnail_cx: Option<u32>,
) -> Option<Result<DynamicImage>> {
    let cover = crate::container::extract_cover(bytes)?;
    Some(match cover {
        crate::container::CoverOut::Bytes(b) => {
            decode_image_with_raw_order(&b, raw_preview, wic_thumbnail_cx)
        }
        crate::container::CoverOut::Image(img) => Ok(img),
    })
}

/// PDF: rasterize page 1 via the OS PDF engine (Windows.Data.Pdf). The PNG it
/// returns goes through `decode_image`, same as an ebook cover.
///
/// The raster edge follows THIS REQUEST's target, floored at the 1024 this always used, so
/// it is never smaller than before and never larger than the tile actually needs. Two ways
/// to get this wrong, both avoided here:
///   - A fixed 1024 (what shipped before) would make PDFs the one format that upscales a
///     too-small source once the ceiling can exceed 1024 (issue #26.5).
///   - Deriving it from `settings::max_thumb_size()` instead — which is what the first cut
///     of this fix did — reads the user's global CEILING rather than what Explorer asked
///     for, so a 32 px icon-view request would rasterize a 2560 px page and throw almost
///     all of it away. `wic_thumbnail_cx` is already clamped per request
///     (`thumbprovider`: `cx.min(max_thumb)`), which is exactly the number wanted here, and
///     is what the JP2 tier above uses too.
///
/// Full-fidelity callers pass None and keep the historical 1024.
pub(crate) fn try_pdf_tier(
    bytes: &[u8],
    raw_preview: RawPreviewOrder,
    wic_thumbnail_cx: Option<u32>,
) -> Option<Result<DynamicImage>> {
    if !bytes.starts_with(b"%PDF-") {
        return None;
    }
    let edge = pdf_raster_edge(wic_thumbnail_cx);
    // Adobe Illustrator is a PDF with the artwork's own rules (issues #44 and #45): every
    // artboard is a page, and a file saved without "Create PDF Compatible File" has a
    // placeholder page where the artwork should be. Answered before the plain page-one
    // render, and only for a file that carries Illustrator's private data.
    if crate::container::ai::is_illustrator(bytes) {
        if let Some(answer) = illustrator_thumbnail(bytes, edge) {
            return Some(answer);
        }
    }
    let png = crate::pdf::render_first_page(bytes, edge)?;
    Some(decode_image_with_raw_order(
        &png,
        raw_preview,
        wic_thumbnail_cx,
    ))
}

/// Pages of an Illustrator file laid out as a contact sheet: up to this many artboards.
pub(crate) const AI_SHEET_PAGES: usize = 4;

/// The thumbnail of an Illustrator file, by what the file actually holds:
///
/// - several pages (one per artboard): the first [`AI_SHEET_PAGES`] laid out as a contact
///   sheet, so a three-artboard file shows three artboards (issue #44), the way a comic
///   archive shows its pages;
/// - one page that is the "saved without PDF content" placeholder: the raster thumbnail
///   Illustrator wrote into its private data, which is the artwork at low resolution and what
///   every other viewer shows for such a file (issue #45);
/// - one page of real artwork: that page, rendered as before.
///
/// `None` hands the file to the ordinary page-one render (a PDF the session cannot open, a
/// file whose private thumbnail is missing or malformed).
fn illustrator_thumbnail(bytes: &[u8], edge: u32) -> Option<Result<DynamicImage>> {
    let session = crate::pdf::PdfSession::open(bytes)?;
    let pages = session.page_count();
    // A sheet takes the first pages; one page is all a single artboard needs.
    let want = if pages >= 2 {
        pages.min(AI_SHEET_PAGES)
    } else {
        1
    };
    let rendered = (0..want)
        .map_while(|i| {
            session
                .render_to_width(i, edge)
                .and_then(|png| image::load_from_memory(&png).ok())
        })
        .collect();
    illustrator_answer(rendered, pages, bytes, edge)
}

/// The Illustrator answer (see [`illustrator_thumbnail`]) from its first pages already drawn
/// `edge` wide, in order (fewer than asked when one failed), the document's page count, and the
/// bytes that hold its private data: the whole file, or the head of one too big to hold (the
/// stream cascade's big-file path, where the pages are drawn off the stream itself).
pub(crate) fn illustrator_answer(
    rendered: Vec<DynamicImage>,
    pages: usize,
    bytes: &[u8],
    edge: u32,
) -> Option<Result<DynamicImage>> {
    use crate::container::ai;
    if pages >= 2 && rendered.len() == pages.min(AI_SHEET_PAGES) {
        if let Some(sheet) = illustrator_sheet(&rendered, edge) {
            return Some(Ok(sheet));
        }
    }
    let page = rendered.into_iter().next();
    match (page, ai::private_thumbnail(bytes)) {
        // Illustrator up to CS4 kept a raster of the artwork beside the placeholder.
        (Some(page), Some(thumb)) if ai::page_is_placeholder(&page, &thumb) => Some(Ok(thumb)),
        // Illustrator 2020 and later keep no raster at all (measured 2026-09-19 on files
        // Illustrator 30.8 wrote: the private data is zstd-compressed PostScript with no
        // thumbnail in it). A placeholder page then means the file holds NO picture another
        // program can show, and an honest answer is no thumbnail plus the reason, not a page
        // of small print that looks like a broken render (issue #45).
        (Some(_), None) if pages == 1 && ai::looks_like_placeholder_page(bytes) => {
            Some(Err(Error::new(E_FAIL, ai::NO_PDF_CONTENT)))
        }
        (Some(page), _) => Some(Ok(page)),
        (None, thumb) => thumb.map(Ok),
    }
}

/// Fold pages already drawn `edge` wide into one square sheet.
fn illustrator_sheet(pages: &[DynamicImage], edge: u32) -> Option<DynamicImage> {
    let prepared: Vec<_> = pages
        .iter()
        .map(|img| crate::container::collage::prepare_for_sheet(img, edge))
        .collect();
    crate::container::collage::compose_prepared(&prepared, edge).map(DynamicImage::ImageRgba8)
}

/// Adobe Illustrator through the PDF tier (issues #44 and #45, 2026-09-19).
#[cfg(test)]
mod illustrator_tests {
    use super::*;

    fn corpus(name: &str) -> Option<Vec<u8>> {
        st2k_base::testcorpus::read(name)
    }

    /// Files Illustrator 30.8 (2026) wrote on 2026-09-19, authored through the app's own
    /// automation for exactly these cases. They are corpus samples, not fixtures in git, so
    /// each test says NOT MEASURED and returns when the sample is absent rather than passing.
    #[test]
    fn a_modern_file_saved_without_pdf_content_gets_no_thumbnail_and_says_why() {
        let Some(bytes) = corpus("real-nocompat.ai") else {
            return;
        };
        assert!(crate::container::ai::is_illustrator(&bytes));
        assert!(
            crate::container::ai::private_thumbnail(&bytes).is_none(),
            "2020+ writes no raster"
        );
        let r = try_pdf_tier(&bytes, RawPreviewOrder::AfterExternal, Some(256)).expect("pdf tier");
        let err = r.expect_err("a placeholder page is not a picture");
        assert!(
            err.message().contains("Create PDF Compatible File"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn a_modern_file_with_three_artboards_shows_all_three() {
        let Some(bytes) = corpus("real-artboards.ai") else {
            return;
        };
        let img = try_pdf_tier(&bytes, RawPreviewOrder::AfterExternal, Some(256))
            .expect("pdf tier")
            .expect("decoded");
        let e = img.width();
        assert_eq!(img.height(), e, "a square sheet");
        // Authored as a red, a green (with a dark disc) and a blue artboard; Illustrator
        // orders the pages its own way, so the claim is "all three are on the tile", one per
        // cell of the 3-up layout, not which cell holds which.
        // Sample each cell's corner: every artboard carries a mark in its middle.
        let cells = [
            dominant(&img, e / 16, e / 16),
            dominant(&img, e * 15 / 16, e / 16),
            dominant(&img, e * 15 / 16, e * 15 / 16),
        ];
        let mut seen = cells.to_vec();
        seen.sort_unstable();
        assert_eq!(seen, vec!['b', 'g', 'r'], "cells: {cells:?}");
    }

    #[test]
    fn a_modern_file_with_pdf_content_and_one_artboard_shows_that_artboard() {
        // Three artboards saved WITHOUT PDF content: one placeholder page, no raster -> no
        // thumbnail (nothing in the file can separate the artboards; issue #44's reply says so).
        let Some(bytes) = corpus("real-artboards-nocompat.ai") else {
            return;
        };
        let r = try_pdf_tier(&bytes, RawPreviewOrder::AfterExternal, Some(256)).expect("pdf tier");
        assert!(r.is_err(), "a placeholder page must not become a thumbnail");
    }

    /// The PostScript-era shape (Illustrator 8 format, still what "save as legacy" writes)
    /// carries the private raster, and the EPS tier serves it.
    #[test]
    fn a_legacy_postscript_ai_shows_its_private_thumbnail() {
        let Some(bytes) = corpus("real-legacy8.ai") else {
            return;
        };
        assert!(bytes.starts_with(b"%!PS"));
        let thumb = crate::container::ai::private_thumbnail(&bytes).expect("private thumbnail");
        assert!(
            thumb.width() > 0 && crate::container::ai::ink_fraction(&thumb) > 0.3,
            "the red artboard"
        );
        let cover = crate::container::extract_cover(&bytes).expect("eps tier cover");
        assert!(matches!(cover, crate::container::CoverOut::Image(_)));
    }

    /// A three-page PDF of three flat colours (one page per "artboard"), optionally wearing
    /// Illustrator's private-data tell, so the tier treats it as an Illustrator file without a
    /// byte of Illustrator's own PDF being needed. Written to the spec by `pdf::tests`, never by
    /// our own Combine writer (a fixture written by code under test shares its assumptions).
    fn three_page_pdf(illustrator: bool) -> Vec<u8> {
        let mut bytes =
            crate::pdf::tests::solid_colour_pdf(&[(220, 30, 30), (30, 200, 40), (30, 60, 220)]);
        if illustrator {
            // The Illustrator tell for every era is the `/AIPrivateData` key (a real file
            // carries it in the PDF catalog; trailing bytes after %%EOF are what every
            // Illustrator file has and what the renderer tolerates). No corpus file is
            // needed, so this test runs on a CI checkout too.
            bytes.extend_from_slice(b"\r%!PS-Adobe-3.0\r/AIPrivateData1 7 0 R\r");
        }
        bytes
    }

    fn dominant(img: &DynamicImage, x: u32, y: u32) -> char {
        let p = img.to_rgba8().get_pixel(x, y).0;
        if p[3] < 128 {
            return 't';
        }
        if p[0] > p[1] && p[0] > p[2] {
            'r'
        } else if p[1] > p[0] && p[1] > p[2] {
            'g'
        } else {
            'b'
        }
    }

    /// Issue #44: a file with several artboards shows them all - the first pages laid out as a
    /// contact sheet, one large cell and two stacked, rather than page one alone.
    #[test]
    fn an_illustrator_file_with_three_artboards_shows_all_three() {
        let bytes = three_page_pdf(true);
        assert!(crate::container::ai::is_illustrator(&bytes));
        let img = try_pdf_tier(&bytes, RawPreviewOrder::AfterExternal, Some(256))
            .expect("pdf tier")
            .expect("decoded");
        let e = img.width();
        assert_eq!(img.height(), e, "a square sheet ({e}x{})", img.height());
        assert_eq!(
            e,
            pdf_raster_edge(Some(256)),
            "the sheet is the tier's raster edge"
        );
        // 3-up layout: one large left column (page 1, red), two stacked right cells
        // (page 2 green above page 3 blue).
        assert_eq!(dominant(&img, e / 4, e / 2), 'r');
        assert_eq!(dominant(&img, e * 3 / 4, e / 4), 'g');
        assert_eq!(dominant(&img, e * 3 / 4, e * 3 / 4), 'b');
    }

    /// The same three pages WITHOUT Illustrator's private data are an ordinary document: page
    /// one, nothing else - a report's first page is its thumbnail, never a sheet of its pages.
    #[test]
    fn a_plain_multipage_pdf_still_shows_page_one_only() {
        let bytes = three_page_pdf(false);
        assert!(!crate::container::ai::is_illustrator(&bytes));
        let img = try_pdf_tier(&bytes, RawPreviewOrder::AfterExternal, Some(256))
            .expect("pdf tier")
            .expect("decoded");
        assert_ne!(
            (img.width(), img.height()),
            (256, 256),
            "page one keeps its own aspect"
        );
        let rgba = img.to_rgba8();
        assert!(
            rgba.pixels().all(|p| p[0] > p[1] && p[0] > p[2]),
            "every pixel is page one's red"
        );
    }
}
