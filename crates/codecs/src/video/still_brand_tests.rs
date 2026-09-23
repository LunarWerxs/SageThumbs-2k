#![cfg(test)]

use super::is_video_magic;

/// Build an `ftyp` box: size, "ftyp", major brand, minor version, compatible brands.
fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
    let size = 16 + 4 * compat.len();
    let mut v = (size as u32).to_be_bytes().to_vec();
    v.extend_from_slice(b"ftyp");
    v.extend_from_slice(major);
    v.extend_from_slice(&[0, 0, 0, 1]); // minor version
    for c in compat {
        v.extend_from_slice(*c);
    }
    v.resize(size.max(16), 0);
    v
}

/// A still image must NEVER be classified as video, and the shell is why this is not a
/// cosmetic distinction: `streamsrc::stream_source` STOPS when something sniffs as video
/// and then decodes no frame, rather than falling through to the image tiers. So one
/// unrecognised image brand is a permanent stock icon in Explorer, while the CLI (which
/// reads by path and never consults this) renders the same file perfectly.
///
/// That shipped. libheif writes `mif3` as the MAJOR brand of an alpha AVIF; `mif3` was
/// absent from the allowlist, and `sample-avif-alpha.avif` returned 0x8004B200 from
/// `IShellItemImageFactory` while `st2k thumbnail` produced a perfect 256x256 tile.
#[test]
fn miaf_still_brands_are_never_mistaken_for_video() {
    // The exact regression: major brand mif3, no compatible brands at all.
    assert!(!is_video_magic(&ftyp(b"mif3", &[])));

    // The MIAF/HEIF/AVIF family, as major brands.
    for major in [
        b"heic", b"heix", b"heim", b"heis", b"mif1", b"mif2", b"mif3", b"msf1", b"miaf", b"MA1A",
        b"MA1B", b"avif", b"avio", b"avis", b"heif",
    ] {
        assert!(
            !is_video_magic(&ftyp(major, &[])),
            "{} must not sniff as video",
            String::from_utf8_lossy(major)
        );
    }

    // The robustness half: an UNKNOWN major brand still reads as a still when a known one
    // appears among the compatible brands. This is what stops the next exotic brand from
    // costing another silent Explorer regression.
    assert!(!is_video_magic(&ftyp(b"zzzz", &[b"mif1", b"avif"])));
    assert!(!is_video_magic(&ftyp(b"1234", &[b"miaf"])));

    // ...and real video is still video, both by major brand and with video-only compat.
    assert!(is_video_magic(&ftyp(b"isom", &[b"isom", b"iso2", b"mp41"])));
    assert!(is_video_magic(&ftyp(b"mp42", &[])));
    assert!(is_video_magic(&ftyp(b"qt  ", &[])));

    // Audio and Canon RAW keep their existing exclusions.
    assert!(!is_video_magic(&ftyp(b"M4A ", &[])));
    assert!(!is_video_magic(&ftyp(b"crx ", &[])));

    // A declared box size larger than the buffer must not panic or over-read.
    let mut lying = ftyp(b"zzzz", &[b"mif1"]);
    lying[0..4].copy_from_slice(&9999u32.to_be_bytes());
    assert!(!is_video_magic(&lying));
}

/// A head that STOPS INSIDE the ftyp box must not panic.
///
/// The function's own entry guard is `len >= 12` (enough for `ftyp` + the major brand),
/// but compatible brands start at offset 16 — so a 12..=15 byte head used to reach
/// `box_end.clamp(16, head.len())`, which is `min > max` and panics by `clamp`'s
/// contract. Not a soft failure: this parser runs in-process inside `explorer.exe`
/// under `panic = "abort"`, so a 13-byte ftyp-shaped file or stream aborted the user's
/// whole shell. Found by `fuzz::parsers_survive_mutation_of_synthetic_seeds`
/// ("seed 'stub15' iter 0: min > max. min = 16, max = 13").
///
/// Every prefix is swept, not just the guilty lengths, because the next edit to this
/// function is as likely to move the boundary as to remove it.
#[test]
fn a_head_truncated_inside_the_ftyp_box_does_not_panic() {
    for major in [b"mp42", b"heic", b"zzzz"] {
        let full = ftyp(major, &[b"mif1", b"isom"]);
        for n in 0..=full.len() {
            let _ = is_video_magic(&full[..n]);
        }
    }

    // The exact fuzz seed: 13 bytes of an ISO-BMFF head, box size declaring more than
    // arrived. The major brand is all that's readable, and it must still be honoured —
    // skipping the compatible-brands scan may not silently reclassify the file.
    let heic13 = &ftyp(b"heic", &[b"mif1"])[..13];
    assert_eq!(heic13.len(), 13);
    assert!(
        !is_video_magic(heic13),
        "a short HEIC head is still a still"
    );
    assert!(
        is_video_magic(&ftyp(b"mp42", &[b"isom"])[..13]),
        "a short mp4 head is still video"
    );

    // The boundary either side of 16, where the clamp's min and max meet.
    for n in 12..=16 {
        assert!(!is_video_magic(&ftyp(b"avif", &[b"mif1"])[..n]));
        assert!(is_video_magic(&ftyp(b"isom", &[b"iso2"])[..n]));
    }
}
