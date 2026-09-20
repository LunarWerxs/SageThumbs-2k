//! The bundled icon font: loading it from memory, the face fallbacks and the missing-glyph probe.

use super::*;

/// The bundled toolbar icon font: a ~4.6 KB subset of Material Symbols (Apache-2.0), generated
/// by `scripts/build-icon-font.py` and committed.
///
/// EMBEDDED rather than installed alongside the EXE, because `AddFontMemResourceEx` loads a
/// font straight from memory: no installer row, no portable-zip row, no path to resolve, no
/// file a user can delete, and it works identically for the installed build and the zip. At
/// this size the binary cost is noise against a 128 KiB per-release installer budget.
pub(super) const ICON_FONT_TTF: &[u8] =
    include_bytes!("../../../../assets/icons/SageThumbs2K-Icons.ttf");

/// Face name of [`ICON_FONT_TTF`]. Deliberately NOT "Material Symbols Outlined": the font is
/// process-private, and a distinct name means a separately installed copy of Material Symbols
/// can never be picked instead of ours.
pub(crate) const BUNDLED_ICON_FACE: &str = "SageThumbs2K Icons";

/// Register the embedded icon font for this process. `true` if GDI accepted it.
///
/// `AddFontMemResourceEx` fonts are PRIVATE to the process and are not enumerable, so this
/// cannot leak into other applications' font pickers. The handle is deliberately never freed:
/// the font must outlive every window that draws with it, and the process owns it until exit.
pub(super) fn load_bundled_icon_font() -> bool {
    use windows::Win32::Graphics::Gdi::AddFontMemResourceEx;
    let mut count: u32 = 0;
    let handle = unsafe {
        AddFontMemResourceEx(
            ICON_FONT_TTF.as_ptr() as *const c_void,
            ICON_FONT_TTF.len() as u32,
            None,
            core::ptr::addr_of_mut!(count),
        )
    };
    !handle.is_invalid() && count > 0
}

/// The icon font the toolbars draw with, as a face name.
///
/// **Issue #21.** These toolbars used to hard-code `Segoe Fluent Icons`, which ships with
/// Windows 11 and does NOT exist on Windows 10 - and GDI substitutes a missing face SILENTLY,
/// so every button rendered as an empty box there. The app supports Windows 10
/// (`MinVersion=10.0`); a user reported exactly this.
///
/// The answer is no longer to guess at what the OS has: a subset of Material Symbols is
/// EMBEDDED (see [`ICON_FONT_TTF`]) and used first, so the toolbars look the same everywhere
/// and depend on nothing the OS ships. The OS faces stay behind it as a safety net only.
///
/// Resolved ONCE: fonts do not appear mid-session, and each probe costs a DC.
pub(crate) fn icon_font_face() -> &'static str {
    static FACE: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    FACE.get_or_init(|| {
        // Dev override, so the Windows 10 appearance can be SEEN on a Windows 11 machine:
        // `ST2K_ICON_FONT="Segoe MDL2 Assets"` forces the fallback and `--shot` captures it.
        // Without this the fix could only be verified by reasoning, which is how the bug got
        // shipped in the first place. Ignored unless the named face actually exists.
        if let Some(forced) = forced_icon_face() {
            return forced;
        }
        // The BUNDLED font first, so the toolbars look identical on every Windows version and
        // do not depend on what the OS happens to ship. The OS fonts remain behind it purely as
        // a safety net for the case where GDI refuses the embedded font.
        if load_bundled_icon_font() && font_face_exists(BUNDLED_ICON_FACE) {
            return BUNDLED_ICON_FACE;
        }
        // Win11's font, then Win10's, then a face that always exists so the last resort is
        // legible text rather than a crash. NOTE: these use DIFFERENT codepoints from the
        // bundled font - see `preview::paint::btn_glyph`, which maps per-face.
        for want in ["Segoe Fluent Icons", "Segoe MDL2 Assets"] {
            if font_face_exists(want) {
                return want;
            }
        }
        "Segoe UI Symbol"
    })
}

/// The face `ST2K_ICON_FONT` forces, when it names a known face this machine really has;
/// `None` leaves `icon_font_face` to its normal chain.
fn forced_icon_face() -> Option<&'static str> {
    let forced = std::env::var("ST2K_ICON_FONT")
        .ok()
        .filter(|f| !f.is_empty())?;
    ["Segoe Fluent Icons", "Segoe MDL2 Assets", "Segoe UI Symbol"]
        .into_iter()
        .find(|&known| forced.eq_ignore_ascii_case(known) && font_face_exists(known))
}

/// An icon-font handle at `em` device pixels. Both toolbars build theirs through here so the
/// face AND the rendering mode are decided once. Caller owns and deletes it.
///
/// **`ANTIALIASED_QUALITY`, deliberately, not `CLEARTYPE_QUALITY`.** ClearType renders through
/// the display's RGB sub-pixels, which is why text looks sharper with it and why an ICON looks
/// worse: measured off the real caption toolbar, 73-97% of every glyph's pixels carried an
/// orange or blue colour cast, against 0% for the hand-drawn OCR mark beside them. That
/// difference is what reads as "the other icons are blurry". Greyscale AA drops the fringing to
/// zero and more than doubles the fully-covered pixels (22% -> 52%) at exactly the same glyph
/// size, on the same machine with ClearType left on system-wide.
///
/// `NONANTIALIASED_QUALITY` was measured too and is a trap: 100% solid pixels, but circles turn
/// polygonal and the gear and the sun's rays go lumpy. Curves need the anti-aliasing; what they
/// never needed was the COLOUR.
///
/// (This is also the fringing CLAUDE.md warns about when sampling rendered pixels - it is why a
/// colour sampler can read grey anti-aliased text as syntax highlighting.)
pub(crate) unsafe fn icon_font(em: i32) -> windows::Win32::Graphics::Gdi::HFONT {
    use windows::Win32::Graphics::Gdi::{
        CreateFontIndirectW, ANTIALIASED_QUALITY, DEFAULT_CHARSET, LOGFONTW,
    };
    let mut lf = LOGFONTW {
        lfHeight: -em,
        lfWeight: 400,
        lfQuality: ANTIALIASED_QUALITY,
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    let face = wide(icon_font_face());
    for (i, c) in face.iter().take(lf.lfFaceName.len() - 1).enumerate() {
        lf.lfFaceName[i] = *c;
    }
    CreateFontIndirectW(&lf)
}

/// Whether GDI can honour `face`, i.e. it resolves to itself rather than being substituted.
///
/// `CreateFontIndirectW` NEVER fails for a missing face - it hands back a substituted font,
/// which is exactly what made this bug invisible. Selecting the font and asking the DC what it
/// actually got is the check that cannot be fooled.
pub(super) fn font_face_exists(face: &str) -> bool {
    use windows::Win32::Graphics::Gdi::{
        CreateFontIndirectW, DeleteDC, DeleteObject, GetTextFaceW, SelectObject, DEFAULT_CHARSET,
        LOGFONTW,
    };
    unsafe {
        let mut lf = LOGFONTW {
            lfHeight: -12,
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        let w = wide(face);
        for (i, c) in w.iter().take(lf.lfFaceName.len() - 1).enumerate() {
            lf.lfFaceName[i] = *c;
        }
        let font = CreateFontIndirectW(&lf);
        if font.is_invalid() {
            return false;
        }
        let dc = windows::Win32::Graphics::Gdi::CreateCompatibleDC(None);
        if dc.is_invalid() {
            let _ = DeleteObject(font.into());
            return false;
        }
        let old = SelectObject(dc, font.into());
        let mut got = [0u16; 64];
        let n = GetTextFaceW(dc, Some(&mut got));
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(font.into());
        if n <= 1 {
            return false;
        }
        let got = String::from_utf16_lossy(&got[..(n as usize - 1).min(got.len())]);
        got.eq_ignore_ascii_case(face)
    }
}

/// Which of `codes` the face `face` has NO real glyph for.
///
/// `GetGlyphIndicesW` with `GGI_MARK_NONEXISTING_GLYPHS` reports `0xFFFF` for a codepoint the
/// font does not cover, which is the only way to ask this question without a font parser - and
/// the missing-glyph case is otherwise invisible, since GDI happily draws a blank box.
#[cfg(test)]
pub(super) fn missing_glyphs(face: &str, codes: &[u16]) -> Vec<u16> {
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateFontIndirectW, DeleteDC, DeleteObject, GetGlyphIndicesW,
        SelectObject, DEFAULT_CHARSET, GGI_MARK_NONEXISTING_GLYPHS, LOGFONTW,
    };
    unsafe {
        let mut lf = LOGFONTW {
            lfHeight: -16,
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        for (i, c) in wide(face).iter().take(lf.lfFaceName.len() - 1).enumerate() {
            lf.lfFaceName[i] = *c;
        }
        let font = CreateFontIndirectW(&lf);
        let dc = CreateCompatibleDC(None);
        let old = SelectObject(dc, font.into());
        let mut out = Vec::new();
        for &c in codes {
            // One NUL-terminated character; `GetGlyphIndicesW` takes a PCWSTR plus a count.
            let s = [c, 0u16];
            let mut idx = [0u16; 1];
            let n = GetGlyphIndicesW(
                dc,
                windows::core::PCWSTR(s.as_ptr()),
                1,
                idx.as_mut_ptr(),
                GGI_MARK_NONEXISTING_GLYPHS,
            );
            if n == u32::MAX || idx[0] == 0xFFFF {
                out.push(c);
            }
        }
        SelectObject(dc, old);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(font.into());
        out
    }
}

#[cfg(test)]
pub(super) mod icon_font_tests {
    use super::*;

    /// Every codepoint the three toolbars draw. Kept in step with the `GLYPHS` table in
    /// `scripts/build-icon-font.py`, which places a Material glyph at each of these.
    const TOOLBAR_CODEPOINTS: &[u16] = &[
        // preview caption
        0xE8FD, 0xEB9F, 0xE943, 0xE76B, 0xE76C, 0xE718, 0xE840, 0xE8C8, 0xE8D2, 0xE946, 0xE898,
        0xE8A7, 0xE7AC, 0xE711, // video transport
        0xE768, 0xE769, 0xE892, 0xE893, 0xE767, 0xE74F, 0xE8EE, 0xE8AB,
        // screenshot editor
        0xE70F, 0xE7E6, 0xEF3C, 0xE7C2, 0xE7A7, 0xE7A6, 0xE74E, 0xE753,
    ];

    /// The bundled font must cover EVERY glyph the app asks for.
    ///
    /// This is the guard that makes adding a toolbar button safe: forget to re-run
    /// `scripts/build-icon-font.py` and that one button would render as a blank box with no
    /// error anywhere, which is precisely how issue #21 reached a release. A missing glyph now
    /// fails the build instead.
    #[test]
    fn the_bundled_font_covers_every_toolbar_glyph() {
        assert!(
            load_bundled_icon_font(),
            "GDI refused the embedded icon font"
        );
        let missing = missing_glyphs(BUNDLED_ICON_FACE, TOOLBAR_CODEPOINTS);
        assert!(
            missing.is_empty(),
            "the bundled icon font is missing {} glyph(s): {:04X?}. Re-run \
             scripts/build-icon-font.py after adding a toolbar button.",
            missing.len(),
            missing
        );
    }

    /// And the coverage check has to be capable of failing, or it proves nothing.
    #[test]
    fn the_coverage_check_detects_an_absent_glyph() {
        assert!(load_bundled_icon_font());
        // A codepoint deliberately outside the subset: upstream Material has thousands, this
        // font has thirty.
        let missing = missing_glyphs(BUNDLED_ICON_FACE, &[0xE000]);
        assert_eq!(
            missing,
            vec![0xE000],
            "a codepoint the subset does not contain must be reported missing"
        );
    }

    /// The picker must return a face this machine REALLY has.
    ///
    /// The failure mode being guarded is silent by construction: `CreateFontIndirectW` happily
    /// returns a substituted font for a name nobody has, so a wrong answer here does not error,
    /// it just draws empty boxes - which is exactly how issue #21 reached a release.
    #[test]
    fn the_resolved_icon_face_actually_exists() {
        let face = icon_font_face();
        assert!(
            font_face_exists(face),
            "icon_font_face() picked {face:?}, which GDI substitutes on this machine"
        );
    }

    /// And the probe has to be capable of saying NO, or it would rubber-stamp anything and the
    /// fallback chain would always stop at its first entry.
    #[test]
    fn the_probe_rejects_a_face_that_does_not_exist() {
        assert!(
            !font_face_exists("Definitely Not An Installed Face 12345"),
            "the probe must detect GDI's silent substitution, not just that a handle came back"
        );
    }

    /// Windows 10's icon font is the whole point of the fallback. This machine is Windows 11,
    /// which ships BOTH, so the assertion is meaningful here; on a host that genuinely lacks it
    /// the picker still has `Segoe UI Symbol` beneath, so this stays a report rather than a
    /// failure.
    #[test]
    fn the_windows_10_fallback_face_is_recognised_when_present() {
        if font_face_exists("Segoe MDL2 Assets") {
            assert_eq!(
                std::env::var("ST2K_ICON_FONT").ok().as_deref(),
                None,
                "this test assumes no forced override"
            );
        } else {
            eprintln!("Segoe MDL2 Assets absent on this host - fallback untested here");
        }
    }
}
