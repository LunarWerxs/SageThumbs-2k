#![cfg(test)]

//! Direct fuzz entry points into `strip`'s private per-format parsers. Test-only.
//!
//! Lives here rather than in `crate::fuzz` because these modules (`isobmff`, `svgmeta`,
//! `webpmeta`, `xmpinfo`, `ddsinfo`, `jumbf`) are private to `strip`; naming them from inside
//! keeps their production visibility unchanged instead of widening it for a test — the same
//! shape as [`crate::container::fuzzseed`].
//!
//! All of them are reached IN-PROCESS from `strip_metadata`/`read_info_verbose`, which run
//! inside `explorer.exe` under `panic = "abort"`. [`targets`] puts them in the fuzz gate's
//! target list and [`seeds`] gives the gate a structurally valid file for each, so a mutation
//! starts past each parser's front door.

use super::*;

/// One parser entry point: a stable name and a closure that must never panic on any input.
type Target = (&'static str, fn(&[u8]));

pub(crate) fn targets() -> Vec<Target> {
    vec![
        ("isobmff::items", |b| {
            let _ = crate::isobmff::items(b);
        }),
        ("strip::isobmff::strip", |b| {
            let _ = isobmff::strip(b);
        }),
        ("strip::svgmeta::strip", |b| {
            let _ = svgmeta::strip(b);
        }),
        ("strip::webpmeta::strip", |b| {
            let _ = webpmeta::strip(img_parts::Bytes::from(b.to_vec()));
        }),
        ("strip::xmpinfo::packet", |b| {
            let _ = xmpinfo::packet(b);
        }),
        ("strip::ddsinfo::describe", |b| {
            let _ = ddsinfo::describe(b);
        }),
        ("strip::jumbf::is_jumbf_app11", |b| {
            let _ = jumbf::is_jumbf_app11(b);
        }),
    ]
}

/// One structurally valid input per parser above. Without these the gate's shared seeds gave
/// them a 12-byte `RIFF....WEBP` stub and nothing at all for SVG, XMP or a C2PA box, so almost
/// every mutation died at the first check. DDS needs none here: `fuzz::surfaces` seeds a real
/// DXT1 file.
pub(crate) fn seeds() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("strip-heic", heic()),
        ("strip-svg", SVG.as_bytes().to_vec()),
        ("strip-webp", webp()),
        ("strip-xmp", XMP.as_bytes().to_vec()),
        ("strip-app11-jumbf", app11_jumbf()),
    ]
}

/// An XMP packet carrying the AI-provenance tag `xmpinfo::facts` reads.
const XMP: &str = concat!(
    r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>"#,
    r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">"#,
    r#"<rdf:Description xmlns:Iptc4xmpExt="http://iptc.org/std/Iptc4xmpExt/2008-02-29/" "#,
    r#"Iptc4xmpExt:DigitalSourceType="trainedAlgorithmicMedia"/></rdf:RDF></x:xmpmeta>"#,
    r#"<?xpacket end="w"?>"#,
);

/// A document-level title, desc and RDF block to cut, a nested title to keep, and a comment
/// holding a `<title>` that is not markup.
const SVG: &str = concat!(
    r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg" width="8" height="8">"#,
    "<title>t</title><desc>d</desc><metadata><rdf:RDF/></metadata>",
    r#"<g><title>kept</title><rect width="8" height="8"/></g><!-- <title>x</title> --></svg>"#,
);

/// A minimal little-endian TIFF whose one IFD holds Orientation = 6, so the WebP strip also takes
/// the path that writes the orientation back.
const EXIF_ORIENTATION_6: &[u8] = &[
    b'I', b'I', 42, 0, 8, 0, 0, 0, // header, first IFD at 8
    1, 0, // one entry
    0x12, 0x01, 3, 0, 1, 0, 0, 0, 6, 0, 0, 0, // Orientation, SHORT, count 1, value 6
    0, 0, 0, 0, // no next IFD
];

/// A HEIC whose EXIF and XMP items are both long enough to be rewritten in place.
fn heic() -> Vec<u8> {
    crate::isobmff::testutil::synth(&[(1, EXIF_ORIENTATION_6), (2, XMP.as_bytes())], &[]).0
}

/// An extended WebP: VP8X advertising EXIF and XMP, a 1x1 lossless picture, then both chunks.
fn webp() -> Vec<u8> {
    let mut encoded = Vec::new();
    image::DynamicImage::ImageRgba8(image::RgbaImage::new(1, 1))
        .write_to(
            &mut std::io::Cursor::new(&mut encoded),
            image::ImageFormat::WebP,
        )
        .expect("a 1x1 WebP encodes");
    let chunk = |id: &[u8; 4], body: &[u8]| {
        let mut c = id.to_vec();
        c.extend_from_slice(&(body.len() as u32).to_le_bytes());
        c.extend_from_slice(body);
        if body.len() % 2 == 1 {
            c.push(0);
        }
        c
    };
    let mut body = b"WEBP".to_vec();
    // Flags EXIF (0x08) and XMP (0x04), three reserved bytes, then the canvas width and height
    // less one, 24 bits each.
    body.extend(chunk(b"VP8X", &[0x0C, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    // The encoder's picture chunk, after its own 12-byte RIFF header.
    body.extend_from_slice(&encoded[12..]);
    body.extend(chunk(b"EXIF", EXIF_ORIENTATION_6));
    body.extend(chunk(b"XMP ", XMP.as_bytes()));
    let mut riff = b"RIFF".to_vec();
    riff.extend_from_slice(&(body.len() as u32).to_le_bytes());
    riff.extend(body);
    riff
}

/// A JPEG APP11 payload holding a C2PA box: `JP`, box instance, packet sequence, then the JUMBF
/// `LBox` and `TBox`.
fn app11_jumbf() -> Vec<u8> {
    const PAYLOAD: &[u8] = b"c2pa-manifest-store-payload";
    let mut v = b"JP".to_vec();
    v.extend_from_slice(&[0, 1, 0, 0, 0, 1]);
    v.extend_from_slice(&(8 + PAYLOAD.len() as u32).to_be_bytes());
    v.extend_from_slice(b"jumb");
    v.extend_from_slice(PAYLOAD);
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seed that fails its own format's magic/entry check is worthless decoration (see
    /// `container::fuzzseed`'s doc for why this matters) — these targets take raw bytes with no
    /// magic gate at all, so the thing actually worth proving is that every target runs without
    /// panicking on a starter set of inputs (empty, all-zero, all-0xFF), i.e. that `targets()` is
    /// wired up and callable at all.
    #[test]
    fn every_target_survives_a_handful_of_degenerate_inputs() {
        for (name, f) in targets() {
            for input in [&b""[..], &[0u8; 64][..], &[0xFFu8; 64][..]] {
                let result = std::panic::catch_unwind(|| f(input));
                assert!(result.is_ok(), "{name} panicked on a degenerate input");
            }
        }
    }

    /// A seed its parser turns away at the door tests nothing (see `container::fuzzseed`), so
    /// each one must get through to the part of its parser that does the work.
    #[test]
    fn every_seed_reaches_its_parser() {
        let by = |name: &str| {
            seeds()
                .into_iter()
                .find(|(n, _)| *n == name)
                .map(|(_, b)| b)
                .unwrap_or_else(|| panic!("seed {name} missing"))
        };
        let heic = by("strip-heic");
        assert_eq!(
            crate::isobmff::items(&heic).len(),
            2,
            "heic EXIF and XMP items"
        );
        assert!(
            isobmff::strip(&heic).is_some(),
            "heic items rewritten in place"
        );
        let svg = by("strip-svg");
        let cut = svgmeta::strip(&svg).expect("svg strips");
        assert!(cut.len() < svg.len(), "svg metadata cut");
        let webp = webpmeta::strip(img_parts::Bytes::from(by("strip-webp"))).expect("webp strips");
        assert!(
            !webp.windows(4).any(|w| w == b"XMP "),
            "webp XMP chunk dropped"
        );
        assert_eq!(
            crate::decode::exif_orientation(&webp),
            Some(6),
            "webp orientation written back"
        );
        let packet = xmpinfo::packet(&by("strip-xmp")).expect("xmp packet found");
        assert!(!xmpinfo::facts(&packet).is_empty(), "xmp facts read");
        assert!(
            jumbf::is_jumbf_app11(&by("strip-app11-jumbf")),
            "c2pa app11 box"
        );
    }
}
