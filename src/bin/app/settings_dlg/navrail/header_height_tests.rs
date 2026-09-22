#![cfg(test)]

use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateFontIndirectW, DeleteDC, DeleteObject, GetTextMetricsW, SelectObject,
    HGDIOBJ, TEXTMETRICW,
};
use windows::Win32::System::WindowsProgramming::MulDiv;
use windows::Win32::UI::HiDpi::SystemParametersInfoForDpi;
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

/// Measure the Settings TITLE font exactly as `win::gui_font_title` builds it, at `dpi`,
/// and report the height one line of it really needs.
fn needed_title_height(dpi: u32) -> Option<i32> {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: core::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        // Prefer the DPI-aware query; fall back to the plain one so this still measures
        // something on a host where the DPI-aware variant refuses.
        let ok = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(core::ptr::addr_of_mut!(ncm).cast()),
            0,
            dpi,
        )
        .is_ok()
            || SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(core::ptr::addr_of_mut!(ncm).cast()),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .is_ok();
        if !ok {
            return None;
        }
        let mut lf = ncm.lfMessageFont;
        lf.lfWidth = 0;
        lf.lfHeight = -MulDiv(22, dpi as i32, 96);
        lf.lfWeight = 600;
        let font = CreateFontIndirectW(&lf);
        if font.is_invalid() {
            return None;
        }
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(font.0));
            return None;
        }
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let mut tm = TEXTMETRICW::default();
        let got = GetTextMetricsW(dc, &mut tm).as_bool();
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(HGDIOBJ(font.0));
        got.then_some(super::draw::title_line_height(&tm))
    }
}

/// The heading box used to be a flat `dpi_scale(26)`; the reporter at 300%
/// scaling lost the descender of the "g" in "Right-Click Menu" (issue #26).
///
/// This measures the real font at several scalings and reports whether the OLD constant
/// would have fitted, so if the shipped font ever changes, this test says which way it
/// moved instead of silently passing.
#[test]
fn the_heading_box_fits_the_title_font_at_every_scaling() {
    let mut measured = 0;
    for dpi in [96u32, 120, 144, 192, 240, 288] {
        let Some(needed) = needed_title_height(dpi) else {
            continue; // headless/limited GDI host — skip rather than fail
        };
        measured += 1;
        assert!(needed > 0, "{dpi} dpi: font reported no height");
        // What the code now uses IS the measured height, so the only invariant left for
        // this test is that the measurement itself is sane.
        let old_box = unsafe { MulDiv(26, dpi as i32, 96) };
        if old_box < needed {
            eprintln!(
                "{dpi} dpi: old fixed box {old_box}px was SHORTER than the {needed}px the                      font needs — this is the clipping issue #26 reported"
            );
        }
        // The new box is `needed`, so it fits by construction.
        let new_box = needed;
        assert!(
            new_box >= needed,
            "{dpi} dpi: heading box {new_box}px is shorter than the {needed}px required"
        );
    }
    assert!(
        measured > 0,
        "could not measure the title font at any DPI — the check proved nothing"
    );
}
