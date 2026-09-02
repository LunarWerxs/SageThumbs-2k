//! Ebook / comic-archive cover extraction — the native-Rust port of DarkThumbs.
//!
//! The shell hands us a byte stream with no extension, so we CONTENT-SNIFF the
//! container by its magic bytes and pull out the cover image. The cover bytes
//! then flow back through the normal tiered decoder (`decode::decode_full` ->
//! `decode_image`), so we add zero new image-decode code — only cover *finding*.
//!
//! Everything here runs in Explorer's thumbnail host under `panic = "abort"`, so
//! every parser works on `&[u8]` with checked slicing and bounded allocation; on
//! any malformed input we return `None` and the shell shows the default icon.

use image::DynamicImage;

/// Combined `Read + Seek` for dynamic dispatch. Funneling every `lofty` read through one
/// `&mut dyn ReadSeek` instead of a fresh monomorphization per concrete reader type (Cursor /
/// the shell IStream / `BufReader<File>` plus lofty's internal `Take`/`Unsynchronized` wrappers,
/// ~9 copies) trims ~400 KB off the DLL with identical behavior. Used by the audio cover
/// extractor ([`audio`]) and [`crate::strip::read_audio_tags`].
pub(crate) trait ReadSeek: std::io::Read + std::io::Seek {}
impl<T: std::io::Read + std::io::Seek + ?Sized> ReadSeek for T {}

mod affinity;
// Shared checked box-header size arithmetic for every ISO-BMFF-family box walker
// in the tree (mp4.rs, streamsrc/mp4remux.rs, decode/{color,magick}.rs, strip/isobmff.rs).
pub(crate) mod boxhdr;
// Android packages (.apk) + split-bundle wrappers (.xapk/.apks/.apkm) — the
// manifest-declared launcher icon, resolved through binary XML + resources.arsc.
mod apk;
mod audio;
mod blend;
mod icns;
// Cinema 4D (.c4d) — carve the embedded document/scene preview JPEG.
mod c4d;
// CorelDRAW (.cdr/.cdt) / Corel Exchange (.cmx) — RIFF DISP preview DIB → BMP.
mod cdr;
mod clip;
// Contact-sheet compositor for generic archive thumbnails (2-4 images, one tile).
pub(crate) mod collage;
// DjVu (.djvu) cover decode — via the maintained pure-Rust `djvu-rs` crate (see djvu.rs).
mod djvu;
mod dwg;
mod eps;
pub(crate) use eps::is_eps;
mod epub;
mod fb2;
mod gcode;
mod indd;
// Amiga / Deluxe Paint IFF ILBM (.iff/.ilbm/.lbm) — a real planar-bitmap decoder.
mod ilbm;
mod max;
mod mobi;
mod office;
pub mod ole;
mod pdn;
mod project;
mod psd;
mod psp;
mod rar;
mod rhino;
pub(crate) mod select;
mod sevenz;
mod skp;
mod tarfmt;
mod util;
// GIMP XCF (.xcf) — native decoder; ImageMagick can't read the modern v011 format.
mod xcf;

/// Decode a GIMP `.xcf` from a seekable source without buffering the file.
///
/// Exposed because `.xcf` is the one format where the whole-file ceiling is not a nuisance but
/// a wall: it bakes in no preview to carve out of a prefix, and Windows has no codec for it, so
/// every other oversized-file rescue declines and a big GIMP file gets the stock icon. Its own
/// decoder is a walk over absolute file offsets, so it can read only the pieces it needs.
///
/// `target_edge` is the longest side the caller can actually use. Passing it lets the decoder
/// flatten on a REDUCED grid instead of building the full canvas and throwing it away, which
/// on a big layered file is the difference between ten seconds and twenty milliseconds. `None`
/// keeps the full-resolution path for callers that want real pixels.
pub(crate) fn xcf_from_reader<R: std::io::Read + std::io::Seek>(
    src: R,
    target_edge: Option<u32>,
) -> Option<image::DynamicImage> {
    xcf::extract_seek(src, target_edge)
}

/// [`xcf_from_reader`] for bytes already in hand.
pub(crate) fn xcf_from_bytes_scaled(
    bytes: &[u8],
    target_edge: Option<u32>,
) -> Option<image::DynamicImage> {
    xcf::extract_scaled(bytes, target_edge)
}

/// Cheap magic test for the above, so a caller can route before it commits to a read.
pub(crate) fn looks_like_xcf(bytes: &[u8]) -> bool {
    xcf::looks_like_xcf(bytes)
}

/// Decode a DjVu cover for a caller that knows the longest side it can use.
///
/// Unlike [`xcf_from_bytes_scaled`] this is NOT about doing less work: a DjVu render costs what
/// its JB2 mask and IW44 background cost, whatever size they are composited into, and shrinking
/// the render only degrades the picture (see `djvu::RENDER_CAP`). The target is here to answer
/// one question the decoder cannot answer without it - is the file's baked TH44 thumbnail big
/// enough to serve THIS request? It is capped at 128 px, so it answers a 96 px icon view for
/// almost nothing and must be rendered past for anything larger. `extract_cover` carries no
/// target, so it has to assume the largest, which is right for Convert and wasteful for a
/// 96 px tile. `None` here means the same.
pub(crate) fn djvu_from_bytes_scaled(
    bytes: &[u8],
    target_edge: Option<u32>,
) -> Option<image::DynamicImage> {
    djvu::extract_scaled(bytes, target_edge)
}

/// Cheap magic test for the above (IFF85 "AT&TFORM"), so a caller can route before it commits.
pub(crate) fn looks_like_djvu(bytes: &[u8]) -> bool {
    bytes.starts_with(b"AT&TFORM")
}
// Waveform thumbnails for raw-PCM audio (WAV/AIFF) with no embedded cover art.
mod waveform;
mod zipfmt;
// Synthetic, structurally-valid seeds + direct fuzz entry points for the extractors above.
// Test-only. Lives inside `container` because the format modules are private to it — see the
// module docs for why CI needed this at all.
#[cfg(test)]
pub(crate) mod fuzzseed;
// Test-only re-export so `crate::fuzz` can aim at the APK sub-parsers directly (a zip's CRC
// check stops any mutation from reaching them through `apk::extract` — see `fuzz::inner_targets`).
// The format modules themselves stay private to `container`.
#[cfg(test)]
pub(crate) use apk::fuzzapi as apk_fuzzapi;

/// A cover: either raw image-file bytes (re-decoded by the image tiers) or
/// already-decoded pixels (DjVu, which is not a standalone image file).
pub enum CoverOut {
    Bytes(Vec<u8>),
    Image(DynamicImage),
}

/// Max bytes we'll read for one cover entry (DarkThumbs' CBXMEM cap, 32 MiB).
pub(crate) const MAX_COVER: u64 = 32 * 1024 * 1024;

/// Do the leading bytes look like a raster image format our tiers can actually
/// render (JPEG / PNG / GIF / BMP / WebP)? Container extractors use this to reject
/// embedded previews we can't decode (e.g. EMF/WMF). Shared magic-byte predicate
/// for `office`, `project`, and `mobi` so the accept set stays in one place.
pub(crate) fn looks_like_raster(data: &[u8]) -> bool {
    data.starts_with(&[0xFF, 0xD8, 0xFF]) // JPEG
        || data.starts_with(&[0x89, b'P', b'N', b'G']) // PNG
        || data.starts_with(b"GIF8") // GIF
        || data.starts_with(b"BM") // BMP
        || (data.len() >= 12 && &data[0..4] == b"RIFF" && &data[8..12] == b"WEBP") // WebP
        // Windows metafiles (EMF / placeable + memory WMF) — decodable via the magick
        // tier (e.g. Visio docProps/thumbnail.emf). Shares decode::looks_like_metafile
        // so the magic bytes live in exactly one place.
        || crate::decode::looks_like_metafile(data)
}

/// ZIP-family signature (local-file / central-dir / end-of-central-dir headers).
fn is_zip(b: &[u8]) -> bool {
    b.starts_with(b"PK\x03\x04") || b.starts_with(b"PK\x05\x06") || b.starts_with(b"PK\x07\x08")
}

/// 7-Zip signature.
pub(crate) fn is_7z(b: &[u8]) -> bool {
    b.starts_with(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C])
}

/// RAR signature (RAR 1.5–4.x `Rar!\x1a\x07\x00` and RAR5 `Rar!\x1a\x07\x01\x00` share this prefix).
fn is_rar(b: &[u8]) -> bool {
    b.starts_with(b"Rar!\x1a\x07")
}

/// List an archive's entries — `(name, uncompressed_size, is_dir)` — WITHOUT extracting anything
/// (central-directory / header read only, so no decompression-bomb risk). Dispatches by signature
/// across ZIP-family, 7-Zip, and RAR. The count is capped so a pathological archive with millions
/// of tiny entries can't stall the viewer. `None` if `bytes` isn't a recognized archive.
/// Cap on how many archive entries any listing/selection path will materialize —
/// a crafted archive whose directory declares millions of entries must never
/// drive millions of `String` allocations (in the viewer's UI thread OR the
/// thumbnail host's cover pick). Shared by [`list_archive`] and every
/// `pick_covers` listing (zip/7z/rar).
pub(crate) const MAX_LIST_ENTRIES: usize = 50_000;

pub fn list_archive(bytes: &[u8]) -> Option<Vec<(String, u64, bool)>> {
    const MAX_ENTRIES: usize = MAX_LIST_ENTRIES;
    // The cap is passed INTO each reader so it bounds the collection itself — a crafted archive
    // with millions of tiny entries never materializes millions of `String`s (which, on the UI
    // thread in `content::archive_listing`, would freeze the viewer).
    let entries = if is_zip(bytes) {
        zipfmt::list_bytes(bytes, MAX_ENTRIES)?
    } else if is_7z(bytes) {
        sevenz::list(bytes, MAX_ENTRIES)?
    } else if is_rar(bytes) {
        rar::list(bytes, MAX_ENTRIES)?
    } else {
        return None;
    };
    Some(
        entries
            .into_iter()
            .map(|e| (e.name, e.size, e.is_dir))
            .collect(),
    )
}

/// Does `head` (the first bytes of a file) look like an audio container that may
/// carry embedded cover art? Lets the thumbnail provider take the memory-light
/// seek path instead of reading the whole (possibly huge) file.
pub fn looks_like_audio(head: &[u8]) -> bool {
    audio::looks_like_audio(head)
}

/// Album art from a seekable reader (the shell's IStream). lofty seeks to the
/// metadata, so we read only what's needed to reach the picture — no whole-file
/// read, hence no size cap on audio.
pub fn audio_art_from_reader<R: std::io::Read + std::io::Seek>(reader: R) -> Option<Vec<u8>> {
    audio::extract_reader(reader)
}

pub(crate) use audio::AsfTags;

/// Artist/album/title/track from an ASF/WMA file (lofty can't read ASF, so the
/// `strip::read_audio_tags` lofty path would return nothing). `None` for non-ASF
/// input → the caller falls back to lofty for every other audio format.
pub(crate) fn audio_asf_tags<R: std::io::Read + std::io::Seek>(reader: &mut R) -> Option<AsfTags> {
    audio::asf_tags(reader)
}

/// Does `head` open a container whose baked-in preview lives in the FIRST bytes of
/// the file — so a bounded head prefix is enough to thumbnail it, no matter how big
/// the file is? Blender writes the `TEST` thumbnail block right after the file
/// header (offset ~100), and Photoshop's image-resources section (resource 1036,
/// the baked JPEG preview) sits just past the fixed header — both LONG before the
/// scene/layer data that makes these files routinely blow past the thumbnail
/// provider's MaxSize cap. (Compressed .blend has no `BLENDER` magic and correctly
/// stays excluded.) Used by the provider's oversized-file path and the CLI preview
/// verbs to rescue exactly these formats from the size skip.
pub fn has_head_preview(head: &[u8]) -> bool {
    head.starts_with(b"BLENDER") // .blend / .blend1..32 (TEST block)
        || head.starts_with(b"8BPS") // PSD + PSB (image resource 1036)
        // gzip / zstd: a COMPRESSED .blend hides its BLENDER magic behind the
        // wrapper, but the TEST block still sits at the head of the decompressed
        // stream (see `blend_compressed_head`). This over-accepts other oversized
        // gzip/zstd files, but the attempt is bounded (16 MiB prefix + capped
        // inflate) and a miss lands on the default icon exactly as before.
        || head.starts_with(&[0x1F, 0x8B])
        || head.starts_with(&[0x28, 0xB5, 0x2F, 0xFD])
}

/// If `bytes` open a gzip or zstd stream whose DECOMPRESSED head is a Blender file
/// (the "Compress" save option — gzip historically, zstd since Blender 3.0), return
/// a bounded decompressed prefix for `blend::extract`. The inner magic is peeked
/// FIRST (12 bytes) so non-Blender gzip payloads (`.svgz`/`.emz`) skip the big
/// inflate. Truncation-tolerant: the input may itself be a bounded prefix of an
/// oversized file, so a mid-stream EOF keeps whatever decompressed so far — the
/// TEST block lives in the first kilobytes, far inside any such prefix. Output is
/// capped (decompression-bomb guard) and the caller feeds it straight to
/// `blend::extract`, never back through `extract_cover` — no recursion.
fn blend_compressed_head(bytes: &[u8]) -> Option<Vec<u8>> {
    use std::io::Read;
    const HEAD_MAX: usize = 16 * 1024 * 1024;
    let mut reader: Box<dyn Read + '_> = if bytes.starts_with(&[0x1F, 0x8B]) {
        Box::new(flate2::read::GzDecoder::new(bytes))
    } else if bytes.starts_with(&[0x28, 0xB5, 0x2F, 0xFD]) {
        Box::new(ruzstd::decoding::StreamingDecoder::new(bytes).ok()?)
    } else {
        return None;
    };
    let mut magic = [0u8; 12];
    reader.read_exact(&mut magic).ok()?;
    if !magic.starts_with(b"BLENDER") {
        return None;
    }
    let mut out = magic.to_vec();
    let mut chunk = vec![0u8; 1 << 16];
    while out.len() < HEAD_MAX {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => out.extend_from_slice(&chunk[..n]),
            // Truncated input (a bounded prefix of an oversized file): keep what
            // we have — the thumbnail block is at the head.
            Err(_) => break,
        }
    }
    Some(out)
}

/// If `bytes` is a recognized ebook/comic container, return its cover image.
///
/// Sniffed in four groups, tried in order (same overall priority as before the
/// split — see each group's own doc comment for why a group's internal order
/// matters, e.g. APK-before-zip and PSD-falls-through-to-the-magick-tier).
pub fn extract_cover(bytes: &[u8]) -> Option<CoverOut> {
    try_generic_archive_cover(bytes)
        .or_else(|| try_creative_app_cover(bytes))
        .or_else(|| try_ebook_and_cad_cover(bytes))
        .or_else(|| try_misc_cover(bytes))
}

/// Generic archive containers: APK/zip/7z/rar.
fn try_generic_archive_cover(bytes: &[u8]) -> Option<CoverOut> {
    // ZIP family: Android packages (and their split-bundle wrappers) FIRST, then
    // EPUB / CBZ / FBZ / any zip of images. The archive is opened and its central
    // directory parsed exactly ONCE and shared between the APK dispatch check and
    // whichever extractor claims it — `apk::looks_like_apk` used to open its own
    // `ZipArchive` for the check and `apk::extract`/`zipfmt::extract` opened a
    // second one for the real read, parsing the same directory twice per file.
    if is_zip(bytes) {
        let Ok(mut zip) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else {
            return None;
        };
        // Android packages and their split-bundle wrappers: the REAL launcher icon
        // via AndroidManifest.xml / resources.arsc. Must stay BEFORE the generic
        // zip branch — an APK is a zip, and the generic image-pick would grab an
        // arbitrary res/ drawable instead of the declared icon. A `.apk`-suffixed
        // entry that turns out not to be a real wrapper (nothing resolvable behind
        // it) falls through to the generic zip cover pick below instead of losing
        // the cover entirely.
        if apk::archive_is_apk(&mut zip) {
            if let Some(icon) = apk::extract_from_archive(&mut zip) {
                return Some(CoverOut::Bytes(icon));
            }
        }
        return zipfmt::extract_from_archive(&mut zip).map(CoverOut::Bytes);
    }
    // 7-Zip: CB7.
    if is_7z(bytes) {
        return sevenz::extract(bytes).map(CoverOut::Bytes);
    }
    // RAR 4.x ("Rar!\x1A\x07\x00") and 5.x ("Rar!\x1A\x07\x01\x00"): CBR. Pure-Rust
    // `rars` — always available now (no feature gate).
    if bytes.starts_with(b"Rar!\x1A\x07") {
        return rar::extract(bytes).map(CoverOut::Bytes);
    }
    None
}

/// Creative-app native formats: DjVu, GIMP, Paint.NET, Photoshop, EPS, icns,
/// Blender, Affinity, Paint Shop Pro, ILBM, Cinema 4D, CorelDRAW, Clip Studio.
fn try_creative_app_cover(bytes: &[u8]) -> Option<CoverOut> {
    // DjVu (IFF85 magic "AT&TFORM").
    if looks_like_djvu(bytes) {
        return djvu::extract(bytes).map(CoverOut::Image);
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
    // Photoshop PSD/PSB: the baked-in JPEG thumbnail (resource 1036). Works with
    // no ImageMagick; on None we fall through so a full install can still render
    // the layers via the magick tier.
    if bytes.starts_with(b"8BPS") {
        if let Some(thumb) = psd::extract(bytes) {
            return Some(CoverOut::Bytes(thumb));
        }
    }
    // DOS-EPS: the baked-in TIFF screen preview (real PS rendering would need
    // Ghostscript). A WMF-only/bare file stays terminally unsupported in the
    // decoder instead of falling through to any PostScript-capable external tier.
    if bytes.starts_with(&[0xC5, 0xD0, 0xD3, 0xC6]) {
        if let Some(tiff) = eps::extract(bytes) {
            return Some(CoverOut::Bytes(tiff));
        }
    }
    // Plain EPS: only read an already-embedded EPSI/Photoshop raster preview;
    // never invoke a PostScript interpreter in the thumbnail host.
    if bytes.starts_with(b"%!PS") {
        if let Some(cover) = eps::extract_ascii_preview(bytes) {
            return Some(cover);
        }
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

/// Ebook/comic and CAD/office containers: Mobi, CBT, FB2, SketchUp, DWG,
/// Rhino, InDesign, and OLE2 (3ds Max / legacy Office).
fn try_ebook_and_cad_cover(bytes: &[u8]) -> Option<CoverOut> {
    // Kindle / Mobipocket: PalmDB type+creator "BOOKMOBI" at offset 60.
    if bytes.len() > 68 && &bytes[60..68] == b"BOOKMOBI" {
        return mobi::extract(bytes).map(CoverOut::Bytes);
    }
    // TAR-based comic (CBT): "ustar" magic at offset 257.
    if bytes.len() > 262 && &bytes[257..262] == b"ustar" {
        return tarfmt::extract(bytes).map(CoverOut::Bytes);
    }
    // FictionBook 2: XML containing "<FictionBook".
    if fb2::looks_like_fb2(bytes) {
        return fb2::extract(bytes).map(CoverOut::Bytes);
    }
    // SketchUp .skp: "SketchUp Model" header → carve the embedded thumbnail PNG.
    if skp::looks_like_skp(bytes) {
        return skp::extract(bytes).map(CoverOut::Bytes);
    }
    // AutoCAD .dwg: "AC10xx" header → preview section (PNG / DIB→BMP / WMF).
    if dwg::looks_like_dwg(bytes) {
        return dwg::extract(bytes).map(CoverOut::Bytes);
    }
    // Rhino .3dm: "3D Geometry File Format" → zlib-inflated DIB preview.
    if rhino::looks_like_3dm(bytes) {
        return rhino::extract(bytes).map(CoverOut::Bytes);
    }
    // Adobe InDesign .indd: master-GUID header → base64 JPEG in the XMP packet.
    if indd::looks_like_indd(bytes) {
        return indd::extract(bytes).map(CoverOut::Bytes);
    }
    // OLE2 compound file (3ds Max .max, legacy Office/Visio/Publisher): the
    // \x05SummaryInformation thumbnail. `extract` returns a CoverOut directly
    // (raw RGB → pixels, or a CF_DIB → BMP bytes).
    if max::looks_like_max(bytes) {
        return max::extract(bytes);
    }
    None
}

/// Everything else: audio album art, then the G-code last resort.
fn try_misc_cover(bytes: &[u8]) -> Option<CoverOut> {
    // Audio with embedded album art (MP3/FLAC/Ogg/Opus/M4A/WMA/APE/…).
    if audio::looks_like_audio(bytes) {
        return audio::extract(bytes).map(CoverOut::Bytes);
    }
    // 3D-printer G-code with an embedded base64 PNG preview (text scan; bails
    // fast on binary, so it's a cheap last resort).
    if let Some(png) = gcode::extract(bytes) {
        return Some(CoverOut::Bytes(png));
    }
    None
}

/// Stream a cover from an OVERSIZED container (past the in-memory size cap) using a
/// seekable reader — the shell's IStream — so a multi-hundred-MB file thumbnails
/// without ever buffering it. ZIP-family and 7-Zip archives seek to the central
/// directory + one cover entry; Clip Studio `.clip` seeks to the embedded SQLite
/// database at the file's tail and reads only that (a big canvas's bulk is layer
/// raster chunks we never touch). RAR can't stream (the `rars` crate needs the full
/// buffer), so a giant CBR still falls through to the default icon. `head` is the
/// first bytes (already peeked) for the magic sniff.
pub fn archive_cover_seek<R: std::io::Read + std::io::Seek>(
    reader: R,
    head: &[u8],
    prefs: &select::CoverPrefs,
) -> Option<Vec<u8>> {
    // ZIP family: CBZ / ZIP (and any zip of images).
    if is_zip(head) {
        // APK FIRST, exactly as in the buffered path above. An Android package IS a zip, so
        // without this an oversized one takes the generic cover pick and shows an arbitrary
        // bundled drawable instead of its launcher icon. Oversized is not the rare case here:
        // `.xapk`/`.apks` split bundles for big games are precisely the ones that pass
        // `limits::MAX_INPUT_BYTES` and reach this path rather than the buffered one.
        // The already-open archive is shared with `extract_from_archive` instead of being
        // re-parsed, and a `.apk`-suffixed entry that turns out not to be a real wrapper
        // falls through to the generic pick below instead of losing the cover entirely.
        let Ok(mut zip) = zip::ZipArchive::new(reader) else {
            return None;
        };
        if apk::archive_is_apk(&mut zip) {
            if let Some(icon) = apk::extract_from_archive(&mut zip) {
                return Some(icon);
            }
        }
        return zipfmt::cover_from_reader(zip.into_inner(), prefs);
    }
    // 7-Zip: CB7.
    if is_7z(head) {
        return sevenz::extract_seek(reader, prefs);
    }
    // Clip Studio Paint: the preview PNG from the tail CHNKSQLi database.
    if head.starts_with(b"CSFCHUNK") {
        return clip::extract_seek(reader);
    }
    None
}

/// Is `head` the signature of a generic archive we thumbnail (.zip / .7z / .rar)?
/// The streamsrc archive branch uses this to decide the probe is worth a
/// `Stat`-name check at all. RAR is included here even though it can't stream —
/// its caller takes the bounded in-memory path instead.
pub fn is_generic_archive_magic(head: &[u8]) -> bool {
    is_zip(head) || is_7z(head) || is_rar(head)
}

/// Does streaming this archive need the FULL in-memory buffer? Only RAR: `rars`
/// accepts no `Read + Seek` source, while zip/7z read the entry list and the
/// picked entries directly off a seekable reader.
pub fn archive_needs_buffer(head: &[u8]) -> bool {
    is_rar(head)
}

/// Up to `want` cover images from a GENERIC archive buffer (.zip/.rar/.7z), for
/// the contact-sheet thumbnail — cover-named images first, then natural-sorted
/// pages ([`select::pick_covers`]). Listing is header/central-directory only;
/// extraction is bounded per entry ([`MAX_COVER`]) and, for solid archives, one
/// budgeted sequential pass. `None` when the archive holds no readable image —
/// the caller fails the thumbnail and Explorer shows the stock icon.
pub fn archive_covers(
    bytes: &[u8],
    want: usize,
    prefs: &select::CoverPrefs,
) -> Option<Vec<Vec<u8>>> {
    if is_zip(bytes) {
        return zipfmt::covers_from_reader(std::io::Cursor::new(bytes), want, prefs);
    }
    if is_7z(bytes) {
        return sevenz::extract_seek_n(std::io::Cursor::new(bytes), want, prefs);
    }
    if is_rar(bytes) {
        return rar::extract_n(bytes, want, prefs);
    }
    None
}

/// Streaming [`archive_covers`] over a seekable reader (the shell's IStream or an
/// open `File`): the entry LIST comes from the central directory / archive header
/// (zip stores it at the tail — one seek, a few KB), then only the picked entries
/// are read. A multi-GB zip of photos costs its directory plus 4 images. ZIP and
/// 7z only ([`archive_needs_buffer`] — RAR goes through the in-memory variant).
pub fn archive_covers_seek<R: std::io::Read + std::io::Seek>(
    reader: R,
    head: &[u8],
    want: usize,
    prefs: &select::CoverPrefs,
) -> Option<Vec<Vec<u8>>> {
    if is_zip(head) {
        return zipfmt::covers_from_reader(reader, want, prefs);
    }
    if is_7z(head) {
        return sevenz::extract_seek_n(reader, want, prefs);
    }
    None
}

/// REAL pixel dimensions of the underlying document, for container formats whose
/// extracted cover is only a small baked-in preview (PSD/PSB today). Captions /
/// info displays show these instead of the preview's dimensions — a 4700×800 PSD
/// must not read "160 × 26 px" just because its thumbnail does.
pub fn real_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.starts_with(b"8BPS") {
        return psd::header_dims(bytes);
    }
    // JPEG 2000 has no `image`-crate probe, so "Image info" used to fall all the way
    // through to a full decode and then report the DECODED size. That decode is capped at
    // 4096 px, so a 9958x7686 scan confidently reported itself as 4096x3161 — a wrong
    // number, not merely a slow one. Reading the codestream's SIZ marker answers it exactly,
    // from the header, with no decode at all.
    if let Some(d) = crate::decode::jp2_dimensions(bytes) {
        return Some(d);
    }
    None
}

/// [`real_dims`], falling back to a full decode's dimensions when the cheap header probe
/// doesn't recognise the format. This exact two-step fallback — the container header probe,
/// then a full [`crate::decode::decode_full`] — used to be hand-copied verbatim at every
/// dims-probing call site that needed it (`strip::read_info_impl`, `strip::read_info_verbose`,
/// `verbs::fileops::dims`); one shared implementation here so a future change to the chain
/// can't update some copies and miss others.
///
/// `decode_full` is the EXPENSIVE tier (it can spawn an ImageMagick subprocess), so a caller
/// that must stay cheap for a bulk probe (e.g. `fileops::page_dims_from_head`, run once per
/// CBZ page) should keep calling [`real_dims`] alone instead of this.
pub(crate) fn real_or_decoded_dims(bytes: &[u8]) -> Option<(u32, u32)> {
    real_dims(bytes).or_else(|| {
        crate::decode::decode_full(bytes)
            .ok()
            .map(|i| (i.width(), i.height()))
    })
}

/// True when a PSD/PSB document is transparent (its merged composite has an alpha
/// channel). The baked-in preview (resource 1036) is a JPEG with no alpha, so the
/// thumbnail/preview path renders the real layer composite for these instead of a
/// flat white preview. See [`psd::has_alpha`].
pub fn psd_has_alpha(bytes: &[u8]) -> bool {
    psd::has_alpha(bytes)
}

/// Long edge of the preview a PSD/PSB bakes into resource 1036, measured without decoding
/// it. `None` when there is none, or none we can measure. See [`psd::preview_dims`].
pub fn psd_preview_long_edge(bytes: &[u8]) -> Option<u32> {
    psd::preview_dims(bytes).map(|(w, h)| w.max(h))
}

/// The baked preview's own JPEG bytes, for a caller that needs to look at its PIXELS rather
/// than its size — `decode` compares it against a rendered composite before preferring one
/// over the other. Named apart from the internal extractor so the intent is visible at the
/// call site: this is the picture we would otherwise have shown.
pub fn psd_baked_preview(bytes: &[u8]) -> Option<Vec<u8>> {
    psd::extract(bytes)
}

/// Long edge of a head-baked preview, **but only for containers where reading the WHOLE file
/// would offer a better picture than that preview** (issue #33).
///
/// This is the question [`crate::streamsrc`]'s head-preview fast path has to answer before it
/// commits to a bounded prefix, and it is narrower than "how big is the preview". A `None`
/// means *do not second-guess the prefix* — and Blender and DWG answer `None` deliberately,
/// not by omission: their baked preview is the only picture in the file, so declining it would
/// buy the identical image for the price of reading the whole document. Photoshop is the one
/// member because a PSD carries a merged composite behind its ~160 px thumbnail, and
/// [`crate::decode`] can render it.
pub fn upgradable_head_preview_edge(bytes: &[u8]) -> Option<u32> {
    if !bytes.starts_with(b"8BPS") {
        return None;
    }
    psd_preview_long_edge(bytes)
}

/// Head-preview prefix sizing: how many leading bytes are enough to extract
/// this container's baked preview, or None when there's no bounded-prefix fast
/// path and the caller should read the whole file. `ext` is the file's lowercase
/// extension when the caller can recover one (G-code has no magic bytes, so it is
/// reachable ONLY by extension); magic-identified formats ignore it.
///
/// The members, and why each is safe to shorten:
///   * PSD/PSB — exact: header + Color Mode Data + the Image Resources section
///     ([`psd::preview_prefix_len`], which also bows out for transparent documents
///     that need the full file for their composite).
///   * `.dwg` — exact: the header seeker names the preview section, whose record
///     table names each payload ([`dwg::preview_prefix_len`]).
///   * plain `.blend` — the `blanket` cap; its TEST thumbnail sits ~100 bytes in.
///   * `.gcode`/`.gco` — [`gcode::SCAN_LIMIT`], which [`gcode::extract`] already
///     clamps to, so the shortened read is byte-identical to the whole-file one.
///
/// Deliberately EXCLUDED: the gzip/zstd wrappers that [`has_head_preview`]
/// over-accepts for the OVERSIZED rescue (under the cap they'd cost every ordinary
/// .gz/.svgz an extra bounded inflate for nothing), and every format whose preview
/// needs a tail index, a full scan, or a real pixel decode — a bounded prefix
/// cannot help those, and guessing one would just add a wasted read.
pub fn head_preview_len<R: std::io::Read + std::io::Seek>(
    head: &[u8],
    ext: Option<&str>,
    r: &mut R,
    blanket: u64,
) -> Option<u64> {
    if head.starts_with(b"8BPS") {
        return psd::preview_prefix_len(r);
    }
    if head.starts_with(b"BLENDER") {
        return Some(blanket);
    }
    if dwg::looks_like_dwg(head) {
        return dwg::preview_prefix_len(r);
    }
    if matches!(ext, Some("gcode" | "gco")) {
        return Some(gcode::SCAN_LIMIT as u64);
    }
    None
}

/// Raster-image extensions we accept as an archive cover. A curated subset of the
/// formats our decoder can read (NOT all of `formats::FORMATS` — most FORMATS
/// entries, e.g. ebook/audio/document types, are not valid cover images). Mirrors
/// DarkThumbs' IsImage set (common.cpp) — including ICO, the camera-RAW types,
/// JPEG-XR and HEIF that our WIC tier reads.
///
/// Kept as a const (not inlined in a `match`) so the set is greppable and the
/// `cover_exts_are_known_formats` test can assert it against `FORMATS`. Every
/// entry must be in `FORMATS` except the documented [`COVER_ONLY_EXCEPTIONS`].
pub(crate) const COVER_IMAGE_EXTS: &[&str] = &[
    "bmp", "ico", "gif", "jpg", "jpe", "jfif", "jpeg", "png", "tif", "tiff", "svg", "webp", "jxr",
    "nrw", "nef", "dng", "cr2", "heif", "heic", "avif", "jxl",
    // JPEG-2000 — decodes only on the full (ImageMagick/openjpeg) install, so
    // `select::pick_cover` treats these as a LAST RESORT: a .jp2 page never shadows a
    // sibling .jpg that the compact (no-magick) install could actually render.
    "jp2", "j2k", "jpf", "jpx", "jpm",
];

/// Cover extensions we accept that are intentionally NOT standalone `FORMATS`
/// entries: WIC can decode them as an archive cover, but we don't hook the bare
/// file type in Explorer. Keep this list as small as possible — if one of these
/// later joins `FORMATS`, the test forces it out of here. (Consumed only by the
/// drift tests, hence `allow(dead_code)` in non-test builds.)
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) const COVER_ONLY_EXCEPTIONS: &[&str] = &[
    // (Currently empty: `jxr` graduated into FORMATS as a hooked JPEG XR format, so it
    // now satisfies the cover-set check via FORMATS directly. Any future WIC-decodable
    // cover type that we deliberately DON'T hook as a standalone format goes here.)
];

/// Does `name` (a path inside an archive) have a raster-image extension we accept
/// as a cover? See [`COVER_IMAGE_EXTS`].
pub(crate) fn is_image_name(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    COVER_IMAGE_EXTS.contains(&ext.as_str())
}

/// Test-only re-export so the `decode` oversized-path tests can build synthetic
/// `.clip` files without reaching into the private `clip` module.
#[cfg(test)]
pub(crate) use clip::testutil as clip_testutil;

#[cfg(test)]
pub(crate) use blend::testutil as blend_testutil;
#[cfg(test)]
pub(crate) use dwg::testutil as dwg_testutil;
/// Test-only re-exports so the `decode`/`streamsrc` head-preview fast-path tests
/// can build synthetic PSD/DWG files without reaching into the private modules.
#[cfg(test)]
pub(crate) use psd::testutil as psd_testutil;

/// Shared embedded-JPEG span scanner — see [`util::jpeg_span_len`]. Re-exported so
/// `decode` and the container extractors (PSP, C4D) don't each hand-roll their own.
pub(crate) use util::{jpeg_sof_is_decodable, jpeg_span, jpeg_span_len};

#[cfg(test)]
mod tests {
    use super::*;

    /// Registry-default cover prefs, for tests that don't care about the values.
    fn default_prefs() -> select::CoverPrefs {
        select::CoverPrefs {
            prefer_cover: true,
            sort: true,
            skip_scanlation: false,
        }
    }

    /// **The assertion every gate in this repo was missing, applied to every extractor at
    /// once.** `extract_cover` returning `Some` proves nothing: the InDesign carver returned
    /// a spliced JPEG that started `FFD8FF`, ended `FFD9`, was the largest candidate in the
    /// file, and decoded to a few rows of page over flat grey. It shipped, because the render
    /// sweep asked for a non-empty PNG and got one. So: run the real dispatcher over every
    /// real corpus sample and require that whatever comes back ACTUALLY DECODES.
    ///
    /// One test rather than 26 per-module ones on purpose. It goes through
    /// [`extract_cover`], so a new format is covered the moment its magic is wired into the
    /// dispatch, with no second list to keep in step.
    ///
    /// Skipped when the corpus is absent (it is a sibling of the repo and CI never checks it
    /// out). That is a real gap, not a pretend one — see `container::fuzzseed`, which exists
    /// because of the same absence.
    /// Split into a gate and a sweep for the same reason the fuzzer is: the whole corpus at
    /// 64 MiB costs ~204 s in a debug build, which is more than the rest of `cargo test`
    /// put together. At 8 MiB it is a few seconds and still covers EVERY cover-bearing
    /// sample — the files above that line are camera RAW, which the container dispatcher
    /// declines on magic and never carves anyway.
    #[test]
    fn every_corpus_cover_actually_decodes() {
        corpus_covers_decode(8 * 1024 * 1024);
    }

    /// The same assertion with the ceiling lifted. Run before a release, and after touching
    /// any extractor that handles large containers:
    ///   cargo test --release --lib every_corpus_cover -- --ignored
    #[test]
    #[ignore = "slow sweep — run on demand with --ignored"]
    fn every_corpus_cover_actually_decodes_full() {
        corpus_covers_decode(u64::MAX);
    }

    fn corpus_covers_decode(max_read: u64) {
        // GIMP XCF is excluded from the fast gate and ONLY from the fast gate. `extract_cover`
        // carries no target edge, so it reaches the XCF decoder's FULL-RESOLUTION path by
        // design — the one Convert/Resize want. That path costs 18 s, 45 s, 68 s and 80 s on
        // the four layer fixtures in a debug build (211 s of the 219 s this sweep first took,
        // fifty times the next slowest sample), because they are deliberately pathological:
        // a 12000x12000 canvas, a 15-layer 6000x4000 stack. Real thumbnails do NOT take this
        // route any more; `decode_image_with_raw_order` hands the decoder its target and gets
        // the same picture 17x faster. And these four already carry a STRICTER assertion than
        // this one: `_expected-colors.txt` pins the exact colour each must flatten to, which
        // is how the 2.0.0 wrong-layer bug was caught. The `--ignored` sweep still runs them.
        let slow_by_design = max_read != u64::MAX;

        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus");
        let Ok(entries) = std::fs::read_dir(&corpus) else {
            return;
        };
        let (mut checked, mut covers) = (0usize, 0usize);
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('_') || !path.is_file() {
                continue;
            }
            if entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX) > max_read {
                continue;
            }
            if slow_by_design && name.to_ascii_lowercase().ends_with(".xcf") {
                continue;
            }
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            checked += 1;
            let started = std::time::Instant::now();
            let cover = extract_cover(&bytes);
            let elapsed = started.elapsed();
            // Kept, not scaffolding: this print is how the XCF cost above was found at all.
            // Run with `-- --nocapture` if this sweep ever starts dragging again.
            if elapsed.as_millis() > 1000 {
                eprintln!("  SLOW {name}: {} ms in extract_cover", elapsed.as_millis());
            }
            let Some(cover) = cover else {
                continue; // "this container has no embedded cover" is a fine answer
            };
            covers += 1;
            let (w, h) = match cover {
                CoverOut::Image(img) => (img.width(), img.height()),
                CoverOut::Bytes(raw) => {
                    // A Windows metafile is an accepted cover (Visio ships one) and the
                    // `image` crate cannot read it — that tier is WIC/magick. Assert what IS
                    // checkable here: that it is the format it claims to be.
                    if crate::decode::looks_like_metafile(&raw) {
                        continue;
                    }
                    let img = image::load_from_memory(&raw).unwrap_or_else(|e| {
                        panic!("{name}: extract_cover handed back bytes that do not decode: {e}")
                    });
                    (img.width(), img.height())
                }
            };
            assert!(
                w > 1 && h > 1,
                "{name}: cover decoded to {w}x{h} — that is not a picture"
            );
        }
        assert!(
            checked == 0 || covers > 0,
            "read {checked} corpus samples and not one produced a cover — the dispatch is broken"
        );
    }

    /// `real_or_decoded_dims` is the shared chain that replaced three hand-copied versions of
    /// "try `real_dims`, else fall back to a full decode" (`strip::read_info_impl`,
    /// `strip::read_info_verbose`, `verbs::fileops::dims`). A plain PNG carries no
    /// `real_dims`-recognised header (that probe only knows PSD/JP2), so getting dimensions
    /// back at all proves the fallback tier actually ran rather than the chain stopping at
    /// `real_dims`'s `None`.
    #[test]
    fn real_or_decoded_dims_falls_back_to_a_full_decode_when_the_header_probe_misses() {
        let mut png_bytes = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(5, 3, image::Rgb([1, 2, 3])))
            .write_to(
                &mut std::io::Cursor::new(&mut png_bytes),
                image::ImageFormat::Png,
            )
            .unwrap();
        assert_eq!(
            real_dims(&png_bytes),
            None,
            "PNG must not be real_dims-recognised, or this test proves nothing about the fallback"
        );
        assert_eq!(real_or_decoded_dims(&png_bytes), Some((5, 3)));
    }

    #[test]
    fn real_or_decoded_dims_declines_when_neither_tier_can_read_the_bytes() {
        assert_eq!(real_or_decoded_dims(b"not an image at all"), None);
    }

    /// The oversized-.clip STREAMING path: the preview comes off a real seekable
    /// File via the tail-database seek — the walk hops the (stand-in) layer
    /// chunk instead of buffering it, exactly what rescues a canvas past the
    /// provider's MaxSize cap.
    #[test]
    fn archive_cover_seek_streams_a_clip_tail_db() {
        use std::io::{Read, Seek};
        let png = [0x89, b'P', b'N', b'G', 9, 9, 9, 9];
        let clip = clip_testutil::synthetic_clip(&png, 2 * 1024 * 1024, false);
        let path = std::env::temp_dir().join(format!("st2k_stream_{}.clip", std::process::id()));
        std::fs::write(&path, &clip).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mut head = [0u8; 8];
        file.read_exact(&mut head).unwrap();
        file.rewind().unwrap();
        let cover = archive_cover_seek(file, &head, &default_prefs());
        let _ = std::fs::remove_file(&path);
        assert_eq!(cover.as_deref(), Some(&png[..]));
    }

    /// The oversized-archive STREAMING path: extract a cover from a real seekable
    /// File handle (not an in-memory `&[u8]`), proving a multi-hundred-MB CBZ can be
    /// thumbnailed off the IStream without buffering the whole archive (#90). The
    /// `zip` crate seeks to the central directory + reads only the chosen entry.
    #[test]
    fn archive_cover_seek_streams_from_a_real_file() {
        use std::io::{Read, Seek, Write};
        let path = std::env::temp_dir().join(format!("st2k_stream_{}.cbz", std::process::id()));
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("readme.txt", opts).unwrap(); // non-image: not a cover candidate
            zw.write_all(b"not an image").unwrap();
            zw.start_file("page1.jpg", opts).unwrap(); // the cover page
            zw.write_all(b"\xFF\xD8\xFFcover-bytes").unwrap();
            zw.finish().unwrap();
        }
        let mut file = std::fs::File::open(&path).unwrap();
        let mut head = [0u8; 8];
        file.read_exact(&mut head).unwrap();
        file.rewind().unwrap();
        let cover = archive_cover_seek(file, &head, &default_prefs());
        let _ = std::fs::remove_file(&path);
        assert_eq!(cover.as_deref(), Some(&b"\xFF\xD8\xFFcover-bytes"[..]));
    }

    /// The oversized-file STREAMED zip path must run the same dedicated project-
    /// preview dispatch as the in-memory path: an OpenRaster archive's real
    /// composite lives at `Thumbnails/thumbnail.png`, while its per-layer rasters
    /// (`data/layer*.png`) natural-sort FIRST — the generic image-pick would
    /// return a wrong (possibly blank) layer instead of the artwork.
    #[test]
    fn streamed_zip_path_prefers_project_preview_over_layers() {
        use std::io::{Read, Seek, Write};
        let png = |color: u8| {
            let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([color, 0, 0, 255]));
            let mut out = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .unwrap();
            out
        };
        let (layer, thumb) = (png(10), png(200));
        let path = std::env::temp_dir().join(format!("st2k_ora_{}.ora", std::process::id()));
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("mimetype", opts).unwrap();
            zw.write_all(b"image/openraster").unwrap();
            zw.start_file("data/layer0.png", opts).unwrap(); // sorts before Thumbnails/
            zw.write_all(&layer).unwrap();
            zw.start_file("Thumbnails/thumbnail.png", opts).unwrap(); // the real preview
            zw.write_all(&thumb).unwrap();
            zw.finish().unwrap();
        }
        let mut file = std::fs::File::open(&path).unwrap();
        let mut head = [0u8; 8];
        file.read_exact(&mut head).unwrap();
        file.rewind().unwrap();
        let cover = archive_cover_seek(file, &head, &default_prefs());
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            cover.as_deref(),
            Some(&thumb[..]),
            "streamed ORA must return the composite preview, not a layer"
        );
    }

    /// An EPUB's declared OPF cover must win on BOTH the in-memory and the STREAMED
    /// path. The seekable path used to lack the EPUB arm entirely, so a book big
    /// enough to stream fell through to the generic natural-first image pick and
    /// returned an arbitrary interior illustration instead of the real cover — a
    /// large EPUB got a worse thumbnail than a small one. The archive here is built
    /// so the two answers are visibly different: `Images/aaa-illustration.png`
    /// natural-sorts FIRST, while the OPF declares `Images/zzz-frontispiece.png`.
    /// NEITHER name contains "cover" on purpose — `select::pick_covers` promotes
    /// any "cover"-named file, which would let the generic pick land on the right
    /// image by accident and make this test pass without the EPUB arm.
    #[test]
    fn epub_cover_cascade_runs_on_the_streamed_path_too() {
        use std::io::{Read, Seek, Write};
        let png = |color: u8| {
            let img = image::RgbaImage::from_pixel(4, 4, image::Rgba([color, 0, 0, 255]));
            let mut out = Vec::new();
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
                .unwrap();
            out
        };
        let (illustration, real_cover) = (png(10), png(200));
        let opf = r#"<?xml version="1.0"?><package><metadata>
            <meta name="cover" content="cover-img"/></metadata><manifest>
            <item id="cover-img" href="Images/zzz-frontispiece.png" media-type="image/png"/>
            </manifest></package>"#;
        let container = r#"<?xml version="1.0"?><container><rootfiles>
            <rootfile full-path="OEBPS/content.opf"/></rootfiles></container>"#;

        let path = std::env::temp_dir().join(format!("st2k_epub_{}.epub", std::process::id()));
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut zw = zip::ZipWriter::new(f);
            let opts = zip::write::SimpleFileOptions::default();
            zw.start_file("mimetype", opts).unwrap();
            zw.write_all(b"application/epub+zip").unwrap();
            zw.start_file("META-INF/container.xml", opts).unwrap();
            zw.write_all(container.as_bytes()).unwrap();
            zw.start_file("OEBPS/content.opf", opts).unwrap();
            zw.write_all(opf.as_bytes()).unwrap();
            // Natural-sorts BEFORE the cover: what the generic pick would grab.
            zw.start_file("OEBPS/Images/aaa-illustration.png", opts)
                .unwrap();
            zw.write_all(&illustration).unwrap();
            zw.start_file("OEBPS/Images/zzz-frontispiece.png", opts)
                .unwrap();
            zw.write_all(&real_cover).unwrap();
            zw.finish().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mut head = [0u8; 8];
        file.read_exact(&mut head).unwrap();
        file.rewind().unwrap();
        let streamed = archive_cover_seek(file, &head, &default_prefs());
        let in_memory = zipfmt::extract(&bytes);
        let _ = std::fs::remove_file(&path);

        assert_eq!(
            in_memory.as_deref(),
            Some(&real_cover[..]),
            "in-memory EPUB must resolve the OPF-declared cover"
        );
        assert_eq!(
            streamed.as_deref(),
            Some(&real_cover[..]),
            "streamed EPUB must resolve the SAME cover, not the natural-first image"
        );
    }

    use super::blend::testutil::synthetic_blend;

    #[test]
    fn compressed_blend_covers_extract() {
        use std::io::Write;
        let blend = synthetic_blend(&[]);

        // gzip (the "Compress" save option pre-3.0).
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&blend).unwrap();
        let gz = gz.finish().unwrap();
        match extract_cover(&gz) {
            Some(CoverOut::Image(img)) => assert_eq!((img.width(), img.height()), (4, 3)),
            other => panic!(
                "gzip blend must extract a cover (got some: {})",
                other.is_some()
            ),
        }

        // zstd (the "Compress" save option, Blender 3.0+). ruzstd is decode-only,
        // so hand-build a single raw-block frame: magic, FHD=0 (window descriptor
        // follows), window 1 KiB, then one last raw block of the payload.
        let mut z = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x00];
        let bh = ((blend.len() as u32) << 3) | 0x01; // last_block=1, type=raw
        z.extend_from_slice(&bh.to_le_bytes()[..3]);
        z.extend_from_slice(&blend);
        match extract_cover(&z) {
            Some(CoverOut::Image(img)) => assert_eq!((img.width(), img.height()), (4, 3)),
            other => panic!(
                "zstd blend must extract a cover (got some: {})",
                other.is_some()
            ),
        }

        // gzip of a NON-blend payload is not ours — svgz/emz stay with the decode tiers.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"<svg xmlns='http://www.w3.org/2000/svg'/>")
            .unwrap();
        assert!(extract_cover(&gz.finish().unwrap()).is_none());
    }

    #[test]
    fn compressed_blend_tolerates_truncation() {
        use std::io::Write;
        // A big compressed scene arrives as a bounded HEAD PREFIX on the oversized-
        // file path — i.e. a gzip stream cut mid-way. The TEST block decompresses
        // long before the cut, so extraction must still succeed. Incompressible
        // (PRNG) tail so the cut point lands deep inside the tail, deterministically.
        let mut tail = vec![0u8; 256 * 1024];
        let mut s: u32 = 0x1234_5678;
        for b in &mut tail {
            s ^= s << 13;
            s ^= s >> 17;
            s ^= s << 5;
            *b = s as u8;
        }
        let blend = synthetic_blend(&tail);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&blend).unwrap();
        let gz = gz.finish().unwrap();
        let cut = &gz[..gz.len() / 2];
        match extract_cover(cut) {
            Some(CoverOut::Image(img)) => assert_eq!((img.width(), img.height()), (4, 3)),
            _ => panic!("truncated gzip blend must still extract the head thumbnail"),
        }
    }

    /// Every cover extension must be a real `FORMATS` entry (so we never pick an
    /// archive member we can't actually decode) — except the documented WIC-only
    /// exceptions. Catches drift when `FORMATS` gains/loses a format but this
    /// hand-maintained cover set doesn't follow (the live `jxr` divergence the
    /// 2026-06 audit found).
    #[test]
    fn cover_exts_are_known_formats() {
        for &ext in COVER_IMAGE_EXTS {
            assert!(
                crate::formats::is_known(ext) || COVER_ONLY_EXCEPTIONS.contains(&ext),
                "is_image_name accepts `{ext}`, which is neither in FORMATS nor a documented \
                 cover-only exception — add it to FORMATS or to COVER_ONLY_EXCEPTIONS",
            );
        }
    }

    /// Each exception must genuinely be (a) absent from FORMATS and (b) still in
    /// the cover set — otherwise it is stale and should be removed, keeping the
    /// exception list honest.
    #[test]
    fn cover_exceptions_are_not_stale() {
        for &ext in COVER_ONLY_EXCEPTIONS {
            assert!(
                !crate::formats::is_known(ext),
                "`{ext}` is now in FORMATS — remove it from COVER_ONLY_EXCEPTIONS",
            );
            assert!(
                COVER_IMAGE_EXTS.contains(&ext),
                "`{ext}` is no longer a cover extension — remove it from COVER_ONLY_EXCEPTIONS",
            );
        }
    }

    /// Seeds for `fuzz_extract_cover`: every corpus sample (size-capped) plus a few
    /// degenerate buffers.
    fn fuzz_seed_corpus() -> Vec<Vec<u8>> {
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("test-corpus");
        let mut seeds: Vec<Vec<u8>> = vec![Vec::new(), vec![0u8; 64], vec![0xFFu8; 64]];
        if let Ok(rd) = std::fs::read_dir(&corpus) {
            for entry in rd.flatten() {
                if let Ok(b) = std::fs::read(entry.path()) {
                    if !b.is_empty() && b.len() <= 1_000_000 {
                        seeds.push(b);
                    }
                }
            }
        }
        seeds
    }

    /// Apply `nmut` random byte-level mutations (flip/set/truncate/insert/extend/increment)
    /// to `data` in place, using `rng` for every random choice.
    fn mutate_fuzz_input(data: &mut Vec<u8>, nmut: u64, rng: &mut impl FnMut() -> u64) {
        for _ in 0..nmut {
            if data.is_empty() {
                data.push((rng() & 0xff) as u8);
                continue;
            }
            match rng() % 6 {
                0 => {
                    let p = (rng() as usize) % data.len();
                    data[p] ^= 1u8 << (rng() % 8);
                }
                1 => {
                    let p = (rng() as usize) % data.len();
                    data[p] = (rng() & 0xff) as u8;
                }
                2 => {
                    let p = (rng() as usize) % data.len();
                    data.truncate(p);
                }
                3 => {
                    let p = (rng() as usize) % (data.len() + 1);
                    data.insert(p, (rng() & 0xff) as u8);
                }
                4 => {
                    for _ in 0..(rng() % 64) {
                        data.push((rng() & 0xff) as u8);
                    }
                }
                _ => {
                    let p = (rng() as usize) % data.len();
                    data[p] = data[p].wrapping_add(1);
                }
            }
        }
    }

    /// Save each crashing input to TEMP and panic naming how many were found, or print the
    /// clean-run summary when `crashes` is empty.
    fn report_fuzz_crashes(iters: u64, crashes: &[(u64, Vec<u8>)]) {
        if crashes.is_empty() {
            eprintln!("fuzz_extract_cover: {iters} iterations, 0 panics");
            return;
        }
        for (i, data) in crashes {
            let p = std::env::temp_dir().join(format!("st2k_fuzz_crash_{i}.bin"));
            let _ = std::fs::write(&p, data);
            eprintln!("PANIC iter {i}: {} bytes -> {}", data.len(), p.display());
        }
        panic!(
            "fuzz_extract_cover found {} panicking input(s)",
            crashes.len()
        );
    }

    /// On-demand FUZZER for the container cover extractors — our untrusted-input surface
    /// (a hostile file lands here inside Explorer's thumbnail host under `panic = "abort"`).
    /// Seeds from the real test corpus, applies random mutations (bit/byte flips, truncate,
    /// insert, extend) plus degenerate buffers, and asserts `extract_cover` never PANICS
    /// (an abort would take down Explorer). Deterministic PRNG → any crash is reproducible;
    /// failing inputs are saved to TEMP. Run on demand (DEV profile, so the catch_unwind
    /// below actually catches — the release profile is panic=abort):
    ///   cargo test --lib fuzz_extract_cover -- --ignored --nocapture
    #[test]
    #[ignore = "fuzzer — run on demand with --ignored"]
    fn fuzz_extract_cover() {
        use std::panic::{catch_unwind, AssertUnwindSafe};

        let seeds = fuzz_seed_corpus();
        eprintln!("fuzz_extract_cover: {} seeds", seeds.len());

        // Deterministic xorshift64 PRNG (reproducible; no rand dep).
        let mut s: u64 = 0x9E37_79B9_7F4A_7C15;
        let mut rng = move || {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            s
        };

        // Quiet the panic hook during the run so a caught panic doesn't flood stderr.
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(|_| {}));

        const ITERS: u64 = 30_000;
        let mut crashes: Vec<(u64, Vec<u8>)> = Vec::new();
        for i in 0..ITERS {
            let mut data = seeds[(rng() as usize) % seeds.len()].clone();
            let nmut = 1 + rng() % 10;
            mutate_fuzz_input(&mut data, nmut, &mut rng);
            let bytes = data.clone();
            if catch_unwind(AssertUnwindSafe(|| {
                let _ = extract_cover(&bytes);
            }))
            .is_err()
            {
                crashes.push((i, data));
                if crashes.len() >= 20 {
                    break;
                }
            }
        }

        std::panic::set_hook(prev);
        report_fuzz_crashes(ITERS, &crashes);
    }
}
