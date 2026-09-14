//! Integration tests for encoder ingestion slice 1 (#694).

use assert_cmd::Command;
use djvu_rs::djvu_render::RenderOptions;
use djvu_rs::png_io::decode_png_to_pixmap;
use djvu_rs::quality;
use std::path::Path;

const ICCP_CHUNK: &[u8] = &[
    0x00, 0x00, 0x00, 0x3c, 0x69, 0x43, 0x43, 0x50, 0x69, 0x67, 0x6e, 0x6f, 0x72, 0x65, 0x64, 0x00,
    0x00, 0x78, 0x9c, 0x0d, 0xc8, 0xc1, 0x0d, 0x00, 0x20, 0x08, 0x04, 0xb0, 0x55, 0x6e, 0x35, 0x25,
    0x60, 0x88, 0x44, 0x08, 0xf2, 0x61, 0x7b, 0xed, 0xb3, 0x32, 0x36, 0x43, 0x89, 0x10, 0xe9, 0xa2,
    0xc6, 0x98, 0x5d, 0x7c, 0x21, 0x9e, 0xd0, 0xb3, 0xf8, 0x16, 0xc2, 0x4d, 0xa9, 0xf1, 0xb7, 0x1e,
    0x7d, 0x9a, 0x10, 0xe0, 0xc6, 0x08, 0xf6, 0x1c,
];

fn write_png(
    path: &Path,
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    pixels: &[u8],
) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(pixels).unwrap();
}

fn write_indexed_png(path: &Path, width: u32, height: u32, indices: &[u8]) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Indexed);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_palette(vec![255, 255, 255, 0, 0, 0, 20, 40, 180]);
    encoder.set_trns(vec![255, 128, 255]);
    let mut writer = encoder.write_header().unwrap();
    writer.write_image_data(indices).unwrap();
}

fn write_sixteen_bit_gray_checker(path: &Path, width: u32, height: u32) {
    let file = std::fs::File::create(path).unwrap();
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Grayscale);
    encoder.set_depth(png::BitDepth::Sixteen);
    let mut writer = encoder.write_header().unwrap();
    let mut pixels = Vec::with_capacity((width * height * 2) as usize);
    for y in 0..height {
        for x in 0..width {
            let v = if (x + y) % 2 == 0 {
                0xFFFFu16
            } else {
                0x0000u16
            };
            pixels.extend_from_slice(&[(v >> 8) as u8, v as u8]);
        }
    }
    writer.write_image_data(&pixels).unwrap();
}

fn inject_iccp_after_ihdr(png: &[u8]) -> Vec<u8> {
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    let mut pos = 8usize;
    while pos + 12 <= png.len() {
        let len = u32::from_be_bytes(png[pos..pos + 4].try_into().unwrap()) as usize;
        let typ = &png[pos + 4..pos + 8];
        let chunk_end = pos + 12 + len;
        if typ == b"IHDR" {
            let mut out = Vec::new();
            out.extend_from_slice(&png[..chunk_end]);
            out.extend_from_slice(ICCP_CHUNK);
            out.extend_from_slice(&png[chunk_end..]);
            return out;
        }
        pos = chunk_end;
    }
    panic!("IHDR chunk not found");
}

fn encode_lossless(input: &Path, output: &Path) {
    Command::cargo_bin("djvu")
        .unwrap()
        .args([
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--quality",
            "lossless",
            "--dpi",
            "100",
        ])
        .assert()
        .success();
}

fn render_first_page(path: &Path) -> djvu_rs::Pixmap {
    let bytes = std::fs::read(path).unwrap();
    let doc = djvu_rs::Document::from_bytes(bytes).unwrap();
    let page = doc.page(0).unwrap();
    let opts = RenderOptions {
        width: page.width(),
        height: page.height(),
        ..Default::default()
    };
    page.render_with(&opts).unwrap()
}

#[test]
fn icc_profile_does_not_change_decoded_pixels() {
    let dir = tempfile::tempdir().unwrap();
    let plain = dir.path().join("plain.png");
    write_png(
        &plain,
        2,
        1,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        &[255, 0, 0, 0, 255, 0],
    );
    let with_icc = dir.path().join("with_icc.png");
    let bytes = std::fs::read(&plain).unwrap();
    std::fs::write(&with_icc, inject_iccp_after_ihdr(&bytes)).unwrap();

    let plain_pm = decode_png_to_pixmap(&plain).unwrap();
    let icc_pm = decode_png_to_pixmap(&with_icc).unwrap();
    assert_eq!(plain_pm.data, icc_pm.data);
}

#[test]
fn encode_one_bit_png_creates_valid_djvu() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("1bit.png");
    write_png(
        &input,
        8,
        8,
        png::ColorType::Grayscale,
        png::BitDepth::One,
        &[0b10101010; 8],
    );
    let output = dir.path().join("out.djvu");
    encode_lossless(&input, &output);
    assert_eq!(&std::fs::read(&output).unwrap()[..4], b"AT&T");
}

#[test]
fn encode_two_bit_png_creates_valid_djvu() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("2bit.png");
    write_png(
        &input,
        4,
        1,
        png::ColorType::Grayscale,
        png::BitDepth::Two,
        &[0b11100100],
    );
    let output = dir.path().join("out.djvu");
    encode_lossless(&input, &output);
    assert_eq!(&std::fs::read(&output).unwrap()[..4], b"AT&T");
}

#[test]
fn encode_sixteen_bit_png_creates_valid_djvu() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("16bit.png");
    write_sixteen_bit_gray_checker(&input, 2, 1);
    let output = dir.path().join("out.djvu");
    encode_lossless(&input, &output);
    assert_eq!(&std::fs::read(&output).unwrap()[..4], b"AT&T");
}

#[test]
fn encode_indexed_png_renders_with_acceptable_quality() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("indexed.png");
    write_indexed_png(&input, 32, 32, &vec![0u8; 32 * 32]);
    let source = decode_png_to_pixmap(&input).unwrap();
    let output = dir.path().join("out.djvu");
    encode_lossless(&input, &output);
    let rendered = render_first_page(&output);
    let report = quality::compare(&source, &rendered);
    assert!(
        report.ssim >= 0.95,
        "indexed PNG round-trip SSIM too low: {}",
        report.ssim
    );
}

#[test]
fn encode_sixteen_bit_png_renders_with_acceptable_quality() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("16bit.png");
    write_sixteen_bit_gray_checker(&input, 32, 32);
    let source = decode_png_to_pixmap(&input).unwrap();
    let output = dir.path().join("out.djvu");
    encode_lossless(&input, &output);
    let rendered = render_first_page(&output);
    let report = quality::compare(&source, &rendered);
    assert!(
        report.ssim >= 0.95,
        "16-bit PNG round-trip SSIM too low: {}",
        report.ssim
    );
}

#[test]
fn alpha_channel_preserved_through_png_ingest() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("alpha.png");
    write_png(
        &input,
        1,
        1,
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        &[10, 20, 30, 128],
    );
    let pm = decode_png_to_pixmap(&input).unwrap();
    assert_eq!(pm.data, vec![10, 20, 30, 128]);
}

// ── TIFF ingestion slice 2 (#694) ────────────────────────────────────────────

/// Library-level TIFF normalization tests over hand-crafted fixtures. The
/// builder writes minimal classic little-endian TIFFs so it can produce
/// layouts the `tiff` crate encoder cannot (bilevel, palette, CMYK,
/// multipage, multi-strip).
#[cfg(feature = "tiff")]
mod tiff_slice2 {
    use djvu_rs::ingest::IngestPolicy;
    use djvu_rs::png_io::{
        decode_tiff_file_to_pixmap, decode_tiff_file_to_pixmap_with_policy,
        decode_tiff_file_to_pixmaps,
    };
    use std::path::Path;

    pub struct TiffPage {
        pub width: u32,
        pub height: u32,
        /// Per-sample bit depth (length = samples per pixel).
        pub bits: Vec<u16>,
        /// 0 WhiteIsZero, 1 BlackIsZero, 2 RGB, 3 Palette, 5 CMYK.
        pub photometric: u16,
        /// Packed strip data, one strip per `rows_per_strip` rows.
        pub data: Vec<u8>,
        pub rows_per_strip: Option<u32>,
        pub colormap: Option<Vec<u16>>,
        /// Compression tag value (1 = none). Data is written as-is.
        pub compression: u16,
        /// Explicit per-strip byte counts (tag 279) for compressed data.
        /// `None` assumes uncompressed strips of `rows * row_bytes` bytes.
        pub strip_byte_counts: Option<Vec<u32>>,
        /// Extra IFD entries appended verbatim: (tag, type, count, value).
        pub extra_tags: Vec<(u16, u16, u32, u32)>,
        /// XResolution (tag 282) as a RATIONAL numerator/denominator pair.
        pub x_resolution: Option<(u32, u32)>,
        /// YResolution (tag 283) as a RATIONAL numerator/denominator pair.
        pub y_resolution: Option<(u32, u32)>,
        /// ResolutionUnit (tag 296): 1 none, 2 inch, 3 centimeter.
        pub resolution_unit: Option<u16>,
    }

    impl TiffPage {
        pub fn gray(width: u32, height: u32, bits: u16, photometric: u16, data: Vec<u8>) -> Self {
            TiffPage {
                width,
                height,
                bits: vec![bits],
                photometric,
                data,
                rows_per_strip: None,
                colormap: None,
                compression: 1,
                strip_byte_counts: None,
                extra_tags: Vec::new(),
                x_resolution: None,
                y_resolution: None,
                resolution_unit: None,
            }
        }
    }

    /// Serialize pages as a classic little-endian TIFF.
    pub fn build_tiff(pages: &[TiffPage]) -> Vec<u8> {
        let mut out: Vec<u8> = Vec::new();
        out.extend_from_slice(b"II");
        out.extend_from_slice(&42u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // first-IFD offset, patched below

        // Strip data first. Multi-strip pages split rows evenly by
        // rows_per_strip; `data` holds the concatenated strips.
        let mut data_offsets = Vec::new();
        for p in pages {
            data_offsets.push(out.len() as u32);
            out.extend_from_slice(&p.data);
            if out.len() % 2 == 1 {
                out.push(0);
            }
        }

        let mut ifd_offsets = Vec::new();
        for (i, p) in pages.iter().enumerate() {
            let samples = p.bits.len() as u32;
            let rows_per_strip = p.rows_per_strip.unwrap_or(p.height).max(1);
            let strip_count = p.height.div_ceil(rows_per_strip).max(1) as usize;
            let row_bytes = (p.width as usize * p.bits[0] as usize).div_ceil(8);

            // Per-strip offsets/byte counts (out-of-line when > 1 strip).
            let strip_counts: Vec<u32> = p.strip_byte_counts.clone().unwrap_or_else(|| {
                (0..strip_count)
                    .map(|s| {
                        let rows = (p.height as usize)
                            .saturating_sub(s * rows_per_strip as usize)
                            .min(rows_per_strip as usize);
                        (rows * row_bytes) as u32
                    })
                    .collect()
            });
            assert_eq!(strip_counts.len(), strip_count, "strip_byte_counts length");
            let mut strip_offsets = Vec::with_capacity(strip_count);
            let mut cursor = data_offsets[i];
            for &len in &strip_counts {
                strip_offsets.push(cursor);
                cursor += len;
            }

            let write_u16s = |out: &mut Vec<u8>, vals: &[u16]| -> u32 {
                let off = out.len() as u32;
                for v in vals {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                off
            };
            let bits_offset = if p.bits.len() > 2 {
                write_u16s(&mut out, &p.bits)
            } else {
                0
            };
            let colormap_offset = p.colormap.as_ref().map(|cm| write_u16s(&mut out, cm));
            let write_u32s = |out: &mut Vec<u8>, vals: &[u32]| -> u32 {
                let off = out.len() as u32;
                for v in vals {
                    out.extend_from_slice(&v.to_le_bytes());
                }
                off
            };
            // RATIONAL values (8 bytes) always live out-of-line.
            let xres_offset = p.x_resolution.map(|(n, d)| write_u32s(&mut out, &[n, d]));
            let yres_offset = p.y_resolution.map(|(n, d)| write_u32s(&mut out, &[n, d]));
            let (offsets_value, counts_value) = if strip_count > 1 {
                (
                    write_u32s(&mut out, &strip_offsets),
                    write_u32s(&mut out, &strip_counts),
                )
            } else {
                (strip_offsets[0], strip_counts[0])
            };
            if out.len() % 2 == 1 {
                out.push(0);
            }

            ifd_offsets.push(out.len() as u32);
            // (tag, type, count, value) — type 3 = SHORT, 4 = LONG.
            let mut entries: Vec<(u16, u16, u32, u32)> = vec![
                (256, 4, 1, p.width),
                (257, 4, 1, p.height),
                (259, 3, 1, p.compression as u32),
                (262, 3, 1, p.photometric as u32),
                (273, 4, strip_count as u32, offsets_value),
                (277, 3, 1, samples),
                (278, 4, 1, rows_per_strip),
                (279, 4, strip_count as u32, counts_value),
            ];
            entries.push(match p.bits.len() {
                1 => (258, 3, 1, p.bits[0] as u32),
                2 => (258, 3, 2, (p.bits[0] as u32) | ((p.bits[1] as u32) << 16)),
                _ => (258, 3, samples, bits_offset),
            });
            if let (Some(cm), Some(off)) = (&p.colormap, colormap_offset) {
                entries.push((320, 3, cm.len() as u32, off));
            }
            // (tag, type 5 = RATIONAL, count, offset)
            if let Some(off) = xres_offset {
                entries.push((282, 5, 1, off));
            }
            if let Some(off) = yres_offset {
                entries.push((283, 5, 1, off));
            }
            if let Some(unit) = p.resolution_unit {
                entries.push((296, 3, 1, unit as u32));
            }
            entries.extend(p.extra_tags.iter().copied());
            entries.sort_by_key(|e| e.0);

            out.extend_from_slice(&(entries.len() as u16).to_le_bytes());
            for (tag, typ, count, value) in &entries {
                out.extend_from_slice(&tag.to_le_bytes());
                out.extend_from_slice(&typ.to_le_bytes());
                out.extend_from_slice(&count.to_le_bytes());
                out.extend_from_slice(&value.to_le_bytes());
            }
            out.extend_from_slice(&0u32.to_le_bytes()); // next-IFD, patched below
        }

        out[4..8].copy_from_slice(&ifd_offsets[0].to_le_bytes());
        for w in ifd_offsets.windows(2) {
            let ifd = w[0] as usize;
            let entry_count = u16::from_le_bytes([out[ifd], out[ifd + 1]]) as usize;
            let next_ptr = ifd + 2 + entry_count * 12;
            out[next_ptr..next_ptr + 4].copy_from_slice(&w[1].to_le_bytes());
        }
        out
    }

    pub fn write_tiff(path: &Path, pages: &[TiffPage]) {
        std::fs::write(path, build_tiff(pages)).unwrap();
    }

    #[test]
    fn bilevel_black_is_zero_expands_to_full_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g1.tif");
        // 8x2: rows 0b10101010 and 0b11001100.
        write_tiff(
            &path,
            &[TiffPage::gray(8, 2, 1, 1, vec![0b1010_1010, 0b1100_1100])],
        );
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!((pm.width, pm.height), (8, 2));
        let grays: Vec<u8> = pm.data.as_chunks::<4>().0.iter().map(|px| px[0]).collect();
        assert_eq!(
            grays,
            vec![
                255, 0, 255, 0, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 0, 0
            ]
        );
        assert!(pm.data.as_chunks::<4>().0.iter().all(|px| px[3] == 255));
    }

    #[test]
    fn bilevel_white_is_zero_inverts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g1w.tif");
        write_tiff(&path, &[TiffPage::gray(8, 1, 1, 0, vec![0b1010_1010])]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        let grays: Vec<u8> = pm.data.as_chunks::<4>().0.iter().map(|px| px[0]).collect();
        assert_eq!(grays, vec![0, 255, 0, 255, 0, 255, 0, 255]);
    }

    #[test]
    fn bilevel_multi_strip_rows_concatenate() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("strips.tif");
        let mut page = TiffPage::gray(8, 4, 1, 1, vec![0xFF, 0x00, 0xFF, 0x00]);
        page.rows_per_strip = Some(1);
        write_tiff(&path, &[page]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!((pm.width, pm.height), (8, 4));
        let row = |r: usize| pm.data[r * 8 * 4];
        assert_eq!([row(0), row(1), row(2), row(3)], [255, 0, 255, 0]);
    }

    #[test]
    fn four_bit_gray_scales_linearly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g4.tif");
        // 2x1 nibbles: 0xF, 0x5 → 255, 85.
        write_tiff(&path, &[TiffPage::gray(2, 1, 4, 1, vec![0xF5])]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!(pm.data[0], 255);
        assert_eq!(pm.data[4], 85);
    }

    #[test]
    fn sixteen_bit_gray_truncates_high_byte() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g16.tif");
        // LE samples 0x1234, 0xABCD → high bytes 0x12, 0xAB.
        write_tiff(
            &path,
            &[TiffPage::gray(2, 1, 16, 1, vec![0x34, 0x12, 0xCD, 0xAB])],
        );
        let pm = decode_tiff_file_to_pixmap_with_policy(&path, IngestPolicy::default()).unwrap();
        assert_eq!(pm.data[0], 0x12);
        assert_eq!(pm.data[4], 0xAB);
    }

    #[test]
    fn sixteen_bit_rgb_truncates_each_channel() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgb16.tif");
        let page = TiffPage {
            width: 1,
            height: 1,
            bits: vec![16, 16, 16],
            photometric: 2,
            data: vec![0x34, 0x12, 0x00, 0x00, 0xCD, 0xAB],
            rows_per_strip: None,
            colormap: None,
            compression: 1,
            strip_byte_counts: None,
            extra_tags: Vec::new(),
            x_resolution: None,
            y_resolution: None,
            resolution_unit: None,
        };
        write_tiff(&path, &[page]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![0x12, 0x00, 0xAB, 255]);
    }

    #[test]
    fn cmyk_converts_through_documented_transform() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cmyk.tif");
        let page = TiffPage {
            width: 3,
            height: 1,
            bits: vec![8, 8, 8, 8],
            photometric: 5,
            // pure cyan, pure black, paper white
            data: vec![255, 0, 0, 0, 0, 0, 0, 255, 0, 0, 0, 0],
            rows_per_strip: None,
            colormap: None,
            compression: 1,
            strip_byte_counts: None,
            extra_tags: Vec::new(),
            x_resolution: None,
            y_resolution: None,
            resolution_unit: None,
        };
        write_tiff(&path, &[page]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!(&pm.data[0..4], &[0, 255, 255, 255], "pure cyan");
        assert_eq!(&pm.data[4..8], &[0, 0, 0, 255], "pure black");
        assert_eq!(&pm.data[8..12], &[255, 255, 255, 255], "paper white");
    }

    #[test]
    fn palette_maps_through_colormap_high_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pal8.tif");
        let mut cm = vec![0u16; 3 * 256];
        cm[0] = 0xFF00; // R of entry 0
        cm[256 + 1] = 0x8000; // G of entry 1
        cm[2 * 256 + 1] = 0x1234; // B of entry 1
        let page = TiffPage {
            width: 2,
            height: 1,
            bits: vec![8],
            photometric: 3,
            data: vec![0, 1],
            rows_per_strip: None,
            colormap: Some(cm),
            compression: 1,
            strip_byte_counts: None,
            extra_tags: Vec::new(),
            x_resolution: None,
            y_resolution: None,
            resolution_unit: None,
        };
        write_tiff(&path, &[page]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!(&pm.data[0..4], &[0xFF, 0, 0, 255]);
        assert_eq!(&pm.data[4..8], &[0, 0x80, 0x12, 255]);
    }

    #[test]
    fn four_bit_palette_unpacks_indices() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pal4.tif");
        let mut cm = vec![0u16; 3 * 16];
        cm[1] = 0xAA00; // R of entry 1
        cm[16 + 2] = 0xBB00; // G of entry 2
        let page = TiffPage {
            width: 2,
            height: 1,
            bits: vec![4],
            photometric: 3,
            data: vec![0x12], // indices 1, 2
            rows_per_strip: None,
            colormap: Some(cm),
            compression: 1,
            strip_byte_counts: None,
            extra_tags: Vec::new(),
            x_resolution: None,
            y_resolution: None,
            resolution_unit: None,
        };
        write_tiff(&path, &[page]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!(&pm.data[0..4], &[0xAA, 0, 0, 255]);
        assert_eq!(&pm.data[4..8], &[0, 0xBB, 0, 255]);
    }

    #[test]
    fn multipage_returns_all_pages_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("book.tif");
        write_tiff(
            &path,
            &[
                TiffPage::gray(2, 1, 8, 1, vec![10, 20]),
                TiffPage::gray(1, 2, 8, 1, vec![30, 40]),
            ],
        );
        let pages = decode_tiff_file_to_pixmaps(&path, IngestPolicy::default()).unwrap();
        assert_eq!(pages.len(), 2);
        assert_eq!((pages[0].width, pages[0].height), (2, 1));
        assert_eq!((pages[1].width, pages[1].height), (1, 2));
        assert_eq!(pages[1].data[0], 30);
        // Single-page API keeps returning the first page only.
        let first = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!(first.data[0], 10);
    }

    #[test]
    fn compressed_bilevel_reports_targeted_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lzwcomp.tif");
        let mut page = TiffPage::gray(8, 1, 1, 1, vec![0xAA]);
        page.compression = 5; // LZW
        write_tiff(&path, &[page]);
        let err = decode_tiff_file_to_pixmap(&path).unwrap_err().to_string();
        assert!(err.contains("LZW"), "unexpected error: {err}");
        assert!(err.contains("not supported"), "unexpected error: {err}");
    }
}

/// #694 slice 3: CCITT G4 and PackBits compressed strips through the raw
/// bilevel/palette reader. G4 fixtures come from the crate's own
/// [`djvu_rs::smmr::encode_g4`], which emits the same raw T.6 bitstream TIFF
/// stores per strip.
#[cfg(feature = "tiff")]
mod tiff_slice3 {
    use super::tiff_slice2::{TiffPage, write_tiff};
    use djvu_rs::Bitmap;
    use djvu_rs::png_io::decode_tiff_file_to_pixmap;
    use djvu_rs::smmr::encode_g4;

    /// 16-wide bitmap with one black run per row: row r has pixels r..r+4 set.
    fn stair_bitmap(height: u32) -> Bitmap {
        let mut bm = Bitmap::new(16, height);
        for r in 0..height {
            for c in r..(r + 4).min(16) {
                bm.set(c, r, true);
            }
        }
        bm
    }

    fn grays(pm: &djvu_rs::Pixmap) -> Vec<u8> {
        pm.data.as_chunks::<4>().0.iter().map(|px| px[0]).collect()
    }

    #[test]
    fn g4_white_is_zero_single_strip_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g4.tif");
        let bm = stair_bitmap(4);
        let stream = encode_g4(&bm);
        let mut page = TiffPage::gray(16, 4, 1, 0, stream.clone());
        page.compression = 4;
        page.strip_byte_counts = Some(vec![stream.len() as u32]);
        write_tiff(&path, &[page]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!((pm.width, pm.height), (16, 4));
        let g = grays(&pm);
        for r in 0..4u32 {
            for c in 0..16u32 {
                let expect = if bm.get(c, r) { 0 } else { 255 };
                assert_eq!(g[(r * 16 + c) as usize], expect, "pixel ({c},{r})");
            }
        }
    }

    #[test]
    fn g4_black_is_zero_renders_inverted() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g4inv.tif");
        let bm = stair_bitmap(2);
        let stream = encode_g4(&bm);
        let mut page = TiffPage::gray(16, 2, 1, 1, stream.clone());
        page.compression = 4;
        page.strip_byte_counts = Some(vec![stream.len() as u32]);
        write_tiff(&path, &[page]);
        let g = grays(&decode_tiff_file_to_pixmap(&path).unwrap());
        // Fax black packs as sample 1; BlackIsZero maps sample 1 to white.
        assert_eq!(g[0], 255, "fax black pixel renders white");
        assert_eq!(g[15], 0, "fax white pixel renders black");
    }

    #[test]
    fn g4_multi_strip_streams_are_independent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g4strips.tif");
        let full = stair_bitmap(6);
        // Three strips of two rows, each an independent T.6 stream.
        let mut data = Vec::new();
        let mut counts = Vec::new();
        for s in 0..3u32 {
            let mut part = Bitmap::new(16, 2);
            for r in 0..2 {
                for c in 0..16 {
                    if full.get(c, s * 2 + r) {
                        part.set(c, r, true);
                    }
                }
            }
            let stream = encode_g4(&part);
            counts.push(stream.len() as u32);
            data.extend_from_slice(&stream);
        }
        let mut page = TiffPage::gray(16, 6, 1, 0, data);
        page.compression = 4;
        page.rows_per_strip = Some(2);
        page.strip_byte_counts = Some(counts);
        write_tiff(&path, &[page]);
        let g = grays(&decode_tiff_file_to_pixmap(&path).unwrap());
        for r in 0..6u32 {
            for c in 0..16u32 {
                let expect = if full.get(c, r) { 0 } else { 255 };
                assert_eq!(g[(r * 16 + c) as usize], expect, "pixel ({c},{r})");
            }
        }
    }

    #[test]
    fn g4_t6options_uncompressed_mode_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g4uncomp.tif");
        let bm = stair_bitmap(1);
        let stream = encode_g4(&bm);
        let mut page = TiffPage::gray(16, 1, 1, 0, stream.clone());
        page.compression = 4;
        page.strip_byte_counts = Some(vec![stream.len() as u32]);
        page.extra_tags = vec![(293, 4, 1, 2)]; // T6Options: uncompressed mode
        write_tiff(&path, &[page]);
        let err = decode_tiff_file_to_pixmap(&path).unwrap_err().to_string();
        assert!(err.contains("uncompressed mode"), "unexpected error: {err}");
    }

    #[test]
    fn g4_palette_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g4pal.tif");
        let mut page = TiffPage::gray(8, 1, 1, 3, vec![0]);
        page.compression = 4;
        page.colormap = Some(vec![0u16; 6]);
        write_tiff(&path, &[page]);
        let err = decode_tiff_file_to_pixmap(&path).unwrap_err().to_string();
        assert!(err.contains("palette"), "unexpected error: {err}");
        assert!(err.contains("G4"), "unexpected error: {err}");
    }

    #[test]
    fn packbits_bilevel_literal_and_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pb1.tif");
        // Rows 0xAA, 0xCC as one literal run, preceded by a -128 no-op byte.
        let data = vec![0x80, 0x01, 0xAA, 0xCC];
        let mut page = TiffPage::gray(8, 2, 1, 1, data);
        page.compression = 32773;
        page.strip_byte_counts = Some(vec![4]);
        write_tiff(&path, &[page]);
        let g = grays(&decode_tiff_file_to_pixmap(&path).unwrap());
        assert_eq!(
            g,
            vec![
                255, 0, 255, 0, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 0, 0
            ]
        );
    }

    #[test]
    fn packbits_repeat_runs_expand() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pb8.tif");
        // 8-bit gray, 4 px: repeat 7 three times, then literal 9.
        let data = vec![0xFE, 7, 0x00, 9];
        let mut page = TiffPage::gray(4, 1, 8, 1, data);
        page.compression = 32773;
        page.strip_byte_counts = Some(vec![4]);
        write_tiff(&path, &[page]);
        let g = grays(&decode_tiff_file_to_pixmap(&path).unwrap());
        assert_eq!(g, vec![7, 7, 7, 9]);
    }

    #[test]
    fn packbits_palette_decodes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pbpal.tif");
        let mut cm = vec![0u16; 3 * 256];
        cm[0] = 0xFF00; // R of entry 0
        cm[256 + 1] = 0x8000; // G of entry 1
        let mut page = TiffPage::gray(2, 1, 8, 3, vec![0x01, 0, 1]);
        page.compression = 32773;
        page.colormap = Some(cm);
        page.strip_byte_counts = Some(vec![3]);
        write_tiff(&path, &[page]);
        let pm = decode_tiff_file_to_pixmap(&path).unwrap();
        assert_eq!(&pm.data[0..4], &[0xFF, 0, 0, 255]);
        assert_eq!(&pm.data[4..8], &[0, 0x80, 0, 255]);
    }

    #[test]
    fn packbits_truncated_reports_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pbbad.tif");
        // Control byte promises 6 literals, none follow.
        let mut page = TiffPage::gray(8, 1, 1, 1, vec![0x05]);
        page.compression = 32773;
        page.strip_byte_counts = Some(vec![1]);
        write_tiff(&path, &[page]);
        let err = decode_tiff_file_to_pixmap(&path).unwrap_err().to_string();
        assert!(err.contains("PackBits"), "unexpected error: {err}");
    }
}

#[cfg(feature = "tiff")]
mod tiff_resolution {
    use super::tiff_slice2::{TiffPage, write_tiff};
    use assert_cmd::Command;
    use djvu_rs::png_io::tiff_file_dpi;
    use std::path::Path;

    /// 8×2 bilevel WhiteIsZero page with the given resolution tags.
    fn page(xres: Option<(u32, u32)>, yres: Option<(u32, u32)>, unit: Option<u16>) -> TiffPage {
        let mut p = TiffPage::gray(8, 2, 1, 0, vec![0xF0, 0x0F]);
        p.x_resolution = xres;
        p.y_resolution = yres;
        p.resolution_unit = unit;
        p
    }

    fn dpi_of(path: &Path) -> Option<u16> {
        tiff_file_dpi(path).unwrap()
    }

    #[test]
    fn inch_resolution_maps_directly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inch.tif");
        write_tiff(&path, &[page(Some((300, 1)), None, Some(2))]);
        assert_eq!(dpi_of(&path), Some(300));
    }

    #[test]
    fn centimeter_resolution_converts_to_inches() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cm.tif");
        // 118 dots/cm × 2.54 = 299.72 → rounds to 300.
        write_tiff(&path, &[page(Some((118, 1)), None, Some(3))]);
        assert_eq!(dpi_of(&path), Some(300));
    }

    #[test]
    fn rational_denominator_is_honored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rat.tif");
        // 6000/20 = 300; ResolutionUnit absent defaults to inch.
        write_tiff(&path, &[page(Some((6000, 20)), None, None)]);
        assert_eq!(dpi_of(&path), Some(300));
    }

    #[test]
    fn missing_tags_yield_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("none.tif");
        write_tiff(&path, &[page(None, None, None)]);
        assert_eq!(dpi_of(&path), None);
    }

    #[test]
    fn unit_none_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("aspect.tif");
        // ResolutionUnit 1 means "no absolute unit": the rational is only
        // an aspect ratio, not a physical density.
        write_tiff(&path, &[page(Some((300, 1)), None, Some(1))]);
        assert_eq!(dpi_of(&path), None);
    }

    #[test]
    fn out_of_range_resolution_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.tif");
        write_tiff(&path, &[page(Some((1, 1)), None, Some(2))]);
        assert_eq!(dpi_of(&path), None);
    }

    #[test]
    fn y_resolution_is_a_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("yonly.tif");
        write_tiff(&path, &[page(None, Some((200, 1)), Some(2))]);
        assert_eq!(dpi_of(&path), Some(200));
    }

    fn encoded_dpi(input: &Path, output: &Path, extra: &[&str]) -> u16 {
        let mut args = vec![
            "encode",
            input.to_str().unwrap(),
            "-o",
            output.to_str().unwrap(),
            "--quality",
            "lossless",
        ];
        args.extend_from_slice(extra);
        Command::cargo_bin("djvu")
            .unwrap()
            .args(&args)
            .assert()
            .success();
        let doc = djvu_rs::Document::from_bytes(std::fs::read(output).unwrap()).unwrap();
        doc.page(0).unwrap().dpi()
    }

    #[test]
    fn cli_uses_tiff_resolution_when_dpi_flag_absent() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("res200.tif");
        write_tiff(&input, &[page(Some((200, 1)), None, Some(2))]);
        let output = dir.path().join("res200.djvu");
        assert_eq!(encoded_dpi(&input, &output, &[]), 200);
    }

    #[test]
    fn cli_explicit_dpi_flag_wins() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("res200b.tif");
        write_tiff(&input, &[page(Some((200, 1)), None, Some(2))]);
        let output = dir.path().join("res200b.djvu");
        assert_eq!(encoded_dpi(&input, &output, &["--dpi", "150"]), 150);
    }

    #[test]
    fn cli_defaults_to_300_without_resolution_tags() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("nores.tif");
        write_tiff(&input, &[page(None, None, None)]);
        let output = dir.path().join("nores.djvu");
        assert_eq!(encoded_dpi(&input, &output, &[]), 300);
    }
}

#[cfg(feature = "tiff")]
mod tiff_fastpath {
    use super::tiff_slice2::{TiffPage, write_tiff};
    use assert_cmd::Command;
    use djvu_rs::Bitmap;
    use djvu_rs::ingest::IngestPolicy;
    use djvu_rs::png_io::decode_tiff_file_to_bitmaps;
    use djvu_rs::smmr::encode_g4;
    use std::path::Path;

    /// 16-wide bitmap with one black run per row: row r has pixels r..r+4 set.
    fn stair_bitmap(height: u32) -> Bitmap {
        let mut bm = Bitmap::new(16, height);
        for r in 0..height {
            for c in r..(r + 4).min(16) {
                bm.set(c, r, true);
            }
        }
        bm
    }

    /// A 1-bit page carrying the packed rows of `bm` verbatim.
    fn bilevel_page(bm: &Bitmap, photometric: u16) -> TiffPage {
        TiffPage::gray(bm.width, bm.height, 1, photometric, bm.data.clone())
    }

    #[test]
    fn white_is_zero_page_decodes_to_bitmap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wz.tif");
        let bm = stair_bitmap(4);
        write_tiff(&path, &[bilevel_page(&bm, 0)]);
        let pages = decode_tiff_file_to_bitmaps(&path, IngestPolicy::default())
            .unwrap()
            .unwrap();
        assert_eq!(pages, vec![bm]);
    }

    #[test]
    fn black_is_zero_page_inverts_and_clears_padding() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bz.tif");
        // Width 12: the last 4 bits of each row byte-pair are padding and
        // must come out zero after the BlackIsZero inversion.
        let page = TiffPage::gray(12, 2, 1, 1, vec![0xF0, 0x00, 0x00, 0xF0]);
        write_tiff(&path, &[page]);
        let pages = decode_tiff_file_to_bitmaps(&path, IngestPolicy::default())
            .unwrap()
            .unwrap();
        let mut expected = Bitmap::new(12, 2);
        for x in 0..12 {
            expected.set(x, 0, x >= 4); // row 0: samples 1 on 0..4 → white
            expected.set(x, 1, !(8..12).contains(&x));
        }
        assert_eq!(pages, vec![expected]);
    }

    #[test]
    fn g4_page_round_trips_to_bitmap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g4fast.tif");
        let bm = stair_bitmap(6);
        let stream = encode_g4(&bm);
        let mut page = TiffPage::gray(16, 6, 1, 0, stream.clone());
        page.compression = 4;
        page.strip_byte_counts = Some(vec![stream.len() as u32]);
        write_tiff(&path, &[page]);
        let pages = decode_tiff_file_to_bitmaps(&path, IngestPolicy::default())
            .unwrap()
            .unwrap();
        assert_eq!(pages, vec![bm]);
    }

    #[test]
    fn gray8_page_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("g8.tif");
        write_tiff(&path, &[TiffPage::gray(4, 1, 8, 1, vec![0, 64, 128, 255])]);
        assert!(
            decode_tiff_file_to_bitmaps(&path, IngestPolicy::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn mixed_multipage_yields_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mixed.tif");
        let bm = stair_bitmap(2);
        write_tiff(
            &path,
            &[
                bilevel_page(&bm, 0),
                TiffPage::gray(4, 1, 8, 1, vec![0, 64, 128, 255]),
            ],
        );
        assert!(
            decode_tiff_file_to_bitmaps(&path, IngestPolicy::default())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn multipage_bilevel_decodes_every_page() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.tif");
        let (a, b) = (stair_bitmap(3), stair_bitmap(5));
        write_tiff(&path, &[bilevel_page(&a, 0), bilevel_page(&b, 0)]);
        let pages = decode_tiff_file_to_bitmaps(&path, IngestPolicy::default())
            .unwrap()
            .unwrap();
        assert_eq!(pages, vec![a, b]);
    }

    fn encode_auto(input: &Path, output: &Path) -> String {
        let assert = Command::cargo_bin("djvu")
            .unwrap()
            .args([
                "encode",
                input.to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--quality",
                "auto",
            ])
            .assert()
            .success();
        String::from_utf8_lossy(&assert.get_output().stderr).into_owned()
    }

    #[test]
    fn cli_auto_takes_fast_path_and_round_trips_pixels() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("fast.tif");
        let bm = stair_bitmap(8);
        write_tiff(&input, &[bilevel_page(&bm, 0)]);
        let output = dir.path().join("fast.djvu");
        let stderr = encode_auto(&input, &output);
        assert!(
            stderr.contains("auto profile: Lossless (1-bit TIFF)"),
            "fast-path marker missing in stderr: {stderr}"
        );
        let pm = super::render_first_page(&output);
        for y in 0..8u32 {
            for x in 0..16u32 {
                let px = pm.data[((y * 16 + x) * 4) as usize];
                let expect = if bm.get(x, y) { 0 } else { 255 };
                assert_eq!(px, expect, "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn cli_auto_multipage_builds_lossless_bundle() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("fastmulti.tif");
        let bm = stair_bitmap(4);
        write_tiff(&input, &[bilevel_page(&bm, 0), bilevel_page(&bm, 1)]);
        let output = dir.path().join("fastmulti.djvu");
        let stderr = encode_auto(&input, &output);
        assert!(
            stderr.contains("auto profile: Lossless (1-bit TIFF)"),
            "fast-path marker missing in stderr: {stderr}"
        );
        let doc = djvu_rs::Document::from_bytes(std::fs::read(&output).unwrap()).unwrap();
        assert_eq!(doc.page_count(), 2);
    }

    #[test]
    fn cli_auto_all_white_page_stays_lossless() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("blank.tif");
        // A blank 1-bit page is bilevel by construction; the pixel-statistics
        // classifier used to route it to a layered profile.
        write_tiff(&input, &[bilevel_page(&Bitmap::new(32, 8), 0)]);
        let output = dir.path().join("blank.djvu");
        let stderr = encode_auto(&input, &output);
        assert!(
            stderr.contains("auto profile: Lossless (1-bit TIFF)"),
            "fast-path marker missing in stderr: {stderr}"
        );
        let pm = super::render_first_page(&output);
        assert!(pm.data.as_chunks::<4>().0.iter().all(|px| px[0] == 255));
    }
}

#[cfg(feature = "tiff")]
mod tiff_orientation {
    use super::tiff_slice2::{TiffPage, write_tiff};
    use djvu_rs::Bitmap;
    use djvu_rs::ingest::IngestPolicy;
    use djvu_rs::png_io::{decode_tiff_file_to_bitmaps, decode_tiff_file_to_pixmap, tiff_file_dpi};

    /// 3×2 gray page (rows `10 20 30` / `40 50 60`) with an Orientation tag.
    fn oriented_gray_page(orientation: u32) -> TiffPage {
        let mut p = TiffPage::gray(3, 2, 8, 1, vec![10, 20, 30, 40, 50, 60]);
        p.extra_tags = vec![(274, 3, 1, orientation)];
        p
    }

    fn grays(pm: &djvu_rs::Pixmap) -> Vec<u8> {
        pm.data.as_chunks::<4>().0.iter().map(|px| px[0]).collect()
    }

    #[test]
    fn all_eight_orientations_reorder_pixels() {
        // Hand-written expected grids — independent of the implementation.
        let cases: [(u32, (u32, u32), Vec<u8>); 8] = [
            (1, (3, 2), vec![10, 20, 30, 40, 50, 60]),
            (2, (3, 2), vec![30, 20, 10, 60, 50, 40]), // mirrored horizontally
            (3, (3, 2), vec![60, 50, 40, 30, 20, 10]), // rotated 180°
            (4, (3, 2), vec![40, 50, 60, 10, 20, 30]), // mirrored vertically
            (5, (2, 3), vec![10, 40, 20, 50, 30, 60]), // transposed
            (6, (2, 3), vec![40, 10, 50, 20, 60, 30]), // rotated 90° CW
            (7, (2, 3), vec![60, 30, 50, 20, 40, 10]), // anti-transposed
            (8, (2, 3), vec![30, 60, 20, 50, 10, 40]), // rotated 90° CCW
        ];
        let dir = tempfile::tempdir().unwrap();
        for (o, dims, expected) in cases {
            let path = dir.path().join(format!("o{o}.tif"));
            write_tiff(&path, &[oriented_gray_page(o)]);
            let pm = decode_tiff_file_to_pixmap(&path).unwrap();
            assert_eq!((pm.width, pm.height), dims, "orientation {o} dims");
            assert_eq!(grays(&pm), expected, "orientation {o} pixels");
        }
    }

    #[test]
    fn out_of_range_orientation_is_upright() {
        let dir = tempfile::tempdir().unwrap();
        for bad in [0u32, 9] {
            let path = dir.path().join(format!("bad{bad}.tif"));
            write_tiff(&path, &[oriented_gray_page(bad)]);
            let pm = decode_tiff_file_to_pixmap(&path).unwrap();
            assert_eq!(grays(&pm), vec![10, 20, 30, 40, 50, 60]);
        }
    }

    /// 16×4 bilevel page: row r has pixels r..r+4 black, plus corner (15, 3).
    fn asymmetric_bitmap() -> Bitmap {
        let mut bm = Bitmap::new(16, 4);
        for r in 0..4 {
            for c in r..r + 4 {
                bm.set(c, r, true);
            }
        }
        bm.set(15, 3, true);
        bm
    }

    #[test]
    fn bilevel_fast_path_matches_pixmap_route_for_every_orientation() {
        let dir = tempfile::tempdir().unwrap();
        let bm = asymmetric_bitmap();
        for o in 1..=8u32 {
            let path = dir.path().join(format!("bl{o}.tif"));
            let mut page = TiffPage::gray(16, 4, 1, 0, bm.data.clone());
            page.extra_tags = vec![(274, 3, 1, o)];
            write_tiff(&path, &[page]);
            let fast = decode_tiff_file_to_bitmaps(&path, IngestPolicy::default())
                .unwrap()
                .unwrap();
            let pm = decode_tiff_file_to_pixmap(&path).unwrap();
            assert_eq!(
                (fast[0].width, fast[0].height),
                (pm.width, pm.height),
                "orientation {o} dims"
            );
            for y in 0..pm.height {
                for x in 0..pm.width {
                    let black = pm.data[((y * pm.width + x) * 4) as usize] == 0;
                    assert_eq!(fast[0].get(x, y), black, "orientation {o} pixel ({x},{y})");
                }
            }
        }
    }

    #[test]
    fn rotated_page_prefers_y_resolution_for_dpi() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rotdpi.tif");
        let mut page = oriented_gray_page(6);
        page.x_resolution = Some((300, 1));
        page.y_resolution = Some((150, 1));
        page.resolution_unit = Some(2);
        write_tiff(&path, &[page]);
        // Rotated 90°: the stored Y axis becomes the visual horizontal.
        assert_eq!(tiff_file_dpi(&path).unwrap(), Some(150));
    }

    #[test]
    fn multipage_orientation_applies_per_page() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi.tif");
        write_tiff(&path, &[oriented_gray_page(1), oriented_gray_page(3)]);
        let pages = djvu_rs::png_io::decode_tiff_file_to_pixmaps(
            &path,
            djvu_rs::ingest::IngestPolicy::default(),
        )
        .unwrap();
        assert_eq!(grays(&pages[0]), vec![10, 20, 30, 40, 50, 60]);
        assert_eq!(grays(&pages[1]), vec![60, 50, 40, 30, 20, 10]);
    }
}

mod alpha_compositing {
    use assert_cmd::Command;
    use djvu_rs::ingest::{AlphaCompositing, IngestPolicy};
    use djvu_rs::png_io::{decode_image_to_pixmap_with_policy, decode_png_to_pixmap};
    use std::path::Path;

    fn on_background(red: u8, green: u8, blue: u8) -> IngestPolicy {
        IngestPolicy {
            alpha: AlphaCompositing::CompositeOnBackground { red, green, blue },
            ..Default::default()
        }
    }

    /// 2×1 RGBA PNG: one half-transparent pixel, one fully transparent.
    fn write_test_png(path: &Path) {
        super::write_png(
            path,
            2,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[200, 100, 0, 128, 55, 66, 77, 0],
        );
    }

    #[test]
    fn png_composites_onto_background() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        write_test_png(&path);
        // (c·a + bg·(255 − a) + 127) / 255; fully transparent pixels become
        // the background colour exactly.
        let white =
            decode_image_to_pixmap_with_policy(&path, on_background(255, 255, 255)).unwrap();
        assert_eq!(white.data, vec![227, 177, 127, 255, 255, 255, 255, 255]);
        let black = decode_image_to_pixmap_with_policy(&path, on_background(0, 0, 0)).unwrap();
        assert_eq!(black.data, vec![100, 50, 0, 255, 0, 0, 0, 255]);
    }

    #[test]
    fn default_policy_preserves_alpha() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.png");
        write_test_png(&path);
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![200, 100, 0, 128, 55, 66, 77, 0]);
    }

    #[cfg(feature = "tiff")]
    #[test]
    fn tiff_rgba_composites() {
        use super::tiff_slice2::{TiffPage, write_tiff};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rgba.tif");
        // RGBA8, 2×1: same pixels as the PNG test. The builder derives row
        // bytes from the first sample only, so the strip byte count is
        // explicit; tag 338 marks the unassociated alpha sample. (GrayA8 is
        // not testable here: tiff 0.9 rejects BlackIsZero with two samples.)
        let mut page = TiffPage::gray(2, 1, 8, 2, vec![200, 100, 0, 128, 55, 66, 77, 0]);
        page.bits = vec![8, 8, 8, 8];
        page.strip_byte_counts = Some(vec![8]);
        page.extra_tags = vec![(338, 3, 1, 2)];
        write_tiff(&path, &[page]);
        let pm = decode_image_to_pixmap_with_policy(&path, on_background(255, 255, 255)).unwrap();
        assert_eq!(pm.data, vec![227, 177, 127, 255, 255, 255, 255, 255]);
    }

    fn encode_with_background(background: &str, output: &Path) {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("t.png");
        // 8×8 fully transparent page: the rendered result is exactly the
        // chosen background colour.
        super::write_png(
            &input,
            8,
            8,
            png::ColorType::Rgba,
            png::BitDepth::Eight,
            &[0u8; 8 * 8 * 4],
        );
        Command::cargo_bin("djvu")
            .unwrap()
            .args([
                "encode",
                input.to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--quality",
                "lossless",
                "--background",
                background,
            ])
            .assert()
            .success();
    }

    #[test]
    fn cli_background_sets_page_colour() {
        let dir = tempfile::tempdir().unwrap();
        for (arg, expected) in [("white", 255u8), ("000000", 0u8), ("#000000", 0u8)] {
            let output = dir.path().join("t.djvu");
            encode_with_background(arg, &output);
            let pm = super::render_first_page(&output);
            let center = ((4 * pm.width + 4) * 4) as usize;
            assert_eq!(pm.data[center], expected, "--background {arg}");
        }
    }

    #[test]
    fn cli_rejects_invalid_background() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("t.png");
        write_test_png(&input);
        Command::cargo_bin("djvu")
            .unwrap()
            .args([
                "encode",
                input.to_str().unwrap(),
                "-o",
                dir.path().join("t.djvu").to_str().unwrap(),
                "--background",
                "chartreuse",
            ])
            .assert()
            .failure()
            .stderr(predicates::str::contains("invalid colour"));
    }
}

mod jpeg_exif {
    use assert_cmd::Command;
    use djvu_rs::png_io::decode_jpeg_file_to_pixmap;
    use std::path::Path;

    /// APP1 payload: `Exif\0\0` + minimal TIFF block with one IFD0 entry —
    /// Orientation (274) as a single SHORT.
    fn exif_app1(orientation: u16, le: bool) -> Vec<u8> {
        let mut v = b"Exif\0\0".to_vec();
        macro_rules! push {
            ($val:expr) => {
                if le {
                    v.extend_from_slice(&$val.to_le_bytes());
                } else {
                    v.extend_from_slice(&$val.to_be_bytes());
                }
            };
        }
        v.extend_from_slice(if le { b"II" } else { b"MM" });
        push!(42u16);
        push!(8u32); // IFD0 offset
        push!(1u16); // entry count
        push!(274u16);
        push!(3u16); // type SHORT
        push!(1u32); // count
        push!(orientation);
        push!(0u16); // value field padding
        push!(0u32); // next IFD offset
        v
    }

    /// 3×2 gray-valued RGB JPEG (rows `a b c` / `d e f`) with an EXIF APP1.
    fn write_jpeg(path: &Path, app1: Option<&[u8]>) {
        let mut pixels = Vec::new();
        for g in [10u8, 60, 110, 160, 210, 250] {
            pixels.extend_from_slice(&[g, g, g]);
        }
        let mut encoder = jpeg_encoder::Encoder::new_file(path, 100).unwrap();
        if let Some(data) = app1 {
            encoder.add_app_segment(1, data).unwrap();
        }
        encoder
            .encode(&pixels, 3, 2, jpeg_encoder::ColorType::Rgb)
            .unwrap();
    }

    fn grays(pm: &djvu_rs::Pixmap) -> Vec<u8> {
        pm.data.as_chunks::<4>().0.iter().map(|px| px[0]).collect()
    }

    #[test]
    fn all_eight_exif_orientations_reorder_pixels() {
        // Hand-written index permutations of the upright grid [0 1 2 / 3 4 5]
        // — independent of the implementation. Pixel values come from the
        // baseline decode, so comparisons are exact despite JPEG loss.
        let cases: [(u16, (u32, u32), [usize; 6]); 8] = [
            (1, (3, 2), [0, 1, 2, 3, 4, 5]),
            (2, (3, 2), [2, 1, 0, 5, 4, 3]), // mirrored horizontally
            (3, (3, 2), [5, 4, 3, 2, 1, 0]), // rotated 180°
            (4, (3, 2), [3, 4, 5, 0, 1, 2]), // mirrored vertically
            (5, (2, 3), [0, 3, 1, 4, 2, 5]), // transposed
            (6, (2, 3), [3, 0, 4, 1, 5, 2]), // rotated 90° CW
            (7, (2, 3), [5, 2, 4, 1, 3, 0]), // anti-transposed
            (8, (2, 3), [2, 5, 1, 4, 0, 3]), // rotated 90° CCW
        ];
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("base.jpg");
        write_jpeg(&base_path, None);
        let base = grays(&decode_jpeg_file_to_pixmap(&base_path).unwrap());
        for (o, dims, perm) in cases {
            let path = dir.path().join(format!("o{o}.jpg"));
            write_jpeg(&path, Some(&exif_app1(o, true)));
            let pm = decode_jpeg_file_to_pixmap(&path).unwrap();
            assert_eq!((pm.width, pm.height), dims, "orientation {o} dims");
            let expected: Vec<u8> = perm.iter().map(|&i| base[i]).collect();
            assert_eq!(grays(&pm), expected, "orientation {o} pixels");
        }
    }

    #[test]
    fn big_endian_exif_is_parsed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mm.jpg");
        write_jpeg(&path, Some(&exif_app1(3, false)));
        let base_path = dir.path().join("base.jpg");
        write_jpeg(&base_path, None);
        let base = grays(&decode_jpeg_file_to_pixmap(&base_path).unwrap());
        let pm = decode_jpeg_file_to_pixmap(&path).unwrap();
        let expected: Vec<u8> = [5, 4, 3, 2, 1, 0].iter().map(|&i| base[i]).collect();
        assert_eq!(grays(&pm), expected);
    }

    #[test]
    fn out_of_range_or_malformed_exif_is_upright() {
        let dir = tempfile::tempdir().unwrap();
        let base_path = dir.path().join("base.jpg");
        write_jpeg(&base_path, None);
        let base = grays(&decode_jpeg_file_to_pixmap(&base_path).unwrap());
        let payloads: Vec<(&str, Vec<u8>)> = vec![
            ("orientation 0", exif_app1(0, true)),
            ("orientation 9", exif_app1(9, true)),
            ("truncated", exif_app1(6, true)[..12].to_vec()),
            ("garbage", b"Exif\0\0not a tiff header at all".to_vec()),
        ];
        for (ctx, app1) in payloads {
            let path = dir.path().join("bad.jpg");
            write_jpeg(&path, Some(&app1));
            let pm = decode_jpeg_file_to_pixmap(&path).unwrap();
            assert_eq!((pm.width, pm.height), (3, 2), "{ctx}: dims");
            assert_eq!(grays(&pm), base, "{ctx}: pixels");
        }
    }

    #[test]
    fn cli_encode_swaps_dimensions_for_rotated_jpeg() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("rot.jpg");
        let output = dir.path().join("rot.djvu");
        write_jpeg(&input, Some(&exif_app1(6, true)));
        Command::cargo_bin("djvu")
            .unwrap()
            .args([
                "encode",
                input.to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--quality",
                "quality",
            ])
            .assert()
            .success();
        let bytes = std::fs::read(&output).unwrap();
        let doc = djvu_rs::Document::from_bytes(bytes).unwrap();
        let page = doc.page(0).unwrap();
        assert_eq!((page.width(), page.height()), (2, 3));
    }
}

mod jpeg_cmyk {
    use assert_cmd::Command;
    use djvu_rs::png_io::decode_jpeg_file_to_pixmap;
    use std::path::Path;

    /// Write a 16×16 solid-color JPEG from true ink values `[C, M, Y, K]`.
    ///
    /// `jpeg-encoder` stores CMYK Adobe-inverted (`255 − v`) and writes the
    /// APP14 marker (transform 0 for `Cmyk`, transform 2 for `CmykAsYcck`),
    /// matching Photoshop/libjpeg conventions.
    fn write_cmyk_jpeg(
        path: &Path,
        color_type: jpeg_encoder::ColorType,
        cmyk: [u8; 4],
        progressive: bool,
    ) {
        let mut pixels = Vec::with_capacity(16 * 16 * 4);
        for _ in 0..16 * 16 {
            pixels.extend_from_slice(&cmyk);
        }
        let mut encoder = jpeg_encoder::Encoder::new_file(path, 100).unwrap();
        if progressive {
            encoder.set_progressive(true);
        }
        encoder.encode(&pixels, 16, 16, color_type).unwrap();
    }

    /// The naive profile-free transform documented for CMYK TIFF ingest:
    /// `channel = (255 − ink) · (255 − K) / 255`.
    fn expected_rgb([c, m, y, k]: [u8; 4]) -> [u8; 3] {
        let mix = |ink: u8| (((255 - ink as u32) * (255 - k as u32) + 127) / 255) as u8;
        [mix(c), mix(m), mix(y)]
    }

    fn assert_decodes_close(path: &Path, expected: [u8; 3], tol: i16, ctx: &str) {
        let pm = decode_jpeg_file_to_pixmap(path).unwrap();
        assert_eq!((pm.width, pm.height), (16, 16), "{ctx}: dims");
        let i = ((8 * pm.width + 8) * 4) as usize;
        let got = [pm.data[i], pm.data[i + 1], pm.data[i + 2]];
        for ch in 0..3 {
            assert!(
                (got[ch] as i16 - expected[ch] as i16).abs() <= tol,
                "{ctx}: got {got:?}, want {expected:?} ±{tol}"
            );
        }
        assert_eq!(pm.data[i + 3], 255, "{ctx}: alpha");
    }

    const INKS: [[u8; 4]; 4] = [
        [0, 0, 0, 0],      // white
        [255, 0, 0, 0],    // pure cyan
        [0, 0, 0, 255],    // pure black ink
        [64, 128, 32, 16], // mixed inks
    ];

    #[test]
    fn cmyk_jpeg_decodes_with_naive_transform() {
        let dir = tempfile::tempdir().unwrap();
        for cmyk in INKS {
            let path = dir.path().join("c.jpg");
            write_cmyk_jpeg(&path, jpeg_encoder::ColorType::Cmyk, cmyk, false);
            assert_decodes_close(&path, expected_rgb(cmyk), 3, &format!("cmyk {cmyk:?}"));
        }
    }

    #[test]
    fn ycck_jpeg_decodes_with_naive_transform() {
        let dir = tempfile::tempdir().unwrap();
        for cmyk in INKS {
            let path = dir.path().join("y.jpg");
            write_cmyk_jpeg(&path, jpeg_encoder::ColorType::CmykAsYcck, cmyk, false);
            // The extra YCbCr round-trip costs a little precision.
            assert_decodes_close(&path, expected_rgb(cmyk), 5, &format!("ycck {cmyk:?}"));
        }
    }

    #[test]
    fn progressive_cmyk_jpeg_decodes() {
        let dir = tempfile::tempdir().unwrap();
        for color_type in [
            jpeg_encoder::ColorType::Cmyk,
            jpeg_encoder::ColorType::CmykAsYcck,
        ] {
            let path = dir.path().join("p.jpg");
            write_cmyk_jpeg(&path, color_type, [64, 128, 32, 16], true);
            assert_decodes_close(
                &path,
                expected_rgb([64, 128, 32, 16]),
                5,
                &format!("progressive {color_type:?}"),
            );
        }
    }

    #[test]
    fn cli_encodes_cmyk_jpeg_to_layered_djvu() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("ink.jpg");
        let output = dir.path().join("ink.djvu");
        write_cmyk_jpeg(&input, jpeg_encoder::ColorType::Cmyk, [255, 0, 0, 0], false);
        Command::cargo_bin("djvu")
            .unwrap()
            .args([
                "encode",
                input.to_str().unwrap(),
                "-o",
                output.to_str().unwrap(),
                "--quality",
                "quality",
            ])
            .assert()
            .success();
        let pm = super::render_first_page(&output);
        assert_eq!((pm.width, pm.height), (16, 16));
        let i = ((8 * pm.width + 8) * 4) as usize;
        let [r, g, b] = [pm.data[i], pm.data[i + 1], pm.data[i + 2]];
        // Pure cyan survives the lossy IW44 background layer approximately.
        assert!(r < 60 && g > 195 && b > 195, "rendered ({r},{g},{b})");
    }
}

mod icc_policy {
    use assert_cmd::Command;
    use djvu_rs::ingest::{IccHandling, IngestPolicy};
    use djvu_rs::png_io::{decode_image_to_pixmap_with_policy, decode_jpeg_file_to_pixmap};
    use std::path::Path;

    /// Any bytes work: detection keys on presence, not profile validity.
    const PROFILE: &[u8] = b"not a real ICC profile, presence is what counts";

    fn reject() -> IngestPolicy {
        IngestPolicy {
            icc: IccHandling::Reject,
            ..Default::default()
        }
    }

    /// 1×1 gray PNG with an iCCP chunk.
    fn write_png_with_iccp(path: &Path) {
        let mut info = png::Info::with_size(1, 1);
        info.color_type = png::ColorType::Grayscale;
        info.bit_depth = png::BitDepth::Eight;
        info.icc_profile = Some(PROFILE.into());
        let file = std::fs::File::create(path).unwrap();
        let encoder = png::Encoder::with_info(std::io::BufWriter::new(file), info).unwrap();
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&[128]).unwrap();
    }

    /// 1×1 gray JPEG with an APP2 `ICC_PROFILE` segment (chunk 1 of 1).
    fn write_jpeg_with_icc(path: &Path) {
        let mut app2 = b"ICC_PROFILE\0\x01\x01".to_vec();
        app2.extend_from_slice(PROFILE);
        let mut encoder = jpeg_encoder::Encoder::new_file(path, 90).unwrap();
        encoder.add_app_segment(2, &app2).unwrap();
        encoder
            .encode(&[128], 1, 1, jpeg_encoder::ColorType::Luma)
            .unwrap();
    }

    #[test]
    fn png_iccp_ignored_by_default_rejected_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.png");
        write_png_with_iccp(&path);
        let pm = decode_image_to_pixmap_with_policy(&path, IngestPolicy::default()).unwrap();
        assert_eq!(pm.data, vec![128, 128, 128, 255]);
        let err = decode_image_to_pixmap_with_policy(&path, reject()).unwrap_err();
        assert!(err.to_string().contains("ICC"), "{err}");
    }

    #[test]
    fn jpeg_app2_ignored_by_default_rejected_on_demand() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.jpg");
        write_jpeg_with_icc(&path);
        assert!(decode_jpeg_file_to_pixmap(&path).is_ok());
        let err = decode_image_to_pixmap_with_policy(&path, reject()).unwrap_err();
        assert!(err.to_string().contains("ICC"), "{err}");
    }

    #[cfg(feature = "tiff")]
    #[test]
    fn tiff_icc_tag_ignored_by_default_rejected_on_demand() {
        use super::tiff_slice2::{TiffPage, write_tiff};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.tif");
        // Tag 34675 (InterColorProfile), type 7 (UNDEFINED), 4 bytes inline.
        let mut page = TiffPage::gray(1, 1, 8, 1, vec![128]);
        page.extra_tags = vec![(34675, 7, 4, u32::from_le_bytes(*b"acsp"))];
        write_tiff(&path, &[page]);
        let pm = decode_image_to_pixmap_with_policy(&path, IngestPolicy::default()).unwrap();
        assert_eq!(pm.data, vec![128, 128, 128, 255]);
        let err = decode_image_to_pixmap_with_policy(&path, reject()).unwrap_err();
        assert!(err.to_string().contains("ICC"), "{err}");
    }

    #[cfg(feature = "tiff")]
    #[test]
    fn tiff_bilevel_fast_path_respects_reject() {
        use super::tiff_slice2::{TiffPage, write_tiff};
        use djvu_rs::png_io::decode_tiff_file_to_bitmaps;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("b.tif");
        let mut page = TiffPage::gray(8, 1, 1, 1, vec![0b1010_1010]);
        page.extra_tags = vec![(34675, 7, 4, u32::from_le_bytes(*b"acsp"))];
        write_tiff(&path, &[page]);
        assert!(
            decode_tiff_file_to_bitmaps(&path, IngestPolicy::default())
                .unwrap()
                .is_some()
        );
        let err = decode_tiff_file_to_bitmaps(&path, reject()).unwrap_err();
        assert!(err.to_string().contains("ICC"), "{err}");
    }

    #[test]
    fn cli_icc_reject_refuses_profiled_png() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("p.png");
        write_png_with_iccp(&input);
        let output = dir.path().join("p.djvu");
        let encode = |icc: &str| {
            Command::cargo_bin("djvu")
                .unwrap()
                .args([
                    "encode",
                    input.to_str().unwrap(),
                    "-o",
                    output.to_str().unwrap(),
                    "--icc",
                    icc,
                ])
                .assert()
        };
        encode("ignore").success();
        encode("reject")
            .failure()
            .stderr(predicates::str::contains("ICC colour profile"));
    }
}
