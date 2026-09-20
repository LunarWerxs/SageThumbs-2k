//! Build the 32bpp top-down premultiplied-BGRA DIB section the shell wants.
//!
//! `IThumbnailProvider::GetThumbnail` requires the returned `HBITMAP` to be a
//! DIB *section* (CreateDIBSection), 32bpp. Memory order is B,G,R,A; for
//! `WTSAT_ARGB` the color channels must be premultiplied by alpha. A negative
//! `biHeight` makes the bitmap top-down (positive would render upside-down).

use windows::core::{Error, Result};
use windows::Win32::Foundation::E_FAIL;
use windows::Win32::Graphics::Gdi::HBITMAP;

/// `rgba` is straight (non-premultiplied) RGBA8, row-major, top row first,
/// `width * height * 4` bytes. Returns an owned `HBITMAP`; on the success path
/// the shell takes ownership and `DeleteObject`s it.
///
/// # Safety
///
/// Must be called on a thread where GDI calls are legal, and the returned `HBITMAP` is an
/// OWNED handle: the caller either hands it to the shell (which deletes it) or `DeleteObject`s
/// it itself, exactly once. Every bounds check this needs is inside - a `rgba` shorter than
/// `width * height * 4` is rejected rather than read past - so the unsafety is the GDI handle
/// contract, not the slice.
pub unsafe fn create_premultiplied_dib(width: i32, height: i32, rgba: &[u8]) -> Result<HBITMAP> {
    if width <= 0 || height <= 0 {
        return Err(Error::from(E_FAIL));
    }
    // Checked, mirroring `previewhandler::make_dib`'s guard: `width`/`height` are i32 and
    // this crate only ever targets x64/arm64 (64-bit `usize`), so `px * 4` cannot actually
    // overflow for any positive i32 pair today — but the twin function next to this one
    // already pays for the check, and letting the two drift apart is how a future decoder
    // change (or a target retarget) would silently reintroduce the gap here first.
    let px = (width as usize)
        .checked_mul(height as usize)
        .ok_or(Error::from(E_FAIL))?;
    let total_bytes = px.checked_mul(4).ok_or(Error::from(E_FAIL))?;
    if rgba.len() < total_bytes {
        return Err(Error::from(E_FAIL));
    }

    let (hbmp, bits) = crate::safety::create_dib_section(width, height)?;

    let dst = core::slice::from_raw_parts_mut(bits as *mut u8, total_bytes);
    for i in 0..px {
        let r = rgba[i * 4];
        let g = rgba[i * 4 + 1];
        let b = rgba[i * 4 + 2];
        let a = rgba[i * 4 + 3];
        // Hoist the two common cases (bit-exact with the divide below): a fully
        // opaque pixel premultiplies to itself, a fully transparent one to zero —
        // skipping three integer divides for the bulk of a typical thumbnail.
        if a == 255 {
            dst[i * 4] = b;
            dst[i * 4 + 1] = g;
            dst[i * 4 + 2] = r;
            dst[i * 4 + 3] = 255;
        } else if a == 0 {
            dst[i * 4] = 0;
            dst[i * 4 + 1] = 0;
            dst[i * 4 + 2] = 0;
            dst[i * 4 + 3] = 0;
        } else {
            let m = |c: u8| (((c as u16) * (a as u16) + 127) / 255) as u8;
            dst[i * 4] = m(b);
            dst[i * 4 + 1] = m(g);
            dst[i * 4 + 2] = m(r);
            dst[i * 4 + 3] = a;
        }
    }

    Ok(hbmp)
}

/// Swap the red and blue channels of a 32bpp raster and force every pixel opaque, copying
/// `src` into `dst`.
///
/// GDI hands a DIB section's bits back as `B,G,R,A` and every `image::RgbaImage` in this repo
/// wants `R,G,B,A`, so BGRA -> RGBA and RGBA -> BGRA are the SAME operation - swap channels 0
/// and 2 - which is why one function serves both directions. Four call sites had pasted the
/// identical four-line loop: `safety::create_dib_from_rgba`'s fully-opaque fast path,
/// `contextmenu::paint`'s menu-preview capture, the preview font specimen's read-back, and the
/// `previewhandler_shot` example.
///
/// ⚠ The `_opaque` in the name is load-bearing: this writes 255 into every alpha byte and
/// discards whatever was there. Anything that must honour transparency premultiplies instead
/// (`create_premultiplied_dib` above, `preview::content::dib`) and must NOT call this.
///
/// Converts `min(src.len(), dst.len()) / 4` pixels and returns that count, so a caller whose
/// two buffers disagree is truncated rather than panicking.
pub fn swap_rb_opaque(src: &[u8], dst: &mut [u8]) -> usize {
    let px = src.len().min(dst.len()) / 4;
    for i in 0..px {
        dst[i * 4] = src[i * 4 + 2];
        dst[i * 4 + 1] = src[i * 4 + 1];
        dst[i * 4 + 2] = src[i * 4];
        dst[i * 4 + 3] = 255;
    }
    px
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn swap_rb_opaque_swaps_red_and_blue_and_forces_alpha() {
        let src = [1u8, 2, 3, 0, 10, 20, 30, 128];
        let mut dst = [0u8; 8];
        assert_eq!(swap_rb_opaque(&src, &mut dst), 2);
        assert_eq!(dst, [3, 2, 1, 255, 30, 20, 10, 255]);
        // Its own inverse: a second pass restores the colour channels.
        let mut back = [0u8; 8];
        assert_eq!(swap_rb_opaque(&dst, &mut back), 2);
        assert_eq!(&back[..3], &src[..3]);
    }

    #[test]
    fn swap_rb_opaque_truncates_to_the_shorter_buffer() {
        let src = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let mut dst = [9u8; 4];
        assert_eq!(swap_rb_opaque(&src, &mut dst), 1);
        assert_eq!(dst, [3, 2, 1, 255]);
        let mut big = [9u8; 12];
        assert_eq!(swap_rb_opaque(&dst, &mut big), 1);
        assert_eq!(&big[4..], &[9u8; 8]);
    }

    /// Mirrors `previewhandler::make_dib`'s own overflow-guard test: huge dims must be
    /// rejected via checked arithmetic, not silently wrap. On this crate's x64/arm64-only
    /// target `px * 4` cannot actually overflow for any positive `i32` pair (`i32::MAX`
    /// squared times 4 stays under `u64::MAX`), so the length check already caught this
    /// input before the `checked_mul` above existed - it is defense-in-depth, not a
    /// behavior change. This test locks in the shared contract regardless.
    #[test]
    fn create_premultiplied_dib_rejects_bad_dims_without_panicking() {
        unsafe {
            assert!(create_premultiplied_dib(i32::MAX, i32::MAX, &[0u8; 4]).is_err());
            assert!(create_premultiplied_dib(0, 5, &[0u8; 4]).is_err());
            assert!(create_premultiplied_dib(5, 0, &[0u8; 4]).is_err());
        }
    }
}
