#![cfg(test)]

//! Naming a coder for a format nothing can sniff, without letting an
//! extension steer the decode of a file that CAN be sniffed.

use super::*;

/// The premise of the whole fallback: a TIM reaches the end of the tiers undecoded.
/// If this ever starts passing, some tier learned to read TIM and
/// [`decode_by_extension`] is no longer load-bearing for it — check before deleting.
#[test]
fn a_tim_is_declined_by_every_ordinary_tier() {
    assert!(
        decode_preview(&synthetic_tim()).is_err(),
        "TIM has no sniffable signature, so the nameless tiers cannot decode it"
    );
}

#[test]
fn naming_the_coder_decodes_a_tim_to_the_right_colour() {
    if !magick_available() {
        // Loud, because a skip that reads as a pass is worse than no test at all.
        eprintln!("SKIPPED naming_the_coder_decodes_a_tim_to_the_right_colour: no ImageMagick");
        return;
    }
    let img = decode_by_extension(&synthetic_tim(), "tim", None)
        .expect("naming the coder must let ImageMagick read a real TIM");
    assert_eq!((img.width(), img.height()), (4, 4));
    let px = img.to_rgba8();
    let [r, g, b, _] = px.get_pixel(2, 2).0;
    assert!(
        r > 200 && g < 60 && b < 60,
        "the TIM is pure red; got ({r},{g},{b}) — a decode that returns the wrong \
         pixels is the failure this asserts against, not merely a decode that errors"
    );
}

/// The routing gate. Naming a coder skips ImageMagick's own detection, so it is
/// offered ONLY for the formats that cannot be sniffed at all. A sniffable format
/// must never be force-routed, however plausible the extension looks.
#[test]
fn only_unsniffable_formats_are_offered_a_named_coder() {
    for ext in [
        "tim", "rla", "cut", "mac", "pix", "jnx", "scr", "sct", "nef", "mdc", "TIM", ".tim",
    ] {
        assert!(
            extension_has_named_coder(ext),
            "{ext} has a name-selected ImageMagick coder and no other tier"
        );
    }
    for ext in [
        "png", "jpg", "gif", "webp", "psd", "xcf", "bmp", "tiff", "svg", "rle",
    ] {
        assert!(
            !extension_has_named_coder(ext),
            "{ext} is sniffable — forcing a coder would bypass ImageMagick's detection"
        );
    }
}

/// The extension only ever becomes part of a temp file NAME, so it must not be able
/// to steer that name. Refused, not escaped.
#[test]
fn a_crafted_extension_cannot_steer_the_staged_file() {
    let tim = synthetic_tim();
    for ext in [
        "../../evil",
        "a/b",
        r"a\b",
        "",
        "tim.exe",
        "waytoolongextension",
        "ti m",
        "t:m",
    ] {
        assert!(
            decode_by_extension(&tim, ext, None).is_err(),
            "{ext:?} must be refused outright"
        );
    }
}

/// The preview pane's route (a decode capped at the pane's edge, with only the stream's
/// extension to go on) retries by extension exactly as the thumbnail provider does. Without it
/// the pane stayed blank for every format nothing can sniff while Explorer's thumbnail drew
/// it: the big-file gate's blind-spot check found six (CUT, MAC, RLA, SCR, SCT, TIM).
#[test]
fn the_pane_route_names_the_coder_a_decline_needs() {
    if !magick_available() {
        eprintln!("SKIPPED the_pane_route_names_the_coder_a_decline_needs: no ImageMagick");
        return;
    }
    let tim = synthetic_tim();
    assert!(
        decode_preview_capped(&tim, 1024).is_err(),
        "the premise: unnamed, it declines"
    );
    let img = decode_preview_capped_named(&tim, 1024, Some("tim"))
        .expect("named by its extension, the pane's decode reads it");
    assert_eq!((img.width(), img.height()), (4, 4));
    assert!(
        decode_preview_capped_named(&tim, 1024, None).is_err(),
        "no name, no retry"
    );
}
