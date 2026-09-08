//! Image file → [`Pixmap`] decoders.
//!
//! Provides:
//! - [`decode_png_to_pixmap`] — decode any 8-bit PNG into the RGBA [`Pixmap`]
//!   format used throughout djvu-rs.
//! - [`decode_jpeg_file_to_pixmap`] — decode a JPEG file into [`Pixmap`].
//! - [`decode_image_to_pixmap`] — unified dispatcher: routes by file extension
//!   (`png`, `jpg`/`jpeg`, `tif`/`tiff`) and falls back to magic-byte sniffing
//!   for extension-less or ambiguous paths. TIFF support is gated by
//!   `#[cfg(feature = "tiff")]`.

use std::path::Path;

use crate::Pixmap;

/// Decode an 8-bit PNG file at `path` into a [`Pixmap`].
///
/// Supports RGBA, RGB, GrayscaleAlpha, and Grayscale color types.
/// Returns an error for indexed-color PNGs and for bit depths other than 8.
pub fn decode_png_to_pixmap(path: &Path) -> Result<Pixmap, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info()?;
    let info = reader.info();
    let width = info.width;
    let height = info.height;
    let color = info.color_type;
    let depth = info.bit_depth;
    let mut buf = vec![0u8; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf)?;
    buf.truncate(frame.buffer_size());

    if depth != png::BitDepth::Eight {
        return Err(format!(
            "{}: unsupported PNG bit depth {:?} (only 8-bit channels supported)",
            path.display(),
            depth
        )
        .into());
    }

    let mut data = Vec::with_capacity((width as usize) * (height as usize) * 4);
    match color {
        png::ColorType::Rgba => data.extend_from_slice(&buf),
        png::ColorType::Rgb => {
            for chunk in buf.chunks_exact(3) {
                data.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
        }
        png::ColorType::GrayscaleAlpha => {
            for chunk in buf.chunks_exact(2) {
                let g = chunk[0];
                data.extend_from_slice(&[g, g, g, chunk[1]]);
            }
        }
        png::ColorType::Grayscale => {
            for &g in &buf {
                data.extend_from_slice(&[g, g, g, 255]);
            }
        }
        png::ColorType::Indexed => {
            return Err(format!("{}: indexed PNG not supported", path.display()).into());
        }
    }

    Ok(Pixmap {
        width,
        height,
        data,
    })
}

/// Decode a JPEG file at `path` into a [`Pixmap`] (RGBA, alpha = 255).
///
/// Uses the `zune-jpeg` crate (already pulled in by the `std` feature).
pub fn decode_jpeg_file_to_pixmap(path: &Path) -> Result<Pixmap, Box<dyn std::error::Error>> {
    use zune_jpeg::JpegDecoder;
    use zune_jpeg::zune_core::bytestream::ZCursor;

    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let cursor = ZCursor::new(&data);
    let mut decoder = JpegDecoder::new(cursor);
    decoder
        .decode_headers()
        .map_err(|e| format!("{}: JPEG header error: {e:?}", path.display()))?;
    let info = decoder
        .info()
        .ok_or_else(|| format!("{}: missing JPEG image info", path.display()))?;
    let w = info.width as usize;
    let h = info.height as usize;
    let rgb = decoder
        .decode()
        .map_err(|e| format!("{}: JPEG decode error: {e:?}", path.display()))?;
    let pixel_count = w * h;
    // zune-jpeg returns packed RGB; convert to RGBA with alpha = 255.
    let rgb = if rgb.len() >= pixel_count * 3 {
        rgb
    } else {
        let mut padded = rgb;
        padded.resize(pixel_count * 3, 0);
        padded
    };
    let mut data = vec![0u8; pixel_count * 4];
    for (i, chunk) in rgb[..pixel_count * 3].chunks_exact(3).enumerate() {
        data[i * 4] = chunk[0];
        data[i * 4 + 1] = chunk[1];
        data[i * 4 + 2] = chunk[2];
        data[i * 4 + 3] = 255;
    }
    Ok(Pixmap {
        width: w as u32,
        height: h as u32,
        data,
    })
}

/// Decode a TIFF file at `path` into a [`Pixmap`] (RGBA, alpha = 255).
///
/// Requires the `tiff` feature.
#[cfg(feature = "tiff")]
pub fn decode_tiff_file_to_pixmap(path: &Path) -> Result<Pixmap, Box<dyn std::error::Error>> {
    use tiff::ColorType;
    use tiff::decoder::{Decoder, DecodingResult};

    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut decoder = Decoder::new(std::io::BufReader::new(file))
        .map_err(|e| format!("{}: TIFF open error: {e}", path.display()))?;
    let (w, h) = decoder
        .dimensions()
        .map_err(|e| format!("{}: TIFF dimensions error: {e}", path.display()))?;
    let color = decoder
        .colortype()
        .map_err(|e| format!("{}: TIFF colortype error: {e}", path.display()))?;
    let result = decoder
        .read_image()
        .map_err(|e| format!("{}: TIFF decode error: {e}", path.display()))?;
    let DecodingResult::U8(pixels) = result else {
        return Err(format!(
            "{}: unsupported TIFF sample depth (only 8-bit channels supported)",
            path.display()
        )
        .into());
    };
    let pixel_count = w as usize * h as usize;
    let mut data = Vec::with_capacity(pixel_count * 4);
    match color {
        ColorType::RGB(8) => {
            for chunk in pixels.chunks_exact(3) {
                data.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
        }
        ColorType::RGBA(8) => {
            data.extend_from_slice(&pixels);
        }
        ColorType::Gray(8) => {
            for &g in &pixels {
                data.extend_from_slice(&[g, g, g, 255]);
            }
        }
        ColorType::GrayA(8) => {
            for chunk in pixels.chunks_exact(2) {
                let g = chunk[0];
                data.extend_from_slice(&[g, g, g, chunk[1]]);
            }
        }
        other => {
            return Err(format!(
                "{}: unsupported TIFF color type {other:?} (supported: RGB8, RGBA8, Gray8, GrayA8)",
                path.display()
            )
            .into());
        }
    }
    Ok(Pixmap {
        width: w,
        height: h,
        data,
    })
}

/// Unified image decoder: dispatch by extension, fall back to magic bytes.
///
/// Supported extensions: `png`, `jpg`, `jpeg`, `tif`, `tiff` (TIFF requires the
/// `tiff` feature — returns a clear error when the feature is off).
///
/// Magic-byte fallback is used when the extension is absent or unrecognised:
/// - `\x89PNG` → PNG
/// - `\xFF\xD8` → JPEG
/// - `II\x2A` / `MM\x00\x2A` → TIFF (feature-gated)
pub fn decode_image_to_pixmap(path: &Path) -> Result<Pixmap, Box<dyn std::error::Error>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("png") => return decode_png_to_pixmap(path),
        Some("jpg") | Some("jpeg") => return decode_jpeg_file_to_pixmap(path),
        Some("tif") | Some("tiff") => {
            #[cfg(feature = "tiff")]
            return decode_tiff_file_to_pixmap(path);
            #[cfg(not(feature = "tiff"))]
            return Err(format!(
                "{}: TIFF input requires the 'tiff' feature \
                 (recompile with `--features tiff`)",
                path.display()
            )
            .into());
        }
        _ => {}
    }

    // Magic-byte sniffing for extension-less / ambiguous paths.
    let header = {
        use std::io::Read;
        let mut f = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut buf = [0u8; 4];
        f.read_exact(&mut buf)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        buf
    };

    if header.starts_with(b"\x89PNG") {
        return decode_png_to_pixmap(path);
    }
    if header.starts_with(b"\xFF\xD8") {
        return decode_jpeg_file_to_pixmap(path);
    }
    if header.starts_with(b"II\x2A\x00") || header.starts_with(b"MM\x00\x2A") {
        #[cfg(feature = "tiff")]
        return decode_tiff_file_to_pixmap(path);
        #[cfg(not(feature = "tiff"))]
        return Err(format!(
            "{}: TIFF input requires the 'tiff' feature \
             (recompile with `--features tiff`)",
            path.display()
        )
        .into());
    }

    Err(format!(
        "{}: unrecognised image format (expected PNG, JPEG, or TIFF)",
        path.display()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Encode raw pixel bytes into a PNG file and return the path.
    fn write_png(
        dir: &tempfile::TempDir,
        name: &str,
        width: u32,
        height: u32,
        color: png::ColorType,
        pixels: &[u8],
    ) -> std::path::PathBuf {
        let path = dir.path().join(name);
        let file = std::fs::File::create(&path).unwrap();
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(color);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
        path
    }

    #[test]
    fn rgb_adds_alpha_255() {
        let dir = tempfile::tempdir().unwrap();
        // 1×1 red pixel in RGB
        let path = write_png(&dir, "rgb.png", 1, 1, png::ColorType::Rgb, &[255, 0, 0]);
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.width, 1);
        assert_eq!(pm.height, 1);
        assert_eq!(pm.data, vec![255, 0, 0, 255]);
    }

    #[test]
    fn rgba_passthrough() {
        let dir = tempfile::tempdir().unwrap();
        // 1×1 semi-transparent blue pixel
        let path = write_png(
            &dir,
            "rgba.png",
            1,
            1,
            png::ColorType::Rgba,
            &[0, 0, 255, 128],
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![0, 0, 255, 128]);
    }

    #[test]
    fn grayscale_expands_to_rgba() {
        let dir = tempfile::tempdir().unwrap();
        // 1×1 gray=200
        let path = write_png(&dir, "gray.png", 1, 1, png::ColorType::Grayscale, &[200]);
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![200, 200, 200, 255]);
    }

    #[test]
    fn grayscale_alpha_expands_to_rgba() {
        let dir = tempfile::tempdir().unwrap();
        // 1×1 gray=100, alpha=50
        let path = write_png(
            &dir,
            "graya.png",
            1,
            1,
            png::ColorType::GrayscaleAlpha,
            &[100, 50],
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.data, vec![100, 100, 100, 50]);
    }

    #[test]
    fn dimensions_preserved() {
        let dir = tempfile::tempdir().unwrap();
        // 3×2 RGB image
        let pixels = vec![0u8; 3 * 2 * 3];
        let path = write_png(&dir, "dim.png", 3, 2, png::ColorType::Rgb, &pixels);
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(pm.width, 3);
        assert_eq!(pm.height, 2);
        assert_eq!(pm.data.len(), 3 * 2 * 4);
    }

    #[test]
    fn nonexistent_file_returns_error() {
        let result = decode_png_to_pixmap(std::path::Path::new("/nonexistent/file.png"));
        assert!(result.is_err());
    }

    #[test]
    fn multi_pixel_rgb_row_order() {
        let dir = tempfile::tempdir().unwrap();
        // 2×1: red then green
        let path = write_png(
            &dir,
            "two.png",
            2,
            1,
            png::ColorType::Rgb,
            &[255, 0, 0, 0, 255, 0],
        );
        let pm = decode_png_to_pixmap(&path).unwrap();
        assert_eq!(&pm.data[0..4], &[255, 0, 0, 255]); // red pixel
        assert_eq!(&pm.data[4..8], &[0, 255, 0, 255]); // green pixel
    }

    #[test]
    fn sixteen_bit_depth_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 1, 1);
            encoder.set_color(png::ColorType::Rgb);
            encoder.set_depth(png::BitDepth::Sixteen);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0u8; 6]).unwrap(); // 1×1 RGB 16-bit = 6 bytes
        }
        let result = decode_png_to_pixmap(&path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("unsupported") || msg.contains("bit depth") || msg.contains("Sixteen"),
            "msg={msg}"
        );
    }

    #[test]
    fn indexed_color_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("indexed.png");
        {
            let file = std::fs::File::create(&path).unwrap();
            let mut encoder = png::Encoder::new(file, 1, 1);
            encoder.set_color(png::ColorType::Indexed);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_palette(vec![0, 0, 0]); // one-entry palette (black)
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[0u8]).unwrap(); // index 0
        }
        let result = decode_png_to_pixmap(&path);
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(
            msg.contains("indexed") || msg.contains("Indexed"),
            "msg={msg}"
        );
    }

    #[test]
    fn write_error_message_contains_path() {
        // write_png error path uses path.display() in the message
        let path = std::path::Path::new("/no/such/dir/x.png");
        let err = decode_png_to_pixmap(path).unwrap_err();
        assert!(err.to_string().contains("x.png"));
    }
}
