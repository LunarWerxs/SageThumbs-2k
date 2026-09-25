//! The creative-app formats' covers: the native decoders (DjVu, GIMP, Paint.NET, icns,
//! Blender, Affinity, Paint Shop Pro, Aseprite, SFW, ILBM, Cinema 4D, CorelDRAW, Clip Studio)
//! and the embedded-preview-only ones (Photoshop, EPS), keyed on magic words.

use super::*;

/// Creative-app native formats: DjVu, GIMP, Paint.NET, Photoshop, EPS, icns,
/// Blender, Affinity, Paint Shop Pro, ILBM, Cinema 4D, CorelDRAW, Clip Studio.
pub(super) fn try_creative_app_cover(bytes: &[u8]) -> Option<CoverOut> {
    // DjVu (IFF85 magic "AT&TFORM").
    if looks_like_djvu(bytes) {
        return djvu::extract(bytes).map(CoverOut::Image);
    }
    // PSD / DOS-EPS / EPS carry an embedded preview only: a magic match answers with
    // that preview or None; only an unmatched magic reaches the checks below.
    if let Some(cover) = try_embedded_preview_cover(bytes) {
        return cover;
    }
    // GIMP XCF: native flatten-to-thumbnail. Takes priority over the magick tier on
    // purpose — ImageMagick's coder fails on the modern "gimp xcf v011" (GIMP 2.10/3.0),
    // and ours needs no ImageMagick at all (works on the compact install).
    if xcf::looks_like_xcf(bytes) {
        return xcf::extract(bytes).map(CoverOut::Image);
    }
    // Paint.NET: the base64 PNG preview in the XML preamble. Never touches the
    // .NET-serialized document after it, so no ImageMagick and no deserializer.
    if pdn::looks_like_pdn(bytes) {
        return pdn::extract(bytes).map(CoverOut::Bytes);
    }
    // Apple Icon Image: slice out the largest embedded PNG / JPEG-2000 member.
    if bytes.starts_with(b"icns") {
        return icns::extract(bytes).map(CoverOut::Bytes);
    }
    // Blender: the RGBA thumbnail baked into the TEST file-block.
    if bytes.starts_with(b"BLENDER") {
        return blend::extract(bytes).map(CoverOut::Image);
    }
    // COMPRESSED Blender scene (the "Compress" save option): gzip or zstd wrapper
    // around the same block stream. Bounded head inflate, gated on the inner
    // BLENDER magic (svgz/emz and other gzip payloads skip the cost and stay with
    // the decode tiers).
    if let Some(inner) = blend_compressed_head(bytes) {
        return blend::extract(&inner).map(CoverOut::Image);
    }
    // Affinity (Photo/Designer/Publisher): an embedded PNG preview.
    if affinity::looks_like_affinity(bytes) {
        return affinity::extract(bytes).map(CoverOut::Bytes);
    }
    // Paint Shop Pro (.pspimage/.psp): carve the JPEG preview from the file's
    // Composite Image Bank (present even when the pixel data is RLE/uncompressed).
    if psp::looks_like_psp(bytes) {
        // Full bank parse first: it finds the LARGEST composite and can decode the LZ77/raw
        // channel planes that `.PspBrush` uses exclusively and `.PspTube` stores alongside a
        // much smaller JPEG thumbnail. Falls back to the cheap JPEG carve when the composite
        // uses a compression we deliberately don't guess at (RLE) or the bank is malformed.
        return psp::extract_best(bytes).or_else(|| psp::extract(bytes).map(CoverOut::Bytes));
    }

    // The remaining native decoders (keyed on a magic word, never the extension).
    try_native_decoder_cover(bytes)
}

/// Native-decoder creative-app formats with no image tier behind them — Aseprite,
/// Seattle FilmWorks, IFF ILBM, Cinema 4D, CorelDRAW, Clip Studio — each carve or
/// decode their own preview; `None` when no magic matches.
fn try_native_decoder_cover(bytes: &[u8]) -> Option<CoverOut> {
    // Aseprite sprites: rendered from their own layers. Keyed on the magic word at offset 4,
    // never the extension - `.ase` is also 3DS ASCII scenes, Adobe swatches and GAP data.
    if aseprite::looks_like_aseprite(bytes) {
        return aseprite::extract(bytes).map(CoverOut::Image);
    }
    // Seattle FilmWorks: `SFW94A` (a photo) or `SFW95A` (an album, first photo). The JPEG
    // inside is rebuilt and decoded here because it has to be flipped afterwards.
    if sfw::looks_like_sfw(bytes) {
        return sfw::extract(bytes).map(CoverOut::Image);
    }
    // Amiga / Deluxe Paint IFF ILBM (and DOS PBM): real planar-bitmap decode to
    // pixels. The `ILBM`/`PBM ` FORM type keeps this off AIFF audio (`FORM…AIFF`).
    if ilbm::looks_like_ilbm(bytes) {
        return ilbm::extract(bytes).map(CoverOut::Image);
    }
    // Cinema 4D (.c4d): carve the document/scene preview JPEG from the header slot
    // (material-swatch JPEGs deeper in the file are filtered out by size/offset).
    if c4d::looks_like_c4d(bytes) {
        return c4d::extract(bytes).map(CoverOut::Bytes);
    }
    // CorelDRAW .cdr/.cdt / Corel .cmx: RIFF files with an embedded DISP preview
    // DIB. The `CDR`/`CDT`/`CMX` form keeps this off WAV/other RIFF (`RIFF…WAVE`).
    if cdr::looks_like_cdr(bytes) {
        return cdr::extract(bytes).map(CoverOut::Bytes);
    }
    // Clip Studio Paint: read the preview PNG out of the embedded SQLite db.
    if bytes.starts_with(b"CSFCHUNK") {
        return clip::extract(bytes).map(CoverOut::Bytes);
    }
    None
}

/// Embedded-preview-only native formats (Photoshop PSD/PSB, DOS-EPS, plain EPS):
/// `Some(Some(cover))` when the magic matched and a preview was usable, `Some(None)`
/// when the magic matched but yields no cover (the caller returns that `None`),
/// `None` when the magic does not match at all.
fn try_embedded_preview_cover(bytes: &[u8]) -> Option<Option<CoverOut>> {
    // Photoshop PSD/PSB: the baked-in JPEG thumbnail (resource 1036). Works with
    // no ImageMagick; on None we fall through so a full install can still render
    // the layers via the magick tier.
    if bytes.starts_with(b"8BPS") {
        if let Some(thumb) = psd::extract(bytes) {
            return Some(Some(CoverOut::Bytes(thumb)));
        }
        return Some(None);
    }
    // DOS-EPS: the baked-in TIFF screen preview (real PS rendering would need
    // Ghostscript). A WMF-only/bare file stays terminally unsupported in the
    // decoder instead of falling through to any PostScript-capable external tier.
    if bytes.starts_with(&[0xC5, 0xD0, 0xD3, 0xC6]) {
        if let Some(cover) = eps::extract_dos_eps_cover(bytes) {
            return Some(Some(cover));
        }
        return Some(None);
    }
    // Plain EPS: only read an already-embedded EPSI/Photoshop raster preview;
    // never invoke a PostScript interpreter in the thumbnail host.
    if bytes.starts_with(b"%!PS") {
        if let Some(cover) = eps::extract_ascii_preview(bytes) {
            return Some(Some(cover));
        }
        return Some(None);
    }
    None
}
