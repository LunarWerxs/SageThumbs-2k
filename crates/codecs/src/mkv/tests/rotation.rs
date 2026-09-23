#![cfg(test)]

//! Display rotation: the projection roll against the MP4 matrix it must agree with.

use super::*;

/// Issue #32, the Matroska half. Matroska has no display matrix; it stores the same
/// intent as a `ProjectionPoseRoll` float in degrees, and the sign is NOT the same as
/// the MP4 matrix, so the two halves have to be pinned against each other or one
/// container silently rotates the wrong way.
///
/// The numbers are measured, by reading the roll back out of files ffmpeg wrote:
/// `-display_rotation 90` into a `.mkv` writes a roll of +90, and ffprobe reports that
/// file as carrying the identical display matrix as the `.mp4` - which the MP4 side
/// maps to 270 clockwise. So a roll of +90 must produce 270 here, and this asserts it
/// BY CALLING THE MP4 MAPPER rather than by restating its answer.
#[test]
fn mkv_roll_matches_the_mp4_matrix() {
    const ONE: i32 = 1 << 16;
    // (roll degrees written by ffmpeg, the equivalent MP4 matrix a, b, c, d)
    let pairs = [
        (90.0, (0, -ONE, ONE, 0)),
        (180.0, (-ONE, 0, 0, -ONE)),
        (-90.0, (0, ONE, -ONE, 0)),
    ];
    for (roll, (a, b, c, d)) in pairs {
        let via_mkv = rotation_from_roll(roll);
        let via_mp4 = crate::mp4::rotation_from_matrix_for_tests(a, b, c, d);
        assert_eq!(
            via_mkv, via_mp4,
            "a roll of {roll} and its equivalent display matrix must agree"
        );
        assert!(via_mkv.is_some(), "a roll of {roll} must rotate something");
    }
}

/// Upright video, non-quarter turns, and the values a float can actually arrive as.
#[test]
fn only_quarter_turns_rotate_anything() {
    assert_eq!(rotation_from_roll(0.0), None, "upright");
    assert_eq!(
        rotation_from_roll(-0.0),
        None,
        "negative zero is still upright"
    );
    assert_eq!(rotation_from_roll(360.0), None, "a full turn is upright");
    assert_eq!(rotation_from_roll(45.0), None, "not a quarter turn");
    assert_eq!(rotation_from_roll(f64::NAN), None, "NaN");
    assert_eq!(rotation_from_roll(f64::INFINITY), None, "infinity");

    // Written by a muxer, not by hand: a hair off a quarter turn is a quarter turn.
    assert_eq!(rotation_from_roll(90.000000001), Some(270));
    assert_eq!(rotation_from_roll(-89.9999), Some(90));
    // And the wrap-around forms mean the same thing as their in-range twins.
    assert_eq!(rotation_from_roll(270.0), rotation_from_roll(-90.0));
    assert_eq!(rotation_from_roll(-180.0), rotation_from_roll(180.0));
}

/// The descent itself: the roll must come from the VIDEO track's Projection, not from a
/// sibling track and not from a Video element that has no Projection at all.
#[test]
fn the_roll_is_read_from_the_video_tracks_projection() {
    // A subtitle track (type 17) claiming a roll, then the real video track.
    let decoy = track_entry(17, Some(180.0));
    let video = track_entry(1, Some(90.0));
    let mut tracks = decoy.clone();
    tracks.extend_from_slice(&video);
    assert_eq!(video_track_roll(&tracks), Some(90.0));

    // A video track with no Projection is upright, not an error.
    assert_eq!(video_track_roll(&track_entry(1, None)), None);
    // No video track at all.
    assert_eq!(video_track_roll(&decoy), None);
    // Garbage is a clean miss, never a panic (panic = "abort" in the shell host).
    assert_eq!(video_track_roll(&[0xFF; 32]), None);
    assert_eq!(video_track_roll(&[]), None);
}

/// A TrackEntry of `track_type`, optionally carrying Video > Projection > PoseRoll.
fn track_entry(track_type: u8, roll: Option<f64>) -> Vec<u8> {
    let mut entry = vec![0x83, 0x81, track_type]; // TrackType
    if let Some(roll) = roll {
        let mut pose = vec![0x76, 0x75, 0x88]; // ProjectionPoseRoll, 8-byte float
        pose.extend_from_slice(&roll.to_be_bytes());
        let mut proj = vec![0x76, 0x70, 0x80 | pose.len() as u8]; // Projection
        proj.extend_from_slice(&pose);
        let mut video = vec![0xE0, 0x80 | proj.len() as u8]; // Video
        video.extend_from_slice(&proj);
        entry.extend_from_slice(&video);
    }
    let mut out = vec![0xAE, 0x80 | entry.len() as u8]; // TrackEntry
    out.extend_from_slice(&entry);
    out
}
