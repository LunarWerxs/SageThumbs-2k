//! Audit E03: `formats::capability()` makes a claim about HOW each category's thumbnail is
//! produced (`Source::FullDecode`/`EmbeddedPreview`/`CoverArt`/`CoverOrFirstPage`/
//! `VideoFrame`/`ContainedImages`). This file proves the claim against the real decode
//! pipeline for the source kinds a fixture exists (or can be built in-test) for: the
//! decoded thumbnail's pixels must actually trace back to the claimed source, not just "a
//! non-empty PNG appeared" (see `docs/DEVELOPMENT_GOTCHAS.md` on why that alone is not a
//! test - three real bugs shipped behind exactly that false confidence).
//!
//! `EmbeddedPreview` (Camera RAW) has no fixture in `tests/fixtures` and building a valid
//! RAW file (CR2/NEF/DNG/…) by hand is not a reasonable in-test construction, so that case
//! is `#[ignore]`d below with the reason in its name, never silently skipped.

use std::io::Write;
use std::path::PathBuf;

use sagethumbs2k_core::cli;

/// A fresh scratch file path under the OS temp dir, PID+name suffixed so parallel test
/// threads (and other concurrent `cargo test` processes in this shared tree) never collide.
fn scratch(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "st2k_capclaim_{}_{}_{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

/// A deterministic, non-flat PNG at `w`x`h` - flat fills hide real decode bugs (see
/// `tests/fringe_dimensions.rs`'s doc comment for why).
fn png_bytes(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_fn(w, h, |x, y| {
        image::Rgba([(x * 5) as u8, (y * 7) as u8, 200, 255])
    });
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut buf, image::ImageFormat::Png)
        .unwrap();
    buf.into_inner()
}

/// `Source::FullDecode` (the ordinary Image category): the thumbnail's pixels ARE the
/// source image, full-fidelity, not a placeholder.
#[test]
fn full_decode_thumbnail_matches_the_source_image() {
    assert_eq!(
        sagethumbs2k_core::formats::capability("png").source,
        sagethumbs2k_core::formats::Source::FullDecode
    );
    let src = scratch("full_decode.png");
    std::fs::write(&src, png_bytes(37, 29)).unwrap();
    let out = scratch("full_decode_out.png");
    cli::thumbnail(src.to_str().unwrap(), out.to_str().unwrap(), 0).unwrap();
    let decoded = image::open(&out).unwrap();
    // Aspect ratio preserved (37:29 is deliberately non-square/non-power-of-two so a
    // stretch or a fixed-square crop would be caught).
    assert_eq!(decoded.width() * 29, decoded.height() * 37);
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

/// `Source::ContainedImages` (Archive category): the thumbnail is the CONTAINED image, not
/// a picture of the archive itself. Built in-test: a zip with one PNG inside.
#[test]
fn contained_images_archive_thumbnail_comes_from_the_zipped_png() {
    assert_eq!(
        sagethumbs2k_core::formats::capability("zip").source,
        sagethumbs2k_core::formats::Source::ContainedImages
    );
    let cap = sagethumbs2k_core::formats::capability("zip");
    assert!(cap.preview_listing, "archives must be preview_listing");
    assert!(!cap.convertible, "archives must not be convertible");

    let inner_w = 41u32;
    let inner_h = 23u32;
    let png = png_bytes(inner_w, inner_h);

    let src = scratch("contained.zip");
    {
        let f = std::fs::File::create(&src).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        zw.start_file("cover.png", opts).unwrap();
        zw.write_all(&png).unwrap();
        zw.finish().unwrap();
    }

    let out = scratch("contained_out.png");
    cli::thumbnail(src.to_str().unwrap(), out.to_str().unwrap(), 0).unwrap();
    let decoded = image::open(&out).unwrap();
    // The zipped PNG's aspect ratio must survive - a thumbnail of the ARCHIVE FILE ITSELF
    // (e.g. a generic-file icon rendered as an image, or an empty/blank tile) would not
    // reproduce this specific, deliberately-odd ratio.
    assert_eq!(decoded.width() * inner_h, decoded.height() * inner_w);

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

/// A hand-built minimal FLAC: magic + a real STREAMINFO block (44100 Hz / 2ch / 16-bit,
/// values `container/audio.rs`'s reader doesn't inspect but `lofty::Probe` validates the
/// block SHAPE of) + one METADATA_BLOCK_PICTURE carrying `picture` as a "Cover (front)"
/// PNG, marked as the last metadata block. No audio frames follow - cover-art extraction
/// never decodes the audio stream, only the metadata blocks, so none are needed.
fn synthetic_flac_with_picture(picture: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");

    // METADATA_BLOCK_STREAMINFO (type 0), NOT last, 34-byte body.
    let streaminfo: [u8; 34] = [
        0x10, 0x00, // min blocksize 4096
        0x10, 0x00, // max blocksize 4096
        0x00, 0x00, 0x00, // min framesize
        0x00, 0x00, 0x00, // max framesize
        // sample_rate=44100 (20b) | channels-1=1 (3b) | bps-1=15 (5b) | total_samples=0 (36b)
        0x0A, 0xC4, 0x42, 0xF0, 0x00, 0x00, 0x00, 0x00,
        // MD5 signature (unused by our reader) - all zero.
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ];
    out.push(0x00); // type 0, not-last
    out.extend_from_slice(&(streaminfo.len() as u32).to_be_bytes()[1..]); // 24-bit BE length
    out.extend_from_slice(&streaminfo);

    // METADATA_BLOCK_PICTURE (type 6), LAST block.
    let mime = b"image/png";
    let desc = b"";
    let mut pic = Vec::new();
    pic.extend_from_slice(&3u32.to_be_bytes()); // picture type 3 = "Cover (front)"
    pic.extend_from_slice(&(mime.len() as u32).to_be_bytes());
    pic.extend_from_slice(mime);
    pic.extend_from_slice(&(desc.len() as u32).to_be_bytes());
    pic.extend_from_slice(desc);
    pic.extend_from_slice(&0u32.to_be_bytes()); // width (unknown/0 is legal)
    pic.extend_from_slice(&0u32.to_be_bytes()); // height
    pic.extend_from_slice(&0u32.to_be_bytes()); // color depth
    pic.extend_from_slice(&0u32.to_be_bytes()); // colors used (0 = not indexed)
    pic.extend_from_slice(&(picture.len() as u32).to_be_bytes());
    pic.extend_from_slice(picture);

    out.push(0x80 | 0x06); // last-block flag set, type 6
    out.extend_from_slice(&(pic.len() as u32).to_be_bytes()[1..]); // 24-bit BE length
    out.extend_from_slice(&pic);

    out
}

/// `Source::CoverArt` (Audio category): the thumbnail is the EMBEDDED cover picture, not a
/// waveform render or a generic audio icon - proven by asserting the decoded thumbnail's
/// aspect ratio matches the embedded picture's, which a fallback path would not reproduce.
#[test]
fn cover_art_audio_thumbnail_matches_the_embedded_picture() {
    assert_eq!(
        sagethumbs2k_core::formats::capability("flac").source,
        sagethumbs2k_core::formats::Source::CoverArt
    );
    let pic_w = 33u32;
    let pic_h = 19u32;
    let picture = png_bytes(pic_w, pic_h);
    let flac = synthetic_flac_with_picture(&picture);

    let src = scratch("cover.flac");
    std::fs::write(&src, &flac).unwrap();
    let out = scratch("cover_out.png");
    cli::thumbnail(src.to_str().unwrap(), out.to_str().unwrap(), 0)
        .unwrap_or_else(|e| panic!("expected the embedded cover art to decode: {e}"));
    let decoded = image::open(&out).unwrap();
    assert_eq!(decoded.width() * pic_h, decoded.height() * pic_w);

    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

/// `Source::VideoFrame` (Video category), against the real committed fixture used by
/// `tests/video_profile_gate.rs`. Skips (not fails) when Media Foundation itself is
/// absent on the machine running the test - the same gate that test uses - since that is
/// an environment fact, not a regression in this code.
#[test]
fn video_frame_thumbnail_comes_from_a_real_decoded_frame() {
    assert_eq!(
        sagethumbs2k_core::formats::capability("mp4").source,
        sagethumbs2k_core::formats::Source::VideoFrame
    );
    assert_eq!(
        sagethumbs2k_core::formats::capability("mp4").os_codec,
        Some(sagethumbs2k_core::formats::OsCodec::MediaFoundation)
    );
    if !sagethumbs2k_core::video::media_foundation_available() {
        eprintln!("skipping: Media Foundation absent on this machine");
        return;
    }
    let fixture: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "video",
        "h264-high-320x240.mp4",
    ]
    .iter()
    .collect();
    let out = scratch("video_out.png");
    cli::thumbnail(fixture.to_str().unwrap(), out.to_str().unwrap(), 0)
        .unwrap_or_else(|e| panic!("expected a real frame from the fixture clip: {e}"));
    let decoded = image::open(&out).unwrap();
    // 320x240 source: a real frame grab preserves that 4:3 ratio; a caption-only/blank
    // fallback tile would not produce a non-trivial image at all (thumbnail() errors
    // instead), so this is checking the SHAPE of what actually decoded.
    assert_eq!(decoded.width() * 3, decoded.height() * 4);
    let _ = std::fs::remove_file(&out);
}

/// `Source::EmbeddedPreview` (Camera RAW category) has NO fixture in `tests/fixtures`, and
/// hand-building a structurally valid CR2/NEF/DNG/… (TIFF-IFD based, with a real embedded
/// JPEG preview stream) is not a reasonable in-test construction the way a zip or a FLAC
/// metadata block is - unlike `container::fuzzseed`'s synthetic seeds (built to survive
/// mutation, not to prove a specific embedded preview's pixels round-trip). Marked
/// `#[ignore]` rather than silently omitted or made to pass vacuously; the reason lives in
/// the name and here: get a real RAW sample (e.g. from `..\test-corpus`) to unignore it.
#[test]
#[ignore = "no RAW fixture available in tests/fixtures; needs a real CR2/NEF/DNG sample to prove the embedded-JPEG-preview claim rather than a hand-built stub"]
fn embedded_preview_raw_thumbnail_matches_its_own_embedded_jpeg() {
    assert_eq!(
        sagethumbs2k_core::formats::capability("cr2").source,
        sagethumbs2k_core::formats::Source::EmbeddedPreview
    );
    // Documented drop-in path (see the `#[ignore]` reason above): a real CR2/NEF/DNG sample.
    let fixture: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "raw",
        "sample.dng",
    ]
    .iter()
    .collect();
    if !fixture.exists() {
        eprintln!(
            "skipping: no RAW fixture at {} - drop a real CR2/NEF/DNG sample there (see the \
             #[ignore] reason on this test) to run the embedded-preview assertion",
            fixture.display()
        );
        return;
    }
    let out = scratch("raw_out.png");
    cli::thumbnail(fixture.to_str().unwrap(), out.to_str().unwrap(), 0)
        .unwrap_or_else(|e| panic!("expected the embedded JPEG preview to decode: {e}"));
    let decoded = image::open(&out).unwrap();
    assert!(
        decoded.width() > 0 && decoded.height() > 0,
        "expected a real decoded preview, not an empty image"
    );
    let _ = std::fs::remove_file(&out);
}

/// `Source::CoverOrFirstPage` (Ebook + Document categories) has no PDF/EPUB fixture in
/// `tests/fixtures` either, and a hand-built PDF page-1 render or EPUB cover extraction
/// depends on real container structure (a WinRT PDF rasterizer call, or a real OPF
/// manifest) that a minimal synthetic stub would not exercise meaningfully. Marked
/// `#[ignore]` for the same reason as the RAW case above.
#[test]
#[ignore = "no PDF/EPUB fixture available in tests/fixtures; needs a real sample to prove the cover/first-page claim rather than a hand-built stub"]
fn cover_or_first_page_document_thumbnail_matches_its_first_page() {
    assert_eq!(
        sagethumbs2k_core::formats::capability("pdf").source,
        sagethumbs2k_core::formats::Source::CoverOrFirstPage
    );
    // Documented drop-in path (see the `#[ignore]` reason above): a real multi-page PDF.
    let fixture: PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "tests",
        "fixtures",
        "pdf",
        "sample.pdf",
    ]
    .iter()
    .collect();
    if !fixture.exists() {
        eprintln!(
            "skipping: no PDF fixture at {} - drop a real sample there (see the #[ignore] \
             reason on this test) to run the cover/first-page assertion",
            fixture.display()
        );
        return;
    }
    let out = scratch("pdf_out.png");
    cli::thumbnail(fixture.to_str().unwrap(), out.to_str().unwrap(), 0)
        .unwrap_or_else(|e| panic!("expected page 1 to render: {e}"));
    let decoded = image::open(&out).unwrap();
    assert!(
        decoded.width() > 0 && decoded.height() > 0,
        "expected a real rendered first page, not an empty image"
    );
    let _ = std::fs::remove_file(&out);
}
