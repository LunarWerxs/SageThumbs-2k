#![cfg(test)]

use super::composite_rgba_over_bg;
use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP, HBITMAP};

/// Read the top-left BGRA quad out of a DIB-section HBITMAP, then free it.
unsafe fn first_px(hbmp: HBITMAP) -> [u8; 4] {
    let mut bm = BITMAP::default();
    let n = GetObjectW(
        hbmp.into(),
        core::mem::size_of::<BITMAP>() as i32,
        Some(&mut bm as *mut _ as *mut core::ffi::c_void),
    );
    assert!(n != 0 && !bm.bmBits.is_null());
    let px = core::slice::from_raw_parts(bm.bmBits as *const u8, 4);
    let out = [px[0], px[1], px[2], px[3]];
    let _ = DeleteObject(hbmp.into());
    out
}

/// The whole point of the `opaque` parameter: a caller that already knows the pixel is
/// opaque can force the fast swizzle path via `Some(true)` and it must be HONORED (not
/// silently re-derived by scanning alpha), even feeding it a genuinely translucent pixel.
/// Without this, `previewhandler.rs` had no way to share the EXE viewer's opacity-hint
/// optimization at all — this pins the parameter actually doing something.
#[test]
fn composite_rgba_over_bg_honors_a_forced_opaque_hint() {
    unsafe {
        // A 50%-alpha red pixel: under the real (computed) opacity it must blend with the
        // background; forced opaque, it must copy straight through instead and ignore bg.
        let translucent = [200u8, 0, 0, 128];
        let bg = 0x00FF_0000; // opaque blue, COLORREF 0x00BBGGRR

        let forced = composite_rgba_over_bg(1, 1, &translucent, bg, Some(true)).unwrap();
        assert_eq!(
            first_px(forced),
            [0, 0, 200, 255],
            "Some(true) must take the swizzle path even though the pixel is translucent"
        );

        let computed = composite_rgba_over_bg(1, 1, &translucent, bg, None).unwrap();
        assert_ne!(
            first_px(computed),
            [0, 0, 200, 255],
            "None must actually blend a translucent pixel with the background"
        );
    }
}

/// Untrusted decoded dimensions must be rejected (None), never deref/overflow — this runs
/// in prevhost on attacker-influenced sizes.
#[test]
fn composite_rgba_over_bg_rejects_bad_dims_without_crashing() {
    unsafe {
        assert!(composite_rgba_over_bg(0, 5, &[0u8; 64], 0, None).is_none());
        assert!(composite_rgba_over_bg(5, 0, &[0u8; 64], 0, None).is_none());
        assert!(composite_rgba_over_bg(-3, 4, &[0u8; 64], 0, None).is_none());
        assert!(composite_rgba_over_bg(2, 2, &[0u8; 4], 0, None).is_none());
        assert!(composite_rgba_over_bg(i32::MAX, i32::MAX, &[0u8; 4], 0, None).is_none());
    }
}

/// The alpha-over-background compositing math the preview pane's `WM_PAINT` later
/// StretchBlts (moved here from `previewhandler.rs` when its private `make_dib` copy was
/// retired). `bg` is a COLORREF 0x00BBGGRR; 0x00FF_0000 is opaque blue.
#[test]
fn composite_rgba_over_bg_composites_alpha_over_background() {
    unsafe {
        // Opaque red over blue copies straight through -> BGRA [0,0,255,255].
        let red = composite_rgba_over_bg(1, 1, &[255, 0, 0, 255], 0x00FF_0000, None).unwrap();
        assert_eq!(first_px(red), [0, 0, 255, 255], "opaque red");

        // 50% red over blue: R ≈ 200*128/255 ≈ 100, B ≈ 255*127/255 ≈ 127.
        let half = composite_rgba_over_bg(1, 1, &[200, 0, 0, 128], 0x00FF_0000, None).unwrap();
        let [b, g, r, a] = first_px(half);
        assert_eq!((g, a), (0, 255), "no green; DIB opaque");
        assert!((r as i32 - 100).abs() <= 2, "R composited ~100, got {r}");
        assert!((b as i32 - 127).abs() <= 2, "B composited ~127, got {b}");
    }
}
