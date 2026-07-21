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
mod indd;
mod epub;
mod gcode;
mod fb2;
// Amiga / Deluxe Paint IFF ILBM (.iff/.ilbm/.lbm) — a real planar-bitmap decoder.
mod ilbm;
mod max;
mod mobi;
mod office;
mod ole;
mod project;
mod psd;
mod psp;
mod rar;
mod rhino;
mod select;
mod sevenz;
mod skp;
mod tarfmt;
mod util;
// GIMP XCF (.xcf) — native decoder; ImageMagick can't read the modern v011 format.
mod xcf;
// Waveform thumbnails for raw-PCM audio (WAV/AIFF) with no embedded cover art.
mod waveform;
mod zipfmt;

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
fn is_7z(b: &[u8]) -> bool {
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
    Some(entries.into_iter().map(|e| (e.name, e.size, e.is_dir)).collect())
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
        Box::new(ruzstd::StreamingDecoder::new(bytes).ok()?)
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
pub fn extract_cover(bytes: &[u8]) -> Option<CoverOut> {
    // ZIP family: EPUB / CBZ / FBZ (and any zip of images).
    if is_zip(bytes) {
        return zipfmt::extract(bytes).map(CoverOut::Bytes);
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
    // DjVu (IFF85 magic "AT&TFORM").
    if bytes.starts_with(b"AT&TFORM") {
        return djvu::extract(bytes).map(CoverOut::Image);
    }
    // GIMP XCF: native flatten-to-thumbnail. Takes priority over the magick tier on
    // purpose — ImageMagick's coder fails on the modern "gimp xcf v011" (GIMP 2.10/3.0),
    // and ours needs no ImageMagick at all (works on the compact install).
    if xcf::looks_like_xcf(bytes) {
        return xcf::extract(bytes).map(CoverOut::Image);
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
    // Ghostscript). On None (WMF-only/bare PS), fall through to the magick tier.
    if bytes.starts_with(&[0xC5, 0xD0, 0xD3, 0xC6]) {
        if let Some(tiff) = eps::extract(bytes) {
            return Some(CoverOut::Bytes(tiff));
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
pub fn archive_cover_seek<R: std::io::Read + std::io::Seek>(reader: R, head: &[u8]) -> Option<Vec<u8>> {
    // ZIP family: CBZ / ZIP (and any zip of images).
    if is_zip(head) {
        return zipfmt::cover_from_reader(reader);
    }
    // 7-Zip: CB7.
    if is_7z(head) {
        return sevenz::extract_seek(reader);
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
pub fn archive_covers(bytes: &[u8], want: usize) -> Option<Vec<Vec<u8>>> {
    if is_zip(bytes) {
        return zipfmt::covers_from_reader(std::io::Cursor::new(bytes), want);
    }
    if is_7z(bytes) {
        return sevenz::extract_seek_n(std::io::Cursor::new(bytes), want);
    }
    if is_rar(bytes) {
        return rar::extract_n(bytes, want);
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
) -> Option<Vec<Vec<u8>>> {
    if is_zip(head) {
        return zipfmt::covers_from_reader(reader, want);
    }
    if is_7z(head) {
        return sevenz::extract_seek_n(reader, want);
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
    None
}

/// True when a PSD/PSB document is transparent (its merged composite has an alpha
/// channel). The baked-in preview (resource 1036) is a JPEG with no alpha, so the
/// thumbnail/preview path renders the real layer composite for these instead of a
/// flat white preview. See [`psd::has_alpha`].
pub fn psd_has_alpha(bytes: &[u8]) -> bool {
    psd::has_alpha(bytes)
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
    "bmp", "ico", "gif", "jpg", "jpe", "jfif", "jpeg", "png", "tif", "tiff", "svg", "webp",
    "jxr", "nrw", "nef", "dng", "cr2", "heif", "heic", "avif", "jxl",
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

/// Test-only re-exports so the `decode`/`streamsrc` head-preview fast-path tests
/// can build synthetic PSD/DWG files without reaching into the private modules.
#[cfg(test)]
pub(crate) use psd::testutil as psd_testutil;
#[cfg(test)]
pub(crate) use dwg::testutil as dwg_testutil;

/// Shared embedded-JPEG span scanner — see [`util::jpeg_span_len`]. Re-exported so
/// `decode` and the container extractors (PSP, C4D) don't each hand-roll their own.
pub(crate) use util::jpeg_span_len;

#[cfg(test)]
mod tests {
    use super::*;

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
        let cover = archive_cover_seek(file, &head);
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
        let cover = archive_cover_seek(file, &head);
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
        let cover = archive_cover_seek(file, &head);
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
            zw.start_file("OEBPS/Images/aaa-illustration.png", opts).unwrap();
            zw.write_all(&illustration).unwrap();
            zw.start_file("OEBPS/Images/zzz-frontispiece.png", opts).unwrap();
            zw.write_all(&real_cover).unwrap();
            zw.finish().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        let mut file = std::fs::File::open(&path).unwrap();
        let mut head = [0u8; 8];
        file.read_exact(&mut head).unwrap();
        file.rewind().unwrap();
        let streamed = archive_cover_seek(file, &head);
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

    /// Minimal valid legacy .blend (BHead4) with a 4×3 TEST thumbnail, plus an
    /// arbitrary tail after ENDB (stands in for the scene data of a big file).
    fn synthetic_blend(tail: &[u8]) -> Vec<u8> {
        let (w, h) = (4u32, 3u32);
        let px = vec![200u8; (w * h * 4) as usize];
        let mut b = Vec::new();
        b.extend_from_slice(b"BLENDER");
        b.push(b'_'); // 32-bit pointers
        b.push(b'v'); // little-endian
        b.extend_from_slice(b"277");
        b.extend_from_slice(b"TEST");
        b.extend_from_slice(&((8 + w * h * 4) as i32).to_le_bytes());
        b.extend_from_slice(&[0u8; 12]); // old(4) + sdna(4) + nr(4)
        b.extend_from_slice(&(w as i32).to_le_bytes());
        b.extend_from_slice(&(h as i32).to_le_bytes());
        b.extend_from_slice(&px);
        b.extend_from_slice(b"ENDB");
        b.extend_from_slice(&[0u8; 16]);
        b.extend_from_slice(tail);
        b
    }

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
            other => panic!("gzip blend must extract a cover (got some: {})", other.is_some()),
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
            other => panic!("zstd blend must extract a cover (got some: {})", other.is_some()),
        }

        // gzip of a NON-blend payload is not ours — svgz/emz stay with the decode tiers.
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(b"<svg xmlns='http://www.w3.org/2000/svg'/>").unwrap();
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

        // Seeds: every corpus sample (size-capped) + a few degenerate buffers.
        let corpus = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("test-corpus");
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

        if !crashes.is_empty() {
            for (i, data) in &crashes {
                let p = std::env::temp_dir().join(format!("st2k_fuzz_crash_{i}.bin"));
                let _ = std::fs::write(&p, data);
                eprintln!("PANIC iter {i}: {} bytes -> {}", data.len(), p.display());
            }
            panic!("fuzz_extract_cover found {} panicking input(s)", crashes.len());
        }
        eprintln!("fuzz_extract_cover: {ITERS} iterations, 0 panics");
    }
}
