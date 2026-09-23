#![cfg(test)]

use super::*;
use crate::container::psd_testutil::synthetic_psd;
use std::os::windows::ffi::OsStrExt;
use windows::core::PCWSTR;
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED, STGM_READ, STGM_SHARE_DENY_NONE,
};
use windows::Win32::UI::Shell::{SHCreateMemStream, SHCreateStreamOnFileEx};

/// A settings snapshot with `max_file_bytes` set and every other value at the user's
/// current setting, which is what the provider passes in production.
fn test_cfg(max_file_bytes: u64) -> ThumbSettings {
    ThumbSettings {
        max_file_bytes,
        ..st2k_base::settings::thumb_settings()
    }
}

/// Run the full source cascade on `bytes` (100 MB cap, like the default
/// MaxSize) and return the byte payload it hands the decode tiers.
fn source_bytes(bytes: &[u8]) -> Vec<u8> {
    let stream = unsafe { SHCreateMemStream(Some(bytes)) }.expect("SHCreateMemStream");
    match unsafe { stream_source(&stream, &test_cfg(100 << 20), 256, "test") } {
        Ok(StreamSource::Bytes(b) | StreamSource::Cover(b)) => b,
        other => panic!(
            "expected StreamSource::Bytes, got {}",
            match other {
                Ok(StreamSource::Frame(_)) => "Frame".into(),
                Ok(StreamSource::Picture(_)) => "Picture".into(),
                Ok(StreamSource::Covers(_)) => "Covers".into(),
                Ok(StreamSource::Bytes(_) | StreamSource::Cover(_)) => unreachable!(),
                Err(e) => format!("Err({e})"),
            }
        ),
    }
}

/// The oversized rescue, exercised through the REAL cascade without staging a 256 MB file.
///
/// The trick is that "oversized" is relative: pass a tiny `max_file_bytes` and an ordinary
/// image is already past every cap, taking the exact branch a half-gigabyte scan takes in
/// production. What is being proven is that the branch can decode from the STREAM ALONE,
/// with no path anywhere, which is the whole reason this rescue works inside Explorer
/// where the shell hands over a stream that knows only a leaf file name.
#[test]
fn oversized_stream_is_rescued_without_any_path() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let jpeg = substantial_jpeg();
    let stream = unsafe { SHCreateMemStream(Some(&jpeg)) }.expect("SHCreateMemStream");

    // A memory stream has NO name at all, let alone a path -- if the rescue needed one it
    // could not possibly succeed here.
    assert!(
        unsafe { stream_path(&stream) }.is_none(),
        "fixture must have no recoverable path, or this proves nothing"
    );

    // `max_file_bytes` is the USER's allowance and is left generous; the small hard cap is
    // what refuses the buffered read, which is exactly the production shape.
    // Generous user allowance, tiny HARD cap: the file is refused by OUR buffering
    // ceiling, which is precisely the production shape for a half-gigabyte scan.
    let got = unsafe { stream_source_with_caps(&stream, &test_cfg(u64::MAX), 1024, 64, "test") };
    match got {
        Ok(StreamSource::Picture(img)) => {
            // Scaled DURING decode, so the rescue never materialises the full image.
            assert!(
                img.width().max(img.height()) <= 64,
                "rescue must honour the target edge, got {}x{}",
                img.width(),
                img.height()
            );
        }
        other => panic!(
            "oversized stream should be rescued into its own picture, got {}",
            match other {
                Ok(StreamSource::Bytes(_)) => "Bytes".into(),
                Ok(StreamSource::Cover(_)) => "Cover".into(),
                Ok(StreamSource::Covers(_)) => "Covers".into(),
                Ok(StreamSource::Frame(_)) => "Frame".into(),
                Ok(StreamSource::Picture(_)) => unreachable!(),
                Err(e) => format!("Err({e})"),
            }
        ),
    }
    unsafe { CoUninitialize() };
}

/// The rescue must fire at the SHIPPED DEFAULT, not merely at "Unlimited".
///
/// Its sibling above passes `u64::MAX` for the user allowance, and that is exactly how the
/// bug hid for two releases: `u64::MAX` satisfies the rescue's `size <= max_file_bytes`
/// gate for any file at all, so the test passed while proving it only for a configuration
/// almost nobody runs. The rescue actually lives in the window
/// `hard cap < size <= MaxSize`, and a default MaxSize EQUAL to the hard cap — which is
/// what shipped — makes that window empty. Every user who never opened Settings got the
/// stock icon on files this code exists to rescue. Driving the real cascade with the real
/// constant is what makes that unrepresentable.
#[test]
fn oversized_stream_is_rescued_at_the_shipped_default_not_only_at_unlimited() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let jpeg = substantial_jpeg();
    let stream = unsafe { SHCreateMemStream(Some(&jpeg)) }.expect("SHCreateMemStream");
    let default_allowance = u64::from(st2k_base::settings::DEFAULT_MAX_FILE_MB) * 1024 * 1024;
    assert!(
        default_allowance > decode::limits::MAX_INPUT_BYTES,
        "the default allowance must sit CLEAR of the buffering ceiling, or the rescue's \
         window is empty and it can never run at the shipped setting ({default_allowance} \
         vs {})",
        decode::limits::MAX_INPUT_BYTES
    );
    // Same shape as production: the user's allowance is the default, and the (scaled-down)
    // hard cap is what refuses the buffered read.
    let got =
        unsafe { stream_source_with_caps(&stream, &test_cfg(default_allowance), 1024, 64, "test") };
    assert!(
        matches!(got, Ok(StreamSource::Picture(_))),
        "the default setting must still reach the oversized rescue"
    );
    unsafe { CoUninitialize() };
}

/// The user's own MaxSize still wins. "Too big to hold in memory" is ours to route around;
/// "do not bother with files over N" is the user's decision and the rescue must not
/// silently overrule it.
#[test]
fn rescue_does_not_overrule_the_users_max_size() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let jpeg = substantial_jpeg();
    let stream = unsafe { SHCreateMemStream(Some(&jpeg)) }.expect("SHCreateMemStream");
    // User allows only 1 KB; the fixture is bigger, so this is THEIR refusal, not ours.
    let got = unsafe { stream_source_with_caps(&stream, &test_cfg(1024), 1 << 30, 64, "test") };
    assert!(
        got.is_err(),
        "a file over the user's MaxSize must stay refused"
    );
    unsafe { CoUninitialize() };
}

/// The exact A032 fix: once the prefer-cover-art pass has already run
/// `vcodec::cover_art` for this stream (whether or not it found anything),
/// the fallback rescue after all frame tiers fail must NOT run it again —
/// that was the third full moov scan. When that pass never ran (feature
/// off), the fallback is still the only place `cover_art` gets called.
#[test]
fn fallback_cover_art_skipped_only_when_already_tried() {
    assert!(!needs_fallback_cover_art(true));
    assert!(needs_fallback_cover_art(false));
}

#[test]
fn unlimited_setting_still_obeys_the_hard_archive_cap() {
    assert_eq!(decode::effective_input_cap(u64::MAX), MAX_BYTES as u64);
    assert_eq!(
        decode::effective_input_cap((MAX_BYTES as u64) + 1),
        MAX_BYTES as u64
    );
    assert_eq!(decode::effective_input_cap(1 << 20), 1 << 20);
}

fn substantial_jpeg() -> Vec<u8> {
    let image = image::RgbImage::from_fn(320, 240, |x, y| {
        // Deterministic high-detail pixels keep the encoded preview well
        // above the 16 KiB real-preview floor.
        image::Rgb([
            ((x * 37 + y * 13) & 255) as u8,
            ((x * 11 + y * 53) & 255) as u8,
            ((x * 71 + y * 19) & 255) as u8,
        ])
    });
    let mut out = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 95)
        .encode_image(&image::DynamicImage::ImageRgb8(image))
        .expect("encode test JPEG");
    assert!(out.len() >= decode::MIN_RAW_PREVIEW);
    out
}

fn mark_synthetic_tiff_raw(bytes: &mut [u8]) {
    assert!(bytes.len() >= 26);
    bytes[..8].copy_from_slice(b"II\x2A\0\x08\0\0\0");
    bytes[8..10].copy_from_slice(&1u16.to_le_bytes());
    bytes[10..12].copy_from_slice(&0x0106u16.to_le_bytes()); // PhotometricInterpretation
    bytes[12..14].copy_from_slice(&3u16.to_le_bytes()); // SHORT
    bytes[14..18].copy_from_slice(&1u32.to_le_bytes());
    bytes[18..20].copy_from_slice(&32_803u16.to_le_bytes()); // CFA
}

#[test]
fn raw_prefix_carver_returns_a_complete_early_preview() {
    let jpeg = substantial_jpeg();
    let mut raw = b"II\x2A\0raw-header".to_vec();
    raw.extend_from_slice(&[0x55; 4096]);
    raw.extend_from_slice(&jpeg);
    raw.extend_from_slice(&[0xA5; 4096]);

    let got =
        decode::largest_embedded_jpeg(&raw, decode::MIN_RAW_PREVIEW).expect("early RAW preview");
    assert_eq!(got, jpeg.as_slice());
    assert!(
        image::load_from_memory(got).is_ok(),
        "must return a full JPEG"
    );
}

/// A minimal TIFF whose IFD0 carries `NewSubfileType` (tag 0xFE) with `value`.
fn tiff_with_new_subfile_type(value: u32, little: bool, long_type: bool) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(if little { b"II\x2A\0" } else { b"MM\0\x2A" });
    let put32 = |b: &mut Vec<u8>, v: u32| {
        b.extend_from_slice(&if little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        })
    };
    let put16 = |b: &mut Vec<u8>, v: u16| {
        b.extend_from_slice(&if little {
            v.to_le_bytes()
        } else {
            v.to_be_bytes()
        })
    };
    put32(&mut b, 8); // IFD0 at offset 8
    put16(&mut b, 1); // one entry
    put16(&mut b, 0x00FE); // NewSubfileType
    put16(&mut b, if long_type { 4 } else { 3 });
    put32(&mut b, 1); // count
    if long_type {
        put32(&mut b, value);
    } else {
        // A SHORT is left-justified in the 4-byte value field, in BOTH endiannesses.
        put16(&mut b, value as u16);
        put16(&mut b, 0);
    }
    put32(&mut b, 0); // next IFD: none
    b
}

/// Camera RAW containers (Hasselblad 3fr, Kodak dcr/kdc, Epson erf, Phase One fff,
/// Nikon nef) put a small preview in IFD0 and the sensor data in SubIFDs, flagging IFD0
/// `NewSubfileType = 1`. The `image` crate only ever decodes IFD0, so before this the
/// FIRST tier answered from a postage stamp — and for a Kodak DCS760C `.dcr`, from a
/// near-black placeholder, which is how a good photo thumbnailed as a black tile.
#[test]
fn reduced_resolution_ifd0_is_recognised_in_both_endiannesses_and_widths() {
    for little in [true, false] {
        for long_type in [true, false] {
            assert!(
                tiff_ifd0_is_reduced(&tiff_with_new_subfile_type(1, little, long_type)),
                "reduced flag missed (little={little}, long={long_type})"
            );
            // 2 = a PAGE of a multi-page document, 4 = a transparency mask. Neither is a
            // reduced copy, and matching them would cost a normal multi-page TIFF its
            // fast tier.
            for other in [0u32, 2, 4] {
                assert!(
                    !tiff_ifd0_is_reduced(&tiff_with_new_subfile_type(other, little, long_type)),
                    "NewSubfileType={other} must not read as reduced"
                );
            }
        }
    }
    // Bit 0 set alongside other bits still means reduced.
    assert!(tiff_ifd0_is_reduced(&tiff_with_new_subfile_type(
        3, true, true
    )));
}

#[test]
fn reduced_resolution_check_declines_non_tiff_and_truncation() {
    assert!(!tiff_ifd0_is_reduced(b""));
    assert!(!tiff_ifd0_is_reduced(b"not a tiff at all"));
    assert!(!tiff_ifd0_is_reduced(b"II\x2B\0")); // BigTIFF: different IFD layout, declines
    let full = tiff_with_new_subfile_type(1, true, true);
    for cut in 0..full.len() {
        let _ = tiff_ifd0_is_reduced(&full[..cut]); // must not panic at any prefix
    }
}

#[test]
fn raw_fast_path_gate_rejects_plain_tiff_and_non_raw() {
    assert!(is_raw_extension("pef"));
    assert!(!is_raw_extension("tif"));
    assert!(looks_like_raw_container(b"II\x2A\0rest", true));
    assert!(!looks_like_raw_container(b"II\x2A\0rest", false));
    let mut raw_tiff = [0u8; 26];
    mark_synthetic_tiff_raw(&mut raw_tiff);
    assert!(looks_like_raw_container(&raw_tiff, false));
    assert!(looks_like_raw_container(b"FUJIFILMCCD-RAW", false));
    assert!(!looks_like_raw_container(b"not a camera raw", true));
}

/// A RAR comic past the buffering ceiling gets the cover the whole-file read picks: its block
/// headers are walked off the stream and only the cover's entry is read.
#[test]
fn an_oversized_rar_comic_gets_the_cover_the_whole_file_gets() {
    let Some(cbr) = st2k_base::testcorpus::read("sample.cbr") else {
        eprintln!("NOT MEASURED: sample.cbr absent");
        return;
    };
    // The same preferences the streamed read uses (`archive_cover_streamed` reads them too).
    let prefs = crate::container::select::CoverPrefs::from_settings();
    let whole = crate::container::archive_covers(&cbr, 1, &prefs)
        .and_then(|mut covers| covers.pop())
        .expect("the buffered read finds a cover");
    let stream = unsafe { SHCreateMemStream(Some(&cbr)) }.expect("SHCreateMemStream");
    // A hard cap below the file's size: the oversized rescue, as for a 300 MB comic.
    assert!(
        cbr.len() > 128,
        "the fixture must be bigger than the cap below"
    );
    let got = unsafe { stream_source_with_caps(&stream, &test_cfg(u64::MAX), 128, 256, "test") };
    match got {
        Ok(StreamSource::Cover(cover)) => assert_eq!(
            crate::decode::decode_preview(&cover)
                .ok()
                .map(|i| i.to_rgba8()),
            crate::decode::decode_preview(&whole)
                .ok()
                .map(|i| i.to_rgba8())
        ),
        other => panic!(
            "expected the streamed cover, got {}",
            match other {
                Ok(StreamSource::Bytes(b)) => format!("Bytes({})", b.len()),
                Ok(StreamSource::Covers(_)) => "Covers".into(),
                Ok(StreamSource::Frame(_)) => "Frame".into(),
                Ok(StreamSource::Picture(_)) => "Picture".into(),
                Ok(StreamSource::Cover(_)) => unreachable!(),
                Err(e) => format!("Err({e})"),
            }
        ),
    }
}

/// A WMA shows its cover through the shell's stream cascade. It shares the ASF container with
/// WMV, and the cascade used to stop at "video with no decodable frame", so no WMA ever showed
/// its cover in Explorer or the preview pane (the big-file gate: the CLI drew it, the shell
/// surfaces drew nothing at any size).
#[test]
fn a_wma_shows_its_cover_through_the_stream() {
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    for name in ["real.wma", "sample.wma"] {
        let Some(wma) = st2k_base::testcorpus::read(name) else {
            eprintln!("NOT MEASURED: {name} absent");
            continue;
        };
        let want = crate::container::extract_cover(&wma).is_some();
        let stream = unsafe { SHCreateMemStream(Some(&wma)) }.expect("SHCreateMemStream");
        let got = unsafe { stream_source(&stream, &test_cfg(u64::MAX), 256, "test") };
        assert_eq!(
            matches!(got, Ok(StreamSource::Cover(_))),
            want,
            "{name}: the stream must find the cover the buffered read finds"
        );
    }
    if com {
        unsafe { CoUninitialize() };
    }
}

#[test]
fn raw_fast_path_respects_the_configured_input_cap() {
    let large_raw = (RAW_PREFIX_BYTES as u64) + 1;
    assert!(raw_preview_size_allowed(large_raw, u64::MAX));
    assert!(!raw_preview_size_allowed(large_raw, 1024 * 1024));
}

#[test]
fn unnamed_raw_stream_returns_only_its_early_preview_and_honors_max_size() {
    let jpeg = substantial_jpeg();
    let mut raw = vec![0u8; RAW_PREFIX_BYTES + 1];
    mark_synthetic_tiff_raw(&mut raw);
    let jpeg_start = 4096;
    raw[jpeg_start..jpeg_start + jpeg.len()].copy_from_slice(&jpeg);

    let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
    match unsafe { stream_source(&stream, &test_cfg(u64::MAX), 256, "test") } {
        Ok(StreamSource::Bytes(bytes)) => assert_eq!(bytes, jpeg),
        _ => panic!("unnamed RAW stream should use its embedded preview"),
    }

    let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
    assert!(
        unsafe { stream_source(&stream, &test_cfg(1024 * 1024), 256, "test") }.is_err(),
        "RAW fast path must not bypass the configured MaxSize"
    );
    drop(stream);

    raw[jpeg_start..jpeg_start + jpeg.len()].fill(0);
    *raw.last_mut().unwrap() = 0xA5;
    let stream = unsafe { SHCreateMemStream(Some(&raw)) }.expect("SHCreateMemStream");
    match unsafe { stream_source(&stream, &test_cfg(u64::MAX), 256, "test") } {
        Ok(StreamSource::Bytes(bytes)) => {
            assert_eq!(bytes.len(), raw.len());
            assert_eq!(bytes.last(), Some(&0xA5));
            assert_eq!(&bytes[..26], &raw[..26]);
        }
        _ => panic!("RAW without an early preview should keep the full-read fallback"),
    }
}

#[test]
fn real_large_pef_stream_uses_bounded_preview_when_corpus_is_available() {
    let path = st2k_base::testcorpus::real_dir().join("sample.pef");
    if !path.exists() {
        return;
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let stream = unsafe {
        SHCreateStreamOnFileEx(
            PCWSTR(wide.as_ptr()),
            (STGM_READ | STGM_SHARE_DENY_NONE).0,
            0,
            false,
            None,
        )
        .expect("SHCreateStreamOnFileEx")
    };
    match unsafe { stream_source(&stream, &test_cfg(u64::MAX), 256, "test") } {
        Ok(StreamSource::Bytes(bytes)) => {
            assert!(bytes.len() >= decode::MIN_RAW_PREVIEW);
            assert!(
                bytes.len() < RAW_PREFIX_BYTES,
                "must return only the embedded preview, not the RAW prefix"
            );
            let thumb = decode::decode_thumbnail_opts(&bytes, 256, true)
                .expect("embedded PEF preview should decode");
            assert_eq!(
                (thumb.width, thumb.height),
                (256, 171),
                "shell fast path should match the corpus PEF thumbnail orientation"
            );
        }
        _ => panic!("large PEF should return its bounded embedded preview"),
    }
    drop(stream);
    if com {
        unsafe { CoUninitialize() };
    }
}

/// The whole point of the EXR tier: a render pass past the size cap must still
/// produce a picture. `max_file_bytes = 1` puts this file hopelessly over the
/// limit, so every other branch of the cascade would return the stock icon.
#[test]
fn oversized_exr_is_scaled_off_the_stream_instead_of_refused() {
    let exr = crate::decode::tests::ramp_exr_bytes(600, 400);
    let stream = unsafe { SHCreateMemStream(Some(&exr)) }.expect("SHCreateMemStream");
    match unsafe { stream_source(&stream, &test_cfg(1), 64, "test") } {
        // step = floor(600 / 64) = 9 -> ceil(600/9) x ceil(400/9) = 67x45.
        Ok(StreamSource::Picture(img)) => {
            assert_eq!((img.width(), img.height()), (67, 45));
        }
        other => panic!(
            "oversized EXR must yield its own picture, scaled, got {}",
            match other {
                Ok(StreamSource::Bytes(_)) => "Bytes".to_string(),
                Ok(StreamSource::Cover(_)) => "Cover".to_string(),
                Ok(StreamSource::Covers(_)) => "Covers".to_string(),
                Ok(StreamSource::Frame(_)) => "Frame".to_string(),
                Ok(StreamSource::Picture(_)) => unreachable!(),
                Err(e) => format!("Err({e})"),
            }
        ),
    }
    drop(stream);

    // The requested edge really drives the decode (a smaller tile reads less).
    let stream = unsafe { SHCreateMemStream(Some(&exr)) }.expect("SHCreateMemStream");
    match unsafe { stream_source(&stream, &test_cfg(u64::MAX), 256, "test") } {
        // step = floor(600 / 256) = 2 -> 300x200.
        Ok(StreamSource::Picture(img)) => assert_eq!((img.width(), img.height()), (300, 200)),
        _ => panic!("EXR should scale to the requested edge"),
    }
}

#[test]
fn sevenz_unknown_size_probe_is_signature_exact_and_rewinds() {
    const SIGNATURE: &[u8] = &[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];
    let mut bytes = SIGNATURE.to_vec();
    bytes.extend_from_slice(b"payload");
    let stream = unsafe { SHCreateMemStream(Some(&bytes)) }.expect("SHCreateMemStream");

    assert!(unsafe { stream_head(&stream) }.is_7z());
    let mut first = [0u8; 6];
    let mut got = 0u32;
    unsafe {
        stream
            .Read(
                first.as_mut_ptr() as *mut c_void,
                first.len() as u32,
                Some(&mut got),
            )
            .unwrap();
    }
    assert_eq!(got as usize, first.len());
    assert_eq!(first, SIGNATURE);

    let not_7z = unsafe { SHCreateMemStream(Some(b"PK\x03\x04zip")) }.expect("SHCreateMemStream");
    assert!(!unsafe { stream_head(&not_7z) }.is_7z());
}

#[test]
fn under_cap_opaque_psd_reads_only_the_head_prefix() {
    // 6 MB of layer data behind the resources section: the fast path must
    // hand the decode tiers the exact head prefix, not the whole file.
    let (psd, head_len) = synthetic_psd(3, true, 6 << 20);
    let got = source_bytes(&psd);
    assert_eq!(
        got.len(),
        head_len,
        "fast path should stop at the resources section"
    );
    assert_eq!(&got[..], &psd[..head_len]);
    // And the prefix must actually decode to the baked thumbnail.
    assert!(crate::container::extract_cover(&got).is_some());
}

#[test]
fn psd_without_baked_thumbnail_falls_back_to_the_whole_file() {
    let (psd, _) = synthetic_psd(3, false, 1 << 20);
    let got = source_bytes(&psd);
    assert_eq!(
        got.len(),
        psd.len(),
        "no baked preview -> the pre-fast-path whole read"
    );
}

#[test]
fn under_cap_dwg_reads_only_the_preview_section() {
    // 4 MB of "object database" behind the preview records: the fast path
    // stops right after the PNG record's payload.
    let (dwg, head_len) = crate::container::dwg_testutil::synthetic_dwg(true, 4 << 20);
    let got = source_bytes(&dwg);
    assert_eq!(
        got.len(),
        head_len,
        "DWG fast path should stop after the record payload"
    );
    assert!(crate::container::extract_cover(&got).is_some());
}

#[test]
fn dwg_without_a_preview_section_falls_back_to_the_whole_file() {
    // RASTERPREVIEW=0 / pre-R13: no sentinel, so no fast path. The whole-file
    // read then fails the decode tiers exactly as it did before this path.
    let (dwg, _) = crate::container::dwg_testutil::synthetic_dwg(false, 1 << 20);
    let stream = unsafe { SHCreateMemStream(Some(&dwg)) }.expect("SHCreateMemStream");
    match unsafe { stream_source(&stream, &test_cfg(100 << 20), 256, "test") } {
        Ok(StreamSource::Bytes(b)) => assert_eq!(b.len(), dwg.len()),
        Ok(_) => panic!("expected Bytes"),
        Err(_) => panic!("expected the whole-file read, not a failure"),
    }
}

/// The fast path sits BEFORE the size-cap branch, so a preview-bearing DWG
/// past the user's MaxSize now thumbnails off its exact prefix. Previously it
/// fell to the oversized arm, which has no DWG rescue (`has_head_preview`
/// covers only blend/PSD/gzip) and returned E_FAIL — the stock icon. This is a
/// new capability, not just a speedup.
#[test]
fn oversized_dwg_now_thumbnails_via_the_exact_prefix() {
    let (dwg, head_len) = crate::container::dwg_testutil::synthetic_dwg(true, 4 << 20);
    let stream = unsafe { SHCreateMemStream(Some(&dwg)) }.expect("SHCreateMemStream");
    // A 1 MiB cap puts this 4 MB+ file firmly over the limit.
    match unsafe { stream_source(&stream, &test_cfg(1 << 20), 256, "test") } {
        Ok(StreamSource::Bytes(b)) => {
            assert_eq!(b.len(), head_len);
            assert!(crate::container::extract_cover(&b).is_some());
        }
        other => panic!(
            "oversized DWG should now yield its preview, got {}",
            other.is_ok()
        ),
    }
}

/// Issue #33, at the layer that decides what BYTES exist.
///
/// This is the half of the bug a decode-side fix alone could never reach: once the
/// cascade commits to the head prefix, the merged composite is not slow to get at, it is
/// absent. So a PSD whose baked preview cannot serve the request must fall through to the
/// whole-file read HERE, before the decoder is ever asked.
///
/// Driven at two sizes on ONE fixture on purpose — the difference between the two
/// assertions is the request, nothing else.
#[test]
fn a_psd_preview_too_small_for_the_request_yields_the_whole_file() {
    use crate::container::psd_testutil::synthetic_psd_preview;
    // A 32 px baked preview: ample for an icon, hopeless for a preview pane.
    let (psd, head_len) = synthetic_psd_preview(3, Some(32), 1 << 20);
    let stream = unsafe { SHCreateMemStream(Some(&psd)) }.expect("SHCreateMemStream");
    match unsafe { stream_source(&stream, &test_cfg(100 << 20), 96, "test") } {
        Ok(StreamSource::Bytes(b)) => assert_eq!(
            b.len(),
            head_len,
            "a 32 px preview still answers a 96 px tile off the prefix"
        ),
        other => panic!("expected Bytes, got {}", other.is_ok()),
    }
    let stream = unsafe { SHCreateMemStream(Some(&psd)) }.expect("SHCreateMemStream");
    match unsafe { stream_source(&stream, &test_cfg(100 << 20), 1024, "test") } {
        Ok(StreamSource::Bytes(b)) => assert_eq!(
            b.len(),
            psd.len(),
            "a 1024 px request must reach the whole file, or the composite is unreachable"
        ),
        other => panic!("expected Bytes, got {}", other.is_ok()),
    }
}

/// The narrowing, at this layer: a `.blend` keeps the prefix at EVERY size, because
/// reading its whole document would buy the identical picture.
#[test]
fn a_blend_keeps_the_head_prefix_at_every_request_size() {
    // A real TEST thumbnail block plus a scene-data tail past `HEAD_PREVIEW_BYTES`, which
    // is what it takes to reach this fast path at all: `.blend` gets the blanket prefix
    // length, and the cascade declines any prefix that is not strictly smaller than the
    // file. The thumbnail inside is 4x3 — far smaller than either request below, so if
    // the issue-#33 gate applied here it would refuse both.
    let blend = crate::container::blend_testutil::synthetic_blend(&vec![
        0u8;
        decode::HEAD_PREVIEW_BYTES
            + (1 << 20)
    ]);
    let mut seen: Vec<usize> = Vec::new();
    for cx in [96u32, 2048] {
        let stream = unsafe { SHCreateMemStream(Some(&blend)) }.expect("SHCreateMemStream");
        match unsafe { stream_source(&stream, &test_cfg(100 << 20), cx, "test") } {
            Ok(StreamSource::Bytes(b)) => {
                assert!(
                    b.len() < blend.len(),
                    "the .blend fast path must hold at {cx} px"
                );
                seen.push(b.len());
            }
            other => panic!("expected Bytes, got {}", other.is_ok()),
        }
    }
    assert_eq!(
        seen[0], seen[1],
        "the request size must not change what a .blend hands back"
    );
}

#[test]
fn transparent_psd_falls_back_to_the_whole_file() {
    // 4 channels in RGB mode = alpha: the composite path needs every byte,
    // so the fast path must bow out even though a baked thumbnail exists.
    let (psd, _) = synthetic_psd(4, true, 1 << 20);
    let got = source_bytes(&psd);
    assert_eq!(got.len(), psd.len());
}

/// A shell stream may expose no filename, so the generic-extension gate
/// cannot distinguish `.7z` from `.cb7`. Oversized 7z must still stop at
/// MaxSize instead of falling into the old cap-bypassing CB7 rescue.
#[test]
fn nameless_oversized_7z_is_not_streamed_past_max_size() {
    const ARCHIVE: &[u8] = include_bytes!("../../tests/fixtures/sevenz/solid_order.7z");
    let path = std::env::temp_dir().join(format!("st2k_generic_cap_{}.7z", std::process::id()));
    std::fs::write(&path, ARCHIVE).expect("write fixture");
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let open = || unsafe {
        SHCreateStreamOnFileEx(
            PCWSTR(wide.as_ptr()),
            (STGM_READ | STGM_SHARE_DENY_NONE).0,
            0,
            false,
            None,
        )
        .expect("SHCreateStreamOnFileEx")
    };

    let stream = open();
    let stat_path = unsafe { stream_path(&stream) };
    assert!(
        stat_path.is_none(),
        "fixture must exercise the name-less shell-stream path, got {stat_path:?}"
    );
    // The other half of the same fact, and the reason two probes were quietly broken:
    // the stream DOES report a usable file TYPE even though it reports no usable PATH.
    // Anything that only needs the extension must therefore ask `stream_extension`;
    // asking `stream_path` gets `None` and silently disables the feature in Explorer.
    assert_eq!(
        unsafe { stream_extension(&stream) }.as_deref(),
        Some("7z"),
        "extension must still be recoverable from a stream that exposes no path"
    );
    assert!(
        unsafe { stream_source(&stream, &test_cfg(1), 256, "test") }.is_err(),
        "over-MaxSize name-less 7z must keep the stock icon"
    );
    drop(stream);

    if com {
        unsafe { CoUninitialize() };
    }
    let _ = std::fs::remove_file(path);
}

/// The whole-file read honours `max` exactly: a stream of exactly `max` bytes is
/// accepted in full, one byte more is refused, and a size hint far past the cap
/// changes neither answer (it only sizes the reservation). 3 MiB spans several of
/// the 1 MiB read steps, so the in-place growth path is exercised, not just one read.
#[test]
fn read_all_accepts_exactly_max_and_refuses_one_more() {
    let bytes: Vec<u8> = (0..=255u8).cycle().take(3 << 20).collect();
    let max = bytes.len();
    let stream = unsafe { SHCreateMemStream(Some(&bytes)) }.expect("SHCreateMemStream");
    let got = unsafe { read_all(&stream, max, Some(u64::MAX)) }.expect("exactly max bytes");
    assert_eq!(got, bytes);

    let stream = unsafe { SHCreateMemStream(Some(&bytes)) }.expect("SHCreateMemStream");
    assert!(
        unsafe { read_all(&stream, max - 1, Some(max as u64)) }.is_err(),
        "one byte past the cap must be refused"
    );
}

/// `stream_prefix_from` keeps the bytes it was handed verbatim and reads only the
/// remainder: the RAW fast path relies on this to read its 16 MiB prefix exactly once
/// across the shared head, the 1 MiB sniff and the prefix itself.
#[test]
fn stream_prefix_from_continues_after_the_bytes_in_hand() {
    let bytes: Vec<u8> = (0..=255u8).cycle().take(8192).collect();
    let stream = unsafe { SHCreateMemStream(Some(&bytes)) }.expect("SHCreateMemStream");
    // A head copy carrying a marker the stream does not contain: if the helper re-read
    // those bytes, the marker would be gone.
    let mut in_hand = bytes[..1000].to_vec();
    in_hand[0] = 0xEE;
    let size = Some(bytes.len() as u64);
    let out = unsafe { stream_prefix_from(&stream, in_hand, size, 4096) };
    let out = out.expect("prefix");
    assert_eq!(out.len(), 4096);
    assert_eq!(out[0], 0xEE, "bytes already in hand must not be re-read");
    assert_eq!(&out[1..], &bytes[1..4096]);
    // And the stream is rewound for the next probe.
    let mut first = [0u8; 4];
    let mut got = 0u32;
    unsafe { stream.Read(first.as_mut_ptr() as *mut c_void, 4, Some(&mut got)) }.unwrap();
    assert_eq!(&first, &bytes[..4]);
}

/// The RAR buffer is bounded by the same effective cap the size gate used, not by the
/// hard ceiling, so a stream that delivers more than its `Stat` size declared stops at
/// the user's MaxSize.
#[test]
fn rar_read_cap_is_the_effective_cap_not_the_hard_ceiling() {
    assert_eq!(rar_buffer_cap(1 << 20), 1 << 20);
    assert_eq!(rar_buffer_cap(u64::MAX), MAX_BYTES);
}

/// MaxSize reaches the two non-targeted video fallbacks: a file past it must not pay
/// their 64 MiB / 128 + 96 MiB reads, a file inside it still may, and a stream with no
/// reported size stays eligible because both reads are bounded on their own.
#[test]
fn prefix_video_tiers_are_gated_on_max_size() {
    assert!(prefix_tiers_allowed(Some(10 << 20), 100 << 20));
    assert!(prefix_tiers_allowed(Some(100 << 20), 100 << 20));
    assert!(!prefix_tiers_allowed(Some((100 << 20) + 1), 100 << 20));
    assert!(prefix_tiers_allowed(None, 1));
}

/// A tier whose precondition is false is never entered: without Media Foundation the
/// in-process tiers must not pay their reads only to fail.
#[test]
fn disabled_tier_never_runs() {
    assert!(tier_if(false, || unreachable!("a disabled tier must not run")).is_none());
    let ran = std::cell::Cell::new(false);
    assert!(tier_if(true, || {
        ran.set(true);
        None
    })
    .is_none());
    assert!(ran.get());
}

/// What the cascade hands back for `bytes` when a tiny hard cap makes it "too big to hold":
/// the production shape of a file past `MAX_INPUT_BYTES`, without staging one.
fn oversized_source(bytes: &[u8], edge: u32) -> Option<StreamSource> {
    let stream = unsafe { SHCreateMemStream(Some(bytes)) }.expect("SHCreateMemStream");
    unsafe { stream_source_with_caps(&stream, &test_cfg(u64::MAX), 1024, edge, "test") }.ok()
}

/// The formats whose preview sits where no bounded head read reaches it in a big file - a
/// compound file's streams behind its FAT, a DOS EPS's preview after its PostScript, a DXF's
/// preview section at its end, a PDF's first page behind the cross-reference at its end - are
/// still drawn past the input ceiling, from the stream alone (the big-file gate, 2026-09-23).
#[test]
fn previews_found_by_offset_or_index_survive_the_ceiling() {
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    for name in [
        "real.max",
        "sample.sldasm",
        "real.eps",
        "real.dxf",
        "real.pdf",
    ] {
        let Some(bytes) = st2k_base::testcorpus::read(name) else {
            eprintln!("NOT MEASURED: {name} absent");
            continue;
        };
        // The rescue itself, not the head window behind it: a file this small would also fit
        // that window, which a big one of these formats does not.
        let stream = unsafe { SHCreateMemStream(Some(&bytes)) }.expect("SHCreateMemStream");
        let head = unsafe { stream_head(&stream) };
        let found = unsafe {
            offset_cover(&stream, &head, "test").or_else(|| pdf_page(&stream, &head, 256, "test"))
        };
        let img = match found {
            Some(StreamSource::Bytes(b) | StreamSource::Cover(b)) => {
                crate::decode::decode_preview(&b).ok()
            }
            Some(StreamSource::Frame(img) | StreamSource::Picture(img)) => Some(img),
            _ => None,
        };
        let img = img.unwrap_or_else(|| panic!("{name}: no preview by offset or index"));
        assert!(
            img.width() >= 16 && img.height() >= 16,
            "{name}: {}x{}",
            img.width(),
            img.height()
        );
        // And the whole cascade, past a (scaled-down) ceiling, answers too.
        assert!(
            oversized_source(&bytes, 256).is_some(),
            "{name}: nothing past the ceiling"
        );
    }
    if com {
        unsafe { CoUninitialize() };
    }
}

/// The head window's contract (see `headwin`): a small picture in front of a long tail is
/// served from the file's head, and a picture that runs past the head is refused rather than
/// drawn from its first rows.
#[test]
fn the_head_window_serves_a_picture_it_holds_whole_and_refuses_one_it_does_not() {
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    // A 64x48 PNG, then 40 MiB of zeros: the whole picture is in the first 16 MiB.
    let mut png = Vec::new();
    image::RgbImage::from_fn(64, 48, |x, y| image::Rgb([x as u8 * 4, y as u8 * 5, 90]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("png");
    let mut tailed = png.clone();
    tailed.resize(40 << 20, 0);
    let stream = unsafe { SHCreateMemStream(Some(&tailed)) }.expect("SHCreateMemStream");
    let head = unsafe { stream_head(&stream) };
    let img = unsafe { head_window(&stream, &head, 256, "test") }.expect("the head holds it");
    assert_eq!((img.width(), img.height()), (64, 48));

    // An uncompressed 40 MiB BMP: its rows run past both windows, so no picture.
    let (w, h) = (4096u32, 3413u32);
    let row = (w * 3) as usize;
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&(54 + row as u32 * h).to_le_bytes());
    bmp.extend_from_slice(&[0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0]);
    bmp.extend_from_slice(&w.to_le_bytes());
    bmp.extend_from_slice(&h.to_le_bytes());
    bmp.extend_from_slice(&[1, 0, 24, 0]);
    bmp.extend_from_slice(&[0u8; 24]);
    bmp.extend((0..row * h as usize).map(|i| (i % 251) as u8));
    let stream = unsafe { SHCreateMemStream(Some(&bmp)) }.expect("SHCreateMemStream");
    let head = unsafe { stream_head(&stream) };
    assert!(
        unsafe { head_window(&stream, &head, 256, "test") }.is_none(),
        "a picture past the head must not be drawn from its first rows"
    );
    if com {
        unsafe { CoUninitialize() };
    }
}
