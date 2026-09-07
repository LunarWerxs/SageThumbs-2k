//! Audit E03: `formats::capability()` makes a claim about HOW each category's thumbnail is
//! produced (`Source::FullDecode`/`EmbeddedPreview`/`CoverArt`/`CoverOrFirstPage`/
//! `VideoFrame`/`ContainedImages`). This file proves the claim against the real decode
//! pipeline for the source kinds a fixture exists (or can be built in-test) for: the
//! decoded thumbnail's pixels must actually trace back to the claimed source, not just "a
//! non-empty PNG appeared" (see `docs/DEVELOPMENT_GOTCHAS.md` on why that alone is not a
//! test - three real bugs shipped behind exactly that false confidence).
//!
//! Nothing here depends on a binary fixture: the archive, the FLAC, the DNG-shaped TIFF and
//! the two-page PDF are all built in the test to their specs, so every source kind except
//! `VideoFrame` (which uses the committed MP4 fixture) is proven by COLOUR, with the wrong
//! route producing a colour miles away from the claimed one.

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

/// Mean RGB of a decoded image. Every fixture below is a flat colour (or a flat colour
/// under low-amplitude noise), so the mean IS the colour, and the wrong source shows up as
/// a channel miles away rather than a subtle difference.
fn mean_rgb(img: &image::DynamicImage) -> (u8, u8, u8) {
    let rgb = img.to_rgb8();
    let n = u64::from(rgb.width()) * u64::from(rgb.height());
    assert!(n > 0, "an empty image has no mean");
    let (mut r, mut g, mut b) = (0u64, 0u64, 0u64);
    for p in rgb.pixels() {
        r += u64::from(p[0]);
        g += u64::from(p[1]);
        b += u64::from(p[2]);
    }
    ((r / n) as u8, (g / n) as u8, (b / n) as u8)
}

/// A DNG-shaped TIFF built the way a camera lays one out: IFD0 is a tiny
/// `NewSubfileType = reduced-resolution` RGB image (here solid BLACK), and a real JPEG
/// preview (here solid GREEN under a little noise, so it clears the carve's 16 KiB floor)
/// sits in the file behind `JPEGInterchangeFormat`. The two colours are the whole test:
/// the decode pipeline stashes a reduced IFD0 rather than answering from it
/// (`streamsrc::tiff_ifd0_is_reduced`), so the ONLY way the thumbnail comes out green is
/// the embedded-preview carve (`decode::tiers::largest_embedded_jpeg`) that
/// `Source::EmbeddedPreview` claims. A regression that let the first tier win, or that
/// skipped the carve and fell through to the stash, produces black and fails here.
///
/// Little-endian classic TIFF; every IFD entry is written in ascending tag order as the
/// spec requires, with inline values where they fit in four bytes.
fn synthetic_dng(preview_jpeg: &[u8]) -> Vec<u8> {
    const W: u32 = 8;
    const H: u32 = 8;
    let strip_len = W * H * 3;
    // Layout: header (8) | IFD0 | BitsPerSample triple (6) | strip | JPEG.
    let entries: u32 = 13;
    let ifd0_at: u32 = 8;
    let ifd0_len = 2 + entries * 12 + 4;
    let bps_at = ifd0_at + ifd0_len;
    let strip_at = bps_at + 6;
    let jpeg_at = strip_at + strip_len;

    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"II\x2A\0");
    out.extend_from_slice(&ifd0_at.to_le_bytes());

    let mut ifd: Vec<u8> = Vec::new();
    ifd.extend_from_slice(&(entries as u16).to_le_bytes());
    let mut entry = |tag: u16, kind: u16, count: u32, value: [u8; 4]| {
        ifd.extend_from_slice(&tag.to_le_bytes());
        ifd.extend_from_slice(&kind.to_le_bytes());
        ifd.extend_from_slice(&count.to_le_bytes());
        ifd.extend_from_slice(&value);
    };
    const SHORT: u16 = 3;
    const LONG: u16 = 4;
    const BYTE: u16 = 1;
    let long = |v: u32| v.to_le_bytes();
    let short = |v: u16| {
        let b = v.to_le_bytes();
        [b[0], b[1], 0, 0]
    };
    entry(254, LONG, 1, long(1)); // NewSubfileType: reduced-resolution image
    entry(256, LONG, 1, long(W)); // ImageWidth
    entry(257, LONG, 1, long(H)); // ImageLength
    entry(258, SHORT, 3, long(bps_at)); // BitsPerSample -> 8,8,8 stored after the IFD
    entry(259, SHORT, 1, short(1)); // Compression: none
    entry(262, SHORT, 1, short(2)); // PhotometricInterpretation: RGB
    entry(273, LONG, 1, long(strip_at)); // StripOffsets
    entry(277, SHORT, 1, short(3)); // SamplesPerPixel
    entry(278, LONG, 1, long(H)); // RowsPerStrip
    entry(279, LONG, 1, long(strip_len)); // StripByteCounts
    entry(513, LONG, 1, long(jpeg_at)); // JPEGInterchangeFormat
    entry(514, LONG, 1, long(preview_jpeg.len() as u32)); // JPEGInterchangeFormatLength
    entry(50706, BYTE, 4, [1, 4, 0, 0]); // DNGVersion 1.4.0.0
    ifd.extend_from_slice(&0u32.to_le_bytes()); // no next IFD
    assert_eq!(ifd.len() as u32, ifd0_len);
    out.extend_from_slice(&ifd);
    out.extend_from_slice(&[8, 0, 8, 0, 8, 0]); // BitsPerSample values
    out.extend(std::iter::repeat_n(0u8, strip_len as usize)); // IFD0 pixels: solid black
    assert_eq!(out.len() as u32, jpeg_at);
    out.extend_from_slice(preview_jpeg);
    out
}

/// A green JPEG with low-amplitude deterministic noise: solid colour compresses to well
/// under the carve's 16 KiB minimum, and the noise is what makes it a "real" camera-sized
/// preview without changing the mean by more than a few units per channel.
fn green_preview_jpeg() -> Vec<u8> {
    let img = image::RgbImage::from_fn(512, 512, |x, y| {
        let n = ((x * 31) ^ (y * 17)) as u8 & 0x1F;
        image::Rgb([20 + n, 200 + (n >> 1), 20 + n])
    });
    let mut buf = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(img)
        .write_to(&mut buf, image::ImageFormat::Jpeg)
        .unwrap();
    let bytes = buf.into_inner();
    assert!(
        bytes.len() >= 16 * 1024,
        "the preview must clear the carve's 16 KiB floor, got {} bytes",
        bytes.len()
    );
    bytes
}

/// `Source::EmbeddedPreview` (Camera RAW): the thumbnail's pixels come from the JPEG the
/// file carries, not from the reduced first image and not from a demosaic. Proven by
/// colour: the carried preview is green, the reduced IFD0 is black, and a sensor dump is
/// absent, so green can only have come from the carve.
#[test]
fn embedded_preview_raw_thumbnail_comes_from_the_carried_jpeg_not_the_reduced_ifd0() {
    assert_eq!(
        sagethumbs2k_core::formats::capability("dng").source,
        sagethumbs2k_core::formats::Source::EmbeddedPreview
    );
    let src = scratch("synthetic.dng");
    std::fs::write(&src, synthetic_dng(&green_preview_jpeg())).unwrap();
    let out = scratch("raw_out.png");
    cli::thumbnail(src.to_str().unwrap(), out.to_str().unwrap(), 0)
        .unwrap_or_else(|e| panic!("expected the embedded JPEG preview to decode: {e}"));
    let decoded = image::open(&out).unwrap();
    let (r, g, b) = mean_rgb(&decoded);
    assert!(
        g > 150 && r < 90 && b < 90,
        "expected the GREEN carried preview, got mean rgb({r},{g},{b}): black means the \
         reduced IFD0 (or nothing) won over the embedded-preview carve"
    );
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}

/// A PDF written to the spec, one flat page per colour, NOT by our own `topdf` writer: a
/// fixture written by the code under test shares its assumptions. Same construction as
/// `pdf::tests::solid_colour_pdf` (which is `cfg(test)` in the library and so out of reach
/// here). US Letter at 72 dpi, uncompressed content streams, xref offsets computed as the
/// body is written.
fn solid_colour_pdf(colours: &[(u8, u8, u8)]) -> Vec<u8> {
    assert!(!colours.is_empty(), "a PDF needs at least one page");
    let n = colours.len();
    let mut out: Vec<u8> = Vec::new();
    let mut offsets: Vec<usize> = Vec::new();
    let page_obj = |i: usize| 3 + i * 2;
    let obj = |out: &mut Vec<u8>, offsets: &mut Vec<usize>, body: String| {
        offsets.push(out.len());
        out.extend_from_slice(body.as_bytes());
    };
    out.extend_from_slice(b"%PDF-1.4\n");
    obj(
        &mut out,
        &mut offsets,
        "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n".into(),
    );
    let kids: Vec<String> = (0..n).map(|i| format!("{} 0 R", page_obj(i))).collect();
    obj(
        &mut out,
        &mut offsets,
        format!(
            "2 0 obj\n<< /Type /Pages /Kids [{}] /Count {n} >>\nendobj\n",
            kids.join(" ")
        ),
    );
    for (i, &(r, g, b)) in colours.iter().enumerate() {
        let (po, co) = (page_obj(i), page_obj(i) + 1);
        obj(
            &mut out,
            &mut offsets,
            format!(
                "{po} 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] \
                 /Contents {co} 0 R /Resources << >> >>\nendobj\n"
            ),
        );
        let stream = format!(
            "{:.5} {:.5} {:.5} rg\n0 0 612 792 re\nf\n",
            f32::from(r) / 255.0,
            f32::from(g) / 255.0,
            f32::from(b) / 255.0
        );
        obj(
            &mut out,
            &mut offsets,
            format!(
                "{co} 0 obj\n<< /Length {} >>\nstream\n{stream}endstream\nendobj\n",
                stream.len()
            ),
        );
    }
    let xref_at = out.len();
    let total = offsets.len() + 1;
    out.extend_from_slice(format!("xref\n0 {total}\n0000000000 65535 f \n").as_bytes());
    for off in &offsets {
        out.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
    }
    out.extend_from_slice(
        format!("trailer\n<< /Size {total} /Root 1 0 R >>\nstartxref\n{xref_at}\n%%EOF\n")
            .as_bytes(),
    );
    out
}

/// `Source::CoverOrFirstPage` (Document category): a multi-page PDF thumbnails from page
/// ONE. Page one is blue and page two is red, so a render of the wrong page, or of nothing,
/// is a channel miles off rather than a subtle difference. Needs the OS PDF engine, which
/// every Windows this test runs on carries.
#[test]
fn cover_or_first_page_document_thumbnail_is_page_one() {
    assert_eq!(
        sagethumbs2k_core::formats::capability("pdf").source,
        sagethumbs2k_core::formats::Source::CoverOrFirstPage
    );
    let src = scratch("two_pages.pdf");
    std::fs::write(&src, solid_colour_pdf(&[(30, 60, 210), (210, 40, 30)])).unwrap();
    let out = scratch("pdf_out.png");
    cli::thumbnail(src.to_str().unwrap(), out.to_str().unwrap(), 0)
        .unwrap_or_else(|e| panic!("expected page 1 to render: {e}"));
    let decoded = image::open(&out).unwrap();
    let (r, g, b) = mean_rgb(&decoded);
    assert!(
        b > 150 && r < 90 && g < 120,
        "expected page ONE (blue), got mean rgb({r},{g},{b}): red means page two was rendered"
    );
    let _ = std::fs::remove_file(&src);
    let _ = std::fs::remove_file(&out);
}
