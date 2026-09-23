//! EXE-only artwork/HBITMAP helpers for the companion app (Options/About/banner).
//!
//! These build premultiplied 32-bpp DIB-section `HBITMAP`s from app artwork and
//! remote-downloaded images for the Win32 EXEs (About box logo, Options banner,
//! ad/banner image). They live out of the crate root because the DLL never uses
//! them (LTO dead-strips them from the cdylib); the EXEs link them from the rlib
//! as `sagethumbs2k_core::app_image::*`. Pure relocation — see `st2k_base::dib` for the
//! shared DIB builder.

use core::ffi::c_void;

use windows::Win32::Graphics::Gdi::{DeleteObject, HBITMAP};

/// Reject attacker-influenced banner/cover art whose declared dimensions would
/// blow up the decode allocation, charged at the decoder's own bytes-per-pixel
/// (a 16-bit source is 2-8 B/px, not always 4). The upstream
/// byte cap (4 MiB) bounds the *compressed* size, but a tiny payload can still
/// declare an enormous canvas, so probe dimensions before decoding. Reuses the
/// decode pipeline's single bomb-guard ceilings (`decode::limits`) so all paths
/// share one budget.
const REMOTE_ART_MAX_DIM: u32 = crate::decode::limits::MAX_DIM;
const REMOTE_ART_MAX_ALLOC: u64 = crate::decode::limits::MAX_ALLOC;

/// Cheaply read an image's declared dimensions without decoding pixels, and
/// reject anything past the bomb-guard limits. `Some(())` means "safe to decode".
fn remote_art_dims_ok(bytes: &[u8]) -> Option<()> {
    use image::ImageDecoder;
    use std::io::Cursor;
    // Build the decoder from the header (no pixels) so the allocation is charged at the
    // bytes-per-pixel the decoder will ACTUALLY materialize: `load_from_memory` does not
    // set `image::Limits`, so a 16-bit source yields an 8 B/px buffer, not 4.
    let decoder = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_decoder()
        .ok()?;
    let (w, h) = decoder.dimensions();
    let bpp = u64::from(decoder.color_type().bytes_per_pixel());
    if w > REMOTE_ART_MAX_DIM
        || h > REMOTE_ART_MAX_DIM
        || (w as u64 * h as u64 * bpp) > REMOTE_ART_MAX_ALLOC
    {
        return None;
    }
    Some(())
}

/// RAII wrapper around a raw GDI `HBITMAP` handle: dropping it frees the handle
/// with `DeleteObject`. Call [`into_raw`] to surrender ownership at the point of
/// building a successful return value.
///
/// [`into_raw`]: OwnedHbitmap::into_raw
pub struct OwnedHbitmap(isize);

impl OwnedHbitmap {
    /// Surrender ownership, returning the raw handle. The caller (or the shell)
    /// is now responsible for `DeleteObject`; `Drop` will NOT run.
    pub fn into_raw(self) -> isize {
        let raw = self.0;
        core::mem::forget(self);
        raw
    }
}

impl Drop for OwnedHbitmap {
    fn drop(&mut self) {
        if self.0 != 0 {
            unsafe {
                let _ = DeleteObject(HBITMAP(self.0 as *mut c_void).into());
            }
        }
    }
}

/// Raw straight-RGBA pixels (top row first) → premultiplied 32-bpp DIB-section
/// HBITMAP handle. For app artwork the caller composites itself (e.g. the About
/// box's light-mode logo chip). None on failure or size mismatch.
pub fn rgba_to_hbitmap(w: u32, h: u32, rgba: &[u8]) -> Option<isize> {
    if w == 0 || h == 0 || rgba.len() != (w as usize) * (h as usize) * 4 {
        return None;
    }
    let hbmp =
        unsafe { st2k_base::dib::create_premultiplied_dib(w as i32, h as i32, rgba) }.ok()?;
    Some(hbmp.0 as isize)
}

/// Decode image bytes (via the `image` crate — PNG/JPEG/GIF/…, not the full
/// thumbnail pipeline) resized to exactly `w`x`h`, into a premultiplied 32-bpp
/// DIB-section HBITMAP returned as a raw handle. For the fixed-size logo / banner
/// controls; also decodes a remote-downloaded image. The first frame is used for
/// animated formats (GIF). None on failure or a bomb-guard rejection.
pub fn image_to_hbitmap_sized(bytes: &[u8], w: u32, h: u32) -> Option<isize> {
    if w == 0 || h == 0 {
        return None;
    }
    // Bomb guard: probe the source canvas before `load_from_memory` allocates it.
    remote_art_dims_ok(bytes)?;
    let img = image::load_from_memory(bytes).ok()?;
    let rgba = img
        .resize_exact(w, h, image::imageops::FilterType::Lanczos3)
        .to_rgba8();
    let hbmp =
        unsafe { st2k_base::dib::create_premultiplied_dib(w as i32, h as i32, rgba.as_raw()) }
            .ok()?;
    Some(OwnedHbitmap(hbmp.0 as isize).into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A BMP file header declaring `w` x `h` and nothing else: `into_decoder` reads the
    /// 54-byte header and never touches pixel data, which is exactly the attack this guard
    /// exists for - a tiny payload declaring an enormous canvas.
    fn bmp_header(w: i32, h: i32) -> Vec<u8> {
        let mut v = b"BM".to_vec();
        v.extend_from_slice(&54u32.to_le_bytes()); // file size
        v.extend_from_slice(&0u32.to_le_bytes()); // reserved
        v.extend_from_slice(&54u32.to_le_bytes()); // pixel-data offset
        v.extend_from_slice(&40u32.to_le_bytes()); // BITMAPINFOHEADER
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&1u16.to_le_bytes()); // planes
        v.extend_from_slice(&24u16.to_le_bytes()); // bpp
        v.extend_from_slice(&[0u8; 24]); // compression .. clrImportant
        v
    }

    #[test]
    fn an_ordinary_canvas_passes_the_bomb_guard() {
        assert!(remote_art_dims_ok(&bmp_header(100, 100)).is_some());
        assert!(remote_art_dims_ok(&bmp_header(1, 1)).is_some());
    }

    #[test]
    fn a_canvas_past_the_dimension_cap_is_refused_in_either_axis() {
        let over = REMOTE_ART_MAX_DIM as i32 + 1;
        assert!(remote_art_dims_ok(&bmp_header(over, 10)).is_none(), "width");
        assert!(
            remote_art_dims_ok(&bmp_header(10, over)).is_none(),
            "height"
        );
    }

    #[test]
    fn the_allocation_cap_bites_before_the_dimension_cap_does() {
        // Both dimensions are legal; it is the allocation product that is not. This header
        // declares 24 bpp, so the decoder's native 3 bytes per pixel is what is charged. The
        // boundary is exact: `MAX_ALLOC` bytes is allowed, one pixel row more is not.
        let w = REMOTE_ART_MAX_DIM as i32;
        let h = (REMOTE_ART_MAX_ALLOC / (REMOTE_ART_MAX_DIM as u64 * 3)) as i32;
        assert!(
            remote_art_dims_ok(&bmp_header(w, h)).is_some(),
            "exactly at the cap"
        );
        assert!(
            remote_art_dims_ok(&bmp_header(w, h + 1)).is_none(),
            "one row past it"
        );
    }

    #[test]
    fn bytes_that_are_not_an_image_are_refused_rather_than_guessed_at() {
        assert!(remote_art_dims_ok(b"").is_none());
        assert!(remote_art_dims_ok(b"<!DOCTYPE html><html>not art</html>").is_none());
        assert!(
            remote_art_dims_ok(&bmp_header(100, 100)[..20]).is_none(),
            "truncated header"
        );
    }
}
