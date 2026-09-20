#![cfg(test)]

use super::*;

/// Assembles a minimal one-component, one-tile, 8x8 J2K codestream: SOC + SIZ + COD +
/// QCD + EOC, with no tile-part data (`parse` never requires one). `levels` and
/// `precinct_byte` (when `Some`, one byte repeated per resolution) let each test dial
/// in exactly the field under test; everything else is the smallest legal value.
fn minimal_codestream(levels: u8, precinct_byte: Option<u8>) -> Vec<u8> {
    let mut cs = Vec::new();
    cs.extend_from_slice(&marker::SOC.to_be_bytes());

    let mut siz_body = Vec::new();
    siz_body.extend_from_slice(&0u16.to_be_bytes()); // Rsiz
    siz_body.extend_from_slice(&8u32.to_be_bytes()); // Xsiz
    siz_body.extend_from_slice(&8u32.to_be_bytes()); // Ysiz
    siz_body.extend_from_slice(&0u32.to_be_bytes()); // XOsiz
    siz_body.extend_from_slice(&0u32.to_be_bytes()); // YOsiz
    siz_body.extend_from_slice(&8u32.to_be_bytes()); // XTsiz
    siz_body.extend_from_slice(&8u32.to_be_bytes()); // YTsiz
    siz_body.extend_from_slice(&0u32.to_be_bytes()); // XTOsiz
    siz_body.extend_from_slice(&0u32.to_be_bytes()); // YTOsiz
    siz_body.extend_from_slice(&1u16.to_be_bytes()); // Csiz = 1 component
    siz_body.push(7); // Ssiz: 8-bit unsigned (prec-1 = 7)
    siz_body.push(1); // XRsiz
    siz_body.push(1); // YRsiz
    cs.extend_from_slice(&marker::SIZ.to_be_bytes());
    cs.extend_from_slice(&((siz_body.len() + 2) as u16).to_be_bytes());
    cs.extend_from_slice(&siz_body);

    let mut cod_body = Vec::new();
    let scod: u8 = if precinct_byte.is_some() { 1 } else { 0 };
    cod_body.push(scod);
    cod_body.push(0); // progression
    cod_body.extend_from_slice(&1u16.to_be_bytes()); // layers
    cod_body.push(0); // MCT off
    cod_body.push(levels);
    cod_body.push(0); // cbw exponent -> code-block width 4
    cod_body.push(0); // cbh exponent -> code-block height 4
    cod_body.push(0); // cblk style
    cod_body.push(1); // transform: reversible
    if let Some(pb) = precinct_byte {
        // One precinct-size byte per resolution level (levels + 1 resolutions).
        for _ in 0..=levels {
            cod_body.push(pb);
        }
    }
    cs.extend_from_slice(&marker::COD.to_be_bytes());
    cs.extend_from_slice(&((cod_body.len() + 2) as u16).to_be_bytes());
    cs.extend_from_slice(&cod_body);

    // QCD, style 0 (no quantization): one 8-bit exponent for the (only) subband.
    let qcd_body = [0u8, 0u8];
    cs.extend_from_slice(&marker::QCD.to_be_bytes());
    cs.extend_from_slice(&((qcd_body.len() + 2) as u16).to_be_bytes());
    cs.extend_from_slice(&qcd_body);

    cs.extend_from_slice(&marker::EOC.to_be_bytes());
    cs
}

/// Same base codestream as [`minimal_codestream`], with a COC segment for component 0
/// spliced in before EOC. `coc_levels` is the ONLY field the COC's SPcoc differs on.
fn codestream_with_coc(base_levels: u8, coc_levels: u8) -> Vec<u8> {
    let mut cs = minimal_codestream(base_levels, None);
    let eoc = cs.split_off(cs.len() - 2);

    let coc_body = vec![
        0,          // Ccoc: component 0
        0,          // Scoc: no precincts
        coc_levels, //
        0,          // cbw
        0,          // cbh
        0,          // cblk style
        1,          // transform: reversible
    ];
    cs.extend_from_slice(&marker::COC.to_be_bytes());
    cs.extend_from_slice(&((coc_body.len() + 2) as u16).to_be_bytes());
    cs.extend_from_slice(&coc_body);

    cs.extend_from_slice(&eoc);
    cs
}

/// A006: NL == 32 is spec-legal but shifts `1u32 << nb` up to 32 downstream (mod.rs),
/// which release silently wraps instead of panicking. Must be rejected, not clamped.
#[test]
fn decomposition_levels_32_is_rejected() {
    let cs = minimal_codestream(32, None);
    assert!(matches!(parse(&cs), Err(Jp2Error::Unsupported(_))));
}

#[test]
fn decomposition_levels_31_is_accepted() {
    let cs = minimal_codestream(31, None);
    assert!(parse(&cs).is_ok());
}

/// A014: a signalled PPx/PPy of 0 makes the precinct grid approach the resolution's
/// full pixel count, and mod.rs allocates one struct per precinct from it.
#[test]
fn precinct_exponent_zero_is_rejected() {
    let cs = minimal_codestream(1, Some(0x00)); // PPx = PPy = 0
    assert!(matches!(parse(&cs), Err(Jp2Error::Unsupported(_))));
}

#[test]
fn precinct_exponent_two_is_accepted() {
    let cs = minimal_codestream(1, Some(0x22)); // PPx = PPy = 2
    assert!(parse(&cs).is_ok());
}

/// Overwrite the SGcod layer count of a [`minimal_codestream`]: the COD body starts
/// four bytes after its marker (marker + Lcod), and `layers` is body bytes 2..4.
fn with_layers(mut cs: Vec<u8>, layers: u16) -> Vec<u8> {
    let cod = cs
        .windows(2)
        .position(|w| w == marker::COD.to_be_bytes())
        .expect("COD marker present");
    cs[cod + 6..cod + 8].copy_from_slice(&layers.to_be_bytes());
    cs
}

/// The layer count multiplies into the packet walk; the u16 maximum is rejected at
/// parse time, and the ceiling itself is still accepted.
#[test]
fn layer_count_65535_is_rejected() {
    let cs = with_layers(minimal_codestream(1, None), u16::MAX);
    assert!(matches!(parse(&cs), Err(Jp2Error::Unsupported(_))));
}

#[test]
fn layer_count_at_ceiling_is_accepted() {
    let cs = with_layers(minimal_codestream(1, None), MAX_LAYERS);
    assert!(parse(&cs).is_ok());
}

/// A147: a COC that actually changes the coding style is silently ignored by decode
/// (it only ever reads the global COD), so accepting it would mis-decode the file.
#[test]
fn coc_override_differing_from_cod_is_rejected() {
    let cs = codestream_with_coc(1, 2);
    assert!(matches!(parse(&cs), Err(Jp2Error::Unsupported(_))));
}

#[test]
fn coc_override_matching_cod_is_accepted() {
    let cs = codestream_with_coc(1, 1);
    assert!(parse(&cs).is_ok());
}
