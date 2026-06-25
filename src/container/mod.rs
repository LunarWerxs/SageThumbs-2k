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
// Cinema 4D (.c4d) — carve the embedded document/scene preview JPEG.
mod c4d;
// CorelDRAW (.cdr/.cdt) / Corel Exchange (.cmx) — RIFF DISP preview DIB → BMP.
mod cdr;
mod clip;
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
    // Blender: the RGBA thumbnail baked into the TEST file-block.
    if bytes.starts_with(b"BLENDER") {
        return blend::extract(bytes).map(CoverOut::Image);
    }
    // Affinity (Photo/Designer/Publisher): an embedded PNG preview.
    if affinity::looks_like_affinity(bytes) {
        return affinity::extract(bytes).map(CoverOut::Bytes);
    }
    // Paint Shop Pro (.pspimage/.psp): carve the JPEG preview from the file's
    // Composite Image Bank (present even when the pixel data is RLE/uncompressed).
    if psp::looks_like_psp(bytes) {
        return psp::extract(bytes).map(CoverOut::Bytes);
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

/// Stream a cover from an OVERSIZED archive (past the in-memory size cap) using a
/// seekable reader — the shell's IStream — so a multi-hundred-MB CBZ/CB7 thumbnails
/// without ever buffering the whole file. ZIP-family and 7-Zip support seeking; RAR
/// can't (the `rars` crate needs the full buffer), so a giant CBR still falls through
/// to the default icon. `head` is the first bytes (already peeked) for the magic sniff.
pub fn archive_cover_seek<R: std::io::Read + std::io::Seek>(reader: R, head: &[u8]) -> Option<Vec<u8>> {
    // ZIP family: CBZ / ZIP (and any zip of images).
    if is_zip(head) {
        return zipfmt::cover_from_reader(reader);
    }
    // 7-Zip: CB7.
    if is_7z(head) {
        return sevenz::extract_seek(reader);
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

#[cfg(test)]
mod tests {
    use super::*;

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
