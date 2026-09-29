#![allow(clippy::unwrap_used)]

use super::*;

fn fixture(name: &str) -> String {
    // A plain drive path: Media Foundation's URL resolver does not take the `\\?\` form
    // `canonicalize` returns.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(std::path::Path::parent)
        .unwrap();
    let p = root.join("tests").join("fixtures").join("video").join(name);
    p.to_string_lossy().into_owned()
}

/// Issue #49: an MKV with DTS sound played silently in the Quick preview, because Windows ships
/// no DTS decoder and nothing said so. The probe must name that stream, and must NOT name any
/// stream of the same file with AAC sound (which is how a wrongly-asked output type, or a probe
/// that never reached the audio stream, would read).
#[test]
fn dts_sound_is_named_as_undecodable_and_aac_sound_is_not() {
    if !crate::video::media_foundation_available() {
        eprintln!("NOT MEASURED: no Media Foundation on this machine");
        return;
    }
    let control = missing_decoders(&fixture("h264-aac.mkv"));
    if !control.is_empty() {
        // A Windows without the inbox H.264 or AAC decoder (Server Core, an N edition without
        // the Media Feature Pack) cannot tell us anything about DTS either.
        eprintln!("NOT MEASURED: this Windows lacks the inbox decoders: {control:?}");
        return;
    }
    let dts = missing_decoders(&fixture("h264-dts.mkv"));
    assert_eq!(
        dts,
        vec![MissingDecoder {
            audio: true,
            codec: Some("DTS"),
            store_extension: None,
        }]
    );
}
