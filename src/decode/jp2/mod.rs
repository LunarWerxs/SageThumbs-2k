//! A pure-Rust JPEG 2000 decoder that decodes at a REDUCED RESOLUTION.
//!
//! # Why this exists
//!
//! JPEG 2000 is a wavelet codec: every file already contains the image at a cascade of
//! halved resolutions, and a thumbnail only needs one of the small ones. Nothing available
//! would actually do that. ImageMagick's `-define jp2:reduce-factor` returns correctly-sized
//! output containing the WRONG PIXELS (the top-left corner rather than a downscale), on both
//! tiled and untiled files. The one Rust crate advertising the feature,
//! `oxigdal-jpeg2000 0.1.7`, computes a scale factor, logs "scale 1/8", then calls
//! `decode_region` and discards the level entirely — also a crop. Both were verified by
//! looking at the decoded pixels, not the timings.
//!
//! So a 76 MP scan (11 MB on disk, issue #11) had to be decoded in full and thrown away,
//! which cost ~4 s of pure wavelet work and pushed the preview pane past its budget.
//!
//! # What this does
//!
//! Decodes only what a target size needs: packets above the chosen resolution are walked for
//! their lengths but never handed to tier-1, and the inverse wavelet stops early. Each level
//! skipped removes about three quarters of the remaining coefficients.
//!
//! # Verification status (2026-08-04)
//!
//! Wired into the capped decode path. Evidence, in order of strength:
//!   * BIT-EXACT on every lossless corpus file (8x8..32x32, gray/RGB, smooth and plasma) —
//!     reversible 5/3 means a correct decoder has no rounding excuse, so exactness is a
//!     hard proof for the whole pipeline: container, packets, tag trees, MQ, tier-1
//!     passes, dequant, DWT, RCT.
//!   * The lossy 512x384 sample decodes within mean 1.65/255 of ImageMagick (residual is
//!     resize-filter difference at edges, ~2% of pixels).
//!   * The 76 MP archival scan (6 tiles, 1529 tile-parts, 30 layers, RPCL, 256x256
//!     precincts) decodes to a visually correct, SHARPER-than-reference map in ~0.5s
//!     against ~4s for the full-decode-and-shrink route.
//!
//! Hard-won debugging lessons, kept because each cost real time:
//!   * A packet has ONE "non-empty" bit and ONE byte-alignment covering ALL its bands;
//!     reading either per band desynchronizes everything after the first r>0 packet.
//!   * A code-block's layers must be CONCATENATED and tier-1-decoded once with continuous
//!     state, never per-layer with fresh contexts.
//!   * The zero-coding H/V swap CANNOT be settled by reading the spec or openjpeg — both
//!     use different labelling conventions than our counting. It was settled by running
//!     all four swap variants against the lossless corpus; only swap-on-Hl is exact. Note
//!     smooth gradients are swap-insensitive, so only textured content distinguishes them.
//!   * Debug the smallest file first. Byte-consumption accounting (segment length vs MQ
//!     bytes consumed) localizes divergence to specific blocks instantly.
//!
//! # Scope, deliberately
//!
//! Single-tile and multi-tile, 5/3 and 9/7, RCT and ICT, up to 4 components, LRCP/RLCP/RPCL
//! progressions. Anything else — arbitrary precincts with PPM/PPT packed headers, HTJ2K,
//! component subsampling other than 1:1 — returns `Unsupported` and the caller falls back to
//! ImageMagick, which is still the tier for everything exotic. This decoder is a fast path
//! for the common case, never the only way a JP2 can render.

// The packet walk indexes several parallel per-component / per-resolution structures at
// once (subbands, precinct state, precinct counts). Iterator form would need a zip of
// three collections and read far worse than the index that the spec itself is written in.
#![allow(clippy::needless_range_loop)]

mod codestream;
mod dwt;
mod mq;
mod packet;

use dwt::SubBand;
use packet::{BitReader, BlockState, PrecBand, TagTree};

#[derive(Debug)]
pub enum Jp2Error {
    Truncated,
    Malformed(&'static str),
    Unsupported(&'static str),
}

impl std::fmt::Display for Jp2Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Jp2Error::Truncated => write!(f, "truncated JPEG 2000 data"),
            Jp2Error::Malformed(m) => write!(f, "malformed JPEG 2000: {m}"),
            Jp2Error::Unsupported(m) => write!(f, "unsupported JPEG 2000 feature: {m}"),
        }
    }
}

/// Is this plausibly a JPEG 2000 file we should try? Cheap magic check, no parsing.
pub fn is_jp2(bytes: &[u8]) -> bool {
    bytes.starts_with(&[0xFF, 0x4F, 0xFF, 0x51])
        || (bytes.len() > 12 && &bytes[4..8] == b"jP  " && bytes[..4] == [0, 0, 0, 12])
}

/// Report the image size without decoding, so a caller can decide whether the reduced path
/// is worth taking at all.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let cs = codestream::find_codestream(bytes).ok()?;
    let c = codestream::parse(cs).ok()?;
    Some((c.siz.width(), c.siz.height()))
}

/// Reject any codestream shape this reduced-resolution path does not cover
/// (multi-component palettes, absurd component counts, chroma subsampling,
/// exotic progression orders). Returns the validated component count.
fn validate_reduced_scope(
    c: &codestream::Codestream,
    has_palette: bool,
) -> Result<usize, Jp2Error> {
    // A palette maps SINGLE-component indices; anything else is a shape we refuse to
    // guess at (the parser already declines the exotic ones).
    if has_palette && c.siz.components.len() != 1 {
        return Err(Jp2Error::Unsupported("palette with multiple components"));
    }
    let ncomp = c.siz.components.len();
    if ncomp == 0 || ncomp > codestream::MAX_COMPONENTS as usize {
        return Err(Jp2Error::Unsupported("component count"));
    }
    // Subsampled components (4:2:0 chroma) would need per-component grids and an upsample;
    // magick handles those. 1:1 covers the scanned/archival files this path is for.
    if c.siz.components.iter().any(|k| k.dx != 1 || k.dy != 1) {
        return Err(Jp2Error::Unsupported("component subsampling"));
    }
    // The DC level shift and the 8-bit scale at the end of `decode_reduced` are taken
    // from component 0 and applied to every plane, so the components that reach the
    // output must share one depth and signedness. Components past the ones the output
    // reads (alpha) are never decoded and may differ.
    let used = used_components(ncomp);
    if let Some(c0) = c.siz.components.first() {
        let mixed = c
            .siz
            .components
            .iter()
            .take(used)
            .any(|k| k.prec != c0.prec || k.signed != c0.signed);
        if mixed {
            return Err(Jp2Error::Unsupported("mixed component precision"));
        }
    }
    if !matches!(c.cod.progression, 0..=2) {
        return Err(Jp2Error::Unsupported("progression order"));
    }
    Ok(ncomp)
}

/// How many leading components the output reads: `color_planes_to_rgb` takes the first
/// three planes of a 3- or 4-component image and the first plane otherwise. Any component
/// past that (alpha) is walked for its packet lengths only, never tier-1 decoded, never
/// wavelet-reconstructed, and gets no pixel storage.
fn used_components(ncomp: usize) -> usize {
    if ncomp >= 3 {
        3
    } else {
        1
    }
}

/// Choose how many wavelet levels to DROP: the most that still leaves the
/// output >= `target_edge` on its long side. Returns `(drop, keep)`.
fn choose_reduction(full_w: u32, full_h: u32, levels: u32, target_edge: u32) -> (u32, u32) {
    let mut drop = 0u32;
    while drop < levels {
        let next = drop + 1;
        let w = full_w.div_ceil(1 << next);
        let h = full_h.div_ceil(1 << next);
        if w.max(h) < target_edge {
            break;
        }
        drop = next;
    }
    (drop, levels - drop) // (drop, keep = reconstruction steps we will actually run)
}

/// Bound the `planes` allocation the caller is about to make, independent of
/// MAX_PIXELS: MAX_PIXELS bounds the DECLARED area, but says nothing about
/// `ncomp` separate f32 buffers of that area. A spec-legal single-resolution
/// file (levels == 0) forces `drop` to stay 0 regardless of target_edge, so a
/// 268MP-declared, single-resolution JP2 requested at a tiny thumbnail size
/// would still try to allocate up to ~4.3GB across 4 components without this.
fn check_reduced_alloc_budget(out_w: u32, out_h: u32, ncomp: usize) -> Result<(), Jp2Error> {
    let px = (out_w as u64) * (out_h as u64);
    let max_px_for_alloc = crate::decode::limits::MAX_ALLOC / (4 * ncomp as u64);
    if px > crate::decode::limits::MAX_PIXELS || px > max_px_for_alloc {
        return Err(Jp2Error::Unsupported("reduced image still too large"));
    }
    Ok(())
}

/// Decode every tile that has data into the shared output `planes`.
fn decode_all_tiles(
    c: &codestream::Codestream,
    keep: u32,
    drop: u32,
    planes: &mut [Vec<f32>],
    out_w: u32,
    out_h: u32,
) -> Result<(), Jp2Error> {
    let ntx = c.siz.num_tiles_x();
    let nty = c.siz.num_tiles_y();
    for ty in 0..nty {
        for tx in 0..ntx {
            let ti = (ty * ntx + tx) as usize;
            let parts = c.tiles.get(ti).map(|v| v.as_slice()).unwrap_or(&[]);
            if parts.is_empty() {
                continue;
            }
            decode_tile(c, tx, ty, keep, drop, planes, out_w, out_h)?;
        }
    }
    Ok(())
}

/// Palette-indexed image: the decoded samples are LOOKUP INDICES, not
/// intensities. Rendering them as gray paints an archive.org blank scanned
/// page (palette 0=white) solid black — the exact failure the corpus bilevel
/// fixture pins.
fn palette_to_rgb(n: usize, plane0: &[f32], pal: &codestream::Palette, shift: f32) -> Vec<u8> {
    let mut rgb = vec![0u8; n * 3];
    let last = pal.entries.len() - 1;
    for i in 0..n {
        let idx = ((plane0[i] + shift).round().max(0.0) as usize).min(last);
        rgb[i * 3..i * 3 + 3].copy_from_slice(&pal.entries[idx]);
    }
    rgb
}

/// One pixel's (R, G, B) sample values, inverse component transform applied.
fn mct_pixel(a: f32, bb: f32, cc: f32, mct: bool, reversible: bool) -> (f32, f32, f32) {
    if !mct {
        return (a, bb, cc);
    }
    if reversible {
        // RCT (inverse): G = Y - floor((Cb + Cr)/4); R = Cr + G; B = Cb + G.
        let g = a - ((bb + cc) / 4.0).floor();
        (cc + g, g, bb + g)
    } else {
        // ICT (inverse), the usual YCbCr matrix.
        (
            a + 1.402 * cc,
            a - 0.344_136 * bb - 0.714_136 * cc,
            a + 1.772 * bb,
        )
    }
}

/// Inverse component transform and DC level shift every pixel into 8-bit RGB.
fn color_planes_to_rgb(
    n: usize,
    ncomp: usize,
    planes: &[Vec<f32>],
    mct: bool,
    reversible: bool,
    shift: f32,
    scale: f32,
) -> Vec<u8> {
    let mut rgb = vec![0u8; n * 3];
    for i in 0..n {
        let (r, g, b) = if ncomp >= 3 {
            let (a, bb, cc) = (planes[0][i], planes[1][i], planes[2][i]);
            mct_pixel(a, bb, cc, mct, reversible)
        } else {
            let v = planes[0][i];
            (v, v, v)
        };
        for (k, v) in [r, g, b].into_iter().enumerate() {
            let s = (v + shift) * scale;
            rgb[i * 3 + k] = s.clamp(0.0, 255.0) as u8;
        }
    }
    rgb
}

/// Decode to RGB8 (or gray expanded to RGB) at the smallest resolution level that is still
/// at least `target_edge` on its long side.
///
/// Returns the pixels plus the decoded dimensions, which will be >= the target and are the
/// caller's to resize down precisely.
pub fn decode_reduced(bytes: &[u8], target_edge: u32) -> Result<(Vec<u8>, u32, u32), Jp2Error> {
    let (cs, palette) = codestream::find_codestream_and_palette(bytes)?;
    let c = codestream::parse(cs)?;
    let ncomp = validate_reduced_scope(&c, palette.is_some())?;

    let full_w = c.siz.width();
    let full_h = c.siz.height();
    let levels = c.cod.levels as u32;
    let (drop, keep) = choose_reduction(full_w, full_h, levels, target_edge);

    let out_w = full_w.div_ceil(1 << drop).max(1);
    let out_h = full_h.div_ceil(1 << drop).max(1);
    let nplanes = used_components(ncomp);
    check_reduced_alloc_budget(out_w, out_h, nplanes)?;

    // One plane per component the output reads, at the reduced size.
    let mut planes: Vec<Vec<f32>> = (0..nplanes)
        .map(|_| vec![0.0f32; (out_w as usize) * (out_h as usize)])
        .collect();

    decode_all_tiles(&c, keep, drop, &mut planes, out_w, out_h)?;

    let n = (out_w as usize) * (out_h as usize);
    let prec = c.siz.components[0].prec as u32 + 1;
    let signed = c.siz.components[0].signed;
    let shift = if signed {
        0.0
    } else {
        (1i64 << (prec - 1)) as f32
    };

    if let Some(pal) = palette {
        return Ok((palette_to_rgb(n, &planes[0], &pal, shift), out_w, out_h));
    }

    let scale = if prec >= 8 {
        1.0 / ((1u64 << (prec - 8)) as f32)
    } else {
        (1u64 << (8 - prec)) as f32
    };
    let mct = c.cod.mct && ncomp >= 3;
    let rgb = color_planes_to_rgb(n, ncomp, &planes, mct, c.cod.reversible, shift, scale);
    Ok((rgb, out_w, out_h))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod dim_tests;

/// A one-component, one-tile codestream whose header multipliers are the caller's (image
/// edge, decomposition levels, layer count, progression, one precinct byte repeated per
/// resolution), followed by a single tile-part carrying `body` bytes. Code-blocks are the
/// smallest legal 4x4 so the block count is at its maximum.
///
/// Lives at module scope (moved out of `fuzz_tests`, which is `#[cfg(test)]`-only) so
/// `fuzzapi` — and `crate::fuzz`'s harness, once wired to it — can build a codestream
/// without reaching into a private test module. `fuzz_tests` still calls it unqualified via
/// its own `use super::*;`.
#[cfg(test)]
pub(crate) fn hostile_codestream(
    edge: u32,
    levels: u8,
    layers: u16,
    progression: u8,
    precinct_byte: Option<u8>,
    body: &[u8],
) -> Vec<u8> {
    let mut cs: Vec<u8> = vec![0xFF, 0x4F]; // SOC

    let mut siz = vec![0u8; 36];
    siz[2..6].copy_from_slice(&edge.to_be_bytes()); // Xsiz
    siz[6..10].copy_from_slice(&edge.to_be_bytes()); // Ysiz
    siz[18..22].copy_from_slice(&edge.to_be_bytes()); // XTsiz
    siz[22..26].copy_from_slice(&edge.to_be_bytes()); // YTsiz
    siz[34..36].copy_from_slice(&1u16.to_be_bytes()); // Csiz
    siz.extend_from_slice(&[7, 1, 1]); // Ssiz, XRsiz, YRsiz
    cs.extend_from_slice(&[0xFF, 0x51]);
    cs.extend_from_slice(&((siz.len() + 2) as u16).to_be_bytes());
    cs.extend_from_slice(&siz);

    let mut cod = vec![u8::from(precinct_byte.is_some()), progression];
    cod.extend_from_slice(&layers.to_be_bytes());
    cod.extend_from_slice(&[0, levels, 0, 0, 0, 1]); // MCT, NL, cbw, cbh, style, 5/3
    if let Some(pb) = precinct_byte {
        cod.extend(std::iter::repeat_n(pb, levels as usize + 1));
    }
    cs.extend_from_slice(&[0xFF, 0x52]);
    cs.extend_from_slice(&((cod.len() + 2) as u16).to_be_bytes());
    cs.extend_from_slice(&cod);

    // QCD style 0, two guard bits, one exponent byte per subband.
    let mut qcd = vec![0x40u8];
    qcd.extend(std::iter::repeat_n(8u8 << 3, 3 * levels as usize + 1));
    cs.extend_from_slice(&[0xFF, 0x5C]);
    cs.extend_from_slice(&((qcd.len() + 2) as u16).to_be_bytes());
    cs.extend_from_slice(&qcd);

    // One tile-part: SOT (Psot spans marker, segment, SOD and body), SOD, body.
    cs.extend_from_slice(&[0xFF, 0x90, 0x00, 0x0A]);
    cs.extend_from_slice(&0u16.to_be_bytes()); // Isot
    cs.extend_from_slice(&((14 + body.len()) as u32).to_be_bytes()); // Psot
    cs.extend_from_slice(&[0x00, 0x01]); // TPsot, TNsot
    cs.extend_from_slice(&[0xFF, 0x93]); // SOD
    cs.extend_from_slice(body);
    cs.extend_from_slice(&[0xFF, 0xD9]); // EOC
    cs
}

/// Direct fuzz entry points into `dimensions` and `decode_reduced`, plus the seed this
/// module's mutation fuzz needs to reach either one — JPEG 2000's own mutation-fuzz harness
/// (`fuzz_tests` below) already red-teams this code, but was never wired into
/// `crate::fuzz`'s cross-format harness, which is test-only itself, so `#[cfg(test)]` here
/// changes no shipped behavior.
#[cfg(test)]
#[doc(hidden)]
pub(crate) mod fuzzapi;
mod resolution;
use resolution::*;
mod packets;
use packets::*;
mod reconstruct;
use reconstruct::*;

#[cfg(test)]
mod fuzz_tests;

#[cfg(test)]
mod exactness_tests;
