//! CorelDRAW `.cdr` / `.cdt` and Corel Presentation Exchange `.cmx`.
//!
//! These are RIFF files (`RIFF????CDR?` / `CDT?` / `CMX?`) that embed a Windows
//! **DIB preview** in a `DISP` chunk (a clipboard `CF_DIB`: a `u32` format tag
//! then a packed `BITMAPINFOHEADER` + palette + pixels). Neither the `image` crate
//! nor our bundled ImageMagick reads CorelDRAW, so we pull the DISP DIB, wrap it in
//! a 14-byte BMP file header, and let the image tier decode it — the same
//! embedded-preview approach as PSD/PSP/C4D. Verified on real CorelDRAW `CDRB`
//! files (128×128, 8-bit palettized previews).
//!
//! Bounds-checked throughout (runs under `panic = "abort"`): malformed input yields
//! `None` and the shell shows the default icon.

use super::util::dib_to_bmp;

const DISP: &[u8] = b"DISP";

/// A RIFF CorelDRAW drawing / template / presentation-exchange file.
pub fn looks_like_cdr(b: &[u8]) -> bool {
    b.len() >= 12 && &b[0..4] == b"RIFF" && matches!(&b[8..11], b"CDR" | b"CDT" | b"CMX")
}

/// Extract the embedded DISP preview as a BMP (re-decoded by the image tier), or `None`.
pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    let disp = find_disp_chunk(bytes)?;
    // DISP data = a `u32` clipboard format (`CF_DIB` = 8) then the packed DIB. Try
    // the DIB right after that prefix; fall back to treating DISP as a bare DIB.
    dib_to_bmp(disp.get(4..).unwrap_or(disp)).or_else(|| dib_to_bmp(disp))
}

/// Walk the top-level RIFF chunks and return the `DISP` chunk's data. `LIST`
/// chunks are skipped whole (the preview is a top-level chunk). Little-endian
/// sizes; chunks are word-aligned.
fn find_disp_chunk(b: &[u8]) -> Option<&[u8]> {
    let mut p = 12usize; // past "RIFF" + size + form
    while p + 8 <= b.len() {
        let id = &b[p..p + 4];
        let size = u32::from_le_bytes(b[p + 4..p + 8].try_into().ok()?) as usize;
        let data_start = p + 8;
        if id == DISP {
            // Use whatever is present even if the declared size overruns the file.
            let end = data_start.checked_add(size).unwrap_or(b.len()).min(b.len());
            return b.get(data_start..end);
        }
        let next = data_start.checked_add(size)?;
        if next > b.len() {
            break;
        }
        p = next + (size & 1); // pad to even
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 2×2 8-bit DIB (4-colour palette) inside a RIFF `CDRB`, in a `DISP` chunk
    /// with the `CF_DIB` (8) clipboard prefix — preceded by a decoy `vrsn` chunk.
    fn fake_cdr() -> Vec<u8> {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes()); // biSize
        dib.extend_from_slice(&2i32.to_le_bytes()); // width
        dib.extend_from_slice(&2i32.to_le_bytes()); // height
        dib.extend_from_slice(&1u16.to_le_bytes()); // planes
        dib.extend_from_slice(&8u16.to_le_bytes()); // bitcount
        dib.extend_from_slice(&0u32.to_le_bytes()); // compression BI_RGB
        dib.extend_from_slice(&0u32.to_le_bytes()); // sizeImage
        dib.extend_from_slice(&0i32.to_le_bytes()); // xppm
        dib.extend_from_slice(&0i32.to_le_bytes()); // yppm
        dib.extend_from_slice(&4u32.to_le_bytes()); // clrUsed = 4
        dib.extend_from_slice(&0u32.to_le_bytes()); // clrImportant
        // BGRA palette (RGBQUAD: blue, green, red, reserved). Entry 1 = red, 2 = green,
        // 3 = blue — so the red/green channels must be in the 3rd/2nd byte, not the 1st.
        for c in [[0u8, 0, 0, 0], [0, 0, 255, 0], [0, 255, 0, 0], [255, 0, 0, 0]] {
            dib.extend_from_slice(&c); // black, red, green, blue
        }
        // 2×2 rows, padded to 4 bytes each: pixels {1,2 / 3,0}
        dib.extend_from_slice(&[3, 0, 0, 0]);
        dib.extend_from_slice(&[1, 2, 0, 0]);

        let mut disp = vec![8u8, 0, 0, 0]; // CF_DIB
        disp.extend_from_slice(&dib);

        let mut riff_body = Vec::new();
        riff_body.extend_from_slice(b"CDRB");
        // decoy vrsn chunk
        riff_body.extend_from_slice(b"vrsn");
        riff_body.extend_from_slice(&2u32.to_le_bytes());
        riff_body.extend_from_slice(&[0, 0]);
        // DISP
        riff_body.extend_from_slice(b"DISP");
        riff_body.extend_from_slice(&(disp.len() as u32).to_le_bytes());
        riff_body.extend_from_slice(&disp);

        let mut f = Vec::new();
        f.extend_from_slice(b"RIFF");
        f.extend_from_slice(&(riff_body.len() as u32).to_le_bytes());
        f.extend_from_slice(&riff_body);
        f
    }

    #[test]
    fn extracts_disp_preview() {
        let cdr = fake_cdr();
        assert!(looks_like_cdr(&cdr));
        let bmp = extract(&cdr).expect("DISP preview");
        let img = image::load_from_memory(&bmp).expect("valid BMP").to_rgba8();
        assert_eq!((img.width(), img.height()), (2, 2));
        // BMP rows are bottom-up: top row in the image is the LAST DIB row {1,2} = red,green.
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255], "top-left = red (palette 1)");
        assert_eq!(img.get_pixel(1, 0).0, [0, 255, 0, 255], "top-right = green (palette 2)");
    }

    #[test]
    fn rejects_non_cdr() {
        assert!(!looks_like_cdr(b"RIFF\0\0\0\0WAVEfmt ")); // a WAV, not CDR
        assert!(!looks_like_cdr(b"PK\x03\x04 zip!!!!"));
        assert!(extract(b"not a cdr").is_none());
    }
}
