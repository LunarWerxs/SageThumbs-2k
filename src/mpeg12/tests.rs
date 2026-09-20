#![cfg(test)]

use super::*;
use std::io::Cursor;
use std::path::PathBuf;

fn corpus(name: &str) -> PathBuf {
    crate::testcorpus::dir().join(name)
}

/// The three synthetic shapes all demux to the SAME elementary stream, byte for byte:
/// the program-stream demux is lossless over both PES header layouts, skips audio,
/// private and padding packets, and handles a zero-length PES.
#[test]
fn synthetic_wrappers_demux_to_their_elementary_stream() {
    for mpeg2 in [false, true] {
        let es = fuzzseed::elementary(mpeg2);
        assert_eq!(demux_program_stream(&fuzzseed::mpeg1_system(&es)), es);
        assert_eq!(demux_program_stream(&fuzzseed::mpeg2_program(&es)), es);
        assert_eq!(
            identify(&mut Cursor::new(fuzzseed::mpeg2_program(&es))),
            Some(if mpeg2 { Codec::Mpeg2 } else { Codec::Mpeg1 })
        );
    }
}

/// The slicer hands back the sequence prelude plus exactly ONE intra picture (with its
/// GOP header), never the P-picture, and picks the GOP the mark falls in.
#[test]
fn slicer_returns_one_intra_picture_from_the_marked_gop() {
    for mpeg2 in [false, true] {
        let es = fuzzseed::elementary(mpeg2);
        let first_gop = find_start_code(&es, 0, |c| c == SC_GROUP).unwrap();
        let second_gop = find_start_code(&es, first_gop + 4, |c| c == SC_GROUP).unwrap();
        // Mark at the start: prelude + first GOP + its I-picture.
        let head = intra_slice(&es, 0).expect("slice at 0");
        assert_eq!(&head[..first_gop], &es[..first_gop], "prelude verbatim");
        assert_eq!(
            head[first_gop + 3],
            SC_GROUP,
            "GOP header travels with the picture"
        );
        let pics: Vec<usize> =
            std::iter::successors(find_start_code(&head, 0, |c| c == SC_PICTURE), |&p| {
                find_start_code(&head, p + 4, |c| c == SC_PICTURE)
            })
            .collect();
        assert_eq!(pics.len(), 1, "exactly one picture");
        assert_eq!(picture_type(&head, pics[0]), Some(PIC_I));
        assert!(!head.ends_with(&[0x00, 0x00, 0x01, 0xB7]));
        // Mark inside the second GOP: the same prelude, the second GOP's I-picture.
        let tail = intra_slice(&es, second_gop + 2).expect("slice in GOP 2");
        assert_eq!(&tail[..first_gop], &es[..first_gop]);
        assert_eq!(&tail[first_gop..], &es[second_gop..es.len() - 4]);
        // Mark past the end: still the last GOP, never None.
        assert_eq!(intra_slice(&es, es.len() + 100), Some(tail));
    }
}

/// Through the reader entry point, all three shapes yield a unit the child could decode,
/// and a source that is not MPEG at all yields nothing without a spawn.
#[test]
fn reader_entry_point_covers_every_shape_and_declines_junk() {
    let es = fuzzseed::elementary(true);
    for src in [
        es.clone(),
        fuzzseed::mpeg1_system(&es),
        fuzzseed::mpeg2_program(&es),
        fuzzseed::transport_stream(&es, 188, true),
        fuzzseed::transport_stream(&es, 192, true),
        fuzzseed::transport_stream(&es, 204, false),
    ] {
        for at in [0.0, 0.3, 1.0, f64::NAN, -5.0] {
            let unit = intra_slice_bytes(&mut Cursor::new(&src), at).expect("a unit");
            assert!(unit.starts_with(&[0x00, 0x00, 0x01, 0xB3]));
        }
    }
    assert!(intra_slice_bytes(&mut Cursor::new(b"junk"), 0.3).is_none());
    assert!(intra_slice_bytes(&mut Cursor::new(&[0u8; 256]), 0.3).is_none());
    assert!(mpeg_frame(&mut Cursor::new(b"RIFF....AVI "), 0.3).is_none());
    assert!(shape(b"\x00\x00\x01\xBA").is_some() && shape(b"\x00\x00\x01").is_none());
}

/// Truncations and stomps of every seed must come back `None`/`Some` and never panic
/// (the always-on `crate::fuzz` gate mutates these far harder; this is the smoke test
/// that runs even with fuzzing filtered out).
#[test]
fn truncations_never_panic() {
    let es = fuzzseed::elementary(true);
    for src in [
        es.clone(),
        fuzzseed::mpeg1_system(&es),
        fuzzseed::mpeg2_program(&es),
    ] {
        for n in 0..src.len() {
            let _ = intra_slice_bytes(&mut Cursor::new(&src[..n]), 0.3);
            let _ = identify(&mut Cursor::new(&src[..n]));
            let _ = demux_program_stream(&src[..n]);
            let _ = intra_slice(&src[..n], n / 2);
        }
    }
    // The transport walk gets the same treatment, at every stride and against every
    // layout (including a layout that does NOT match the bytes, which is what a head
    // that lied would hand it), in steps so a multi-packet seed stays a quick test.
    for stride in TS_STRIDES {
        let ts = fuzzseed::transport_stream(&es, stride, true);
        for n in (0..ts.len()).step_by(7) {
            let _ = intra_slice_bytes(&mut Cursor::new(&ts[..n]), 0.3);
            let _ = identify(&mut Cursor::new(&ts[..n]));
            for other in TS_STRIDES {
                let _ = demux_transport_stream(
                    &ts[..n],
                    TsLayout {
                        stride: other,
                        offset: 0,
                    },
                );
            }
        }
    }
    assert!(packet_payload(&[]).is_none());
    assert!(
        packet_payload(&[TS_SYNC, 0x80, 0x00, 0x10]).is_none(),
        "error bit"
    );
    assert!(
        packet_payload(&[TS_SYNC, 0x00, 0x00, 0x90]).is_none(),
        "scrambled"
    );
    assert!(
        packet_payload(&[TS_SYNC, 0x00, 0x00, 0x20, 0x00]).is_none(),
        "no payload"
    );
    assert!(
        packet_payload(&[TS_SYNC, 0x00, 0x00, 0x30, 0xFF]).is_none(),
        "af overruns"
    );
    assert!(section_body(&[0x00, 0xB0, 0x02, 0x00], 5).is_none());
    // A stride nobody can reach through `ts_layout` must not be walked at all: a zero
    // stride would otherwise advance the packet loops by nothing and hang the shell.
    let ts = fuzzseed::transport_stream(&es, 188, true);
    for stride in [0usize, 1, 187, 189, 4096] {
        assert!(
            demux_transport_stream(&ts, TsLayout { stride, offset: 0 }).is_empty(),
            "stride {stride} is not a transport geometry"
        );
    }
    // A PES whose declared length overruns the buffer, and one whose header claims more
    // header bytes than exist.
    assert!(pes_header_len(&[0x80, 0x80, 0xFF]).is_none());
    assert_eq!(pes_header_len(&[0xFF, 0xFF, 0x0F, 0xAA]), Some(3));
    assert!(pes_header_len(&[0xFF, 0xFF, 0x21]).is_none());
    assert_eq!(
        demux_program_stream(&[0, 0, 1, 0xE0, 0xFF, 0xFF, 0x0F, 0x42]),
        vec![0x42]
    );
}

/// The real corpus files, every shape this tier targets: MPEG-2 ES (`sample.mpg` /
/// `sample.m2v`), MPEG-1 system stream (`sample.mpeg`), MPEG-2 program streams
/// (`sample.vob`, `real.m2v`, `real.vob`), the real MPEG-1 elementary stream
/// (`real.m1v`) and — since 2026-09-17 — MPEG-2 inside a TRANSPORT stream at all three
/// packet strides (`sample.ts` 188, `sample.m2ts` 192, `sample.mts`, and the corpus's
/// `real.mpg`, which is a 188-byte transport stream wearing a program-stream name).
/// Each yields a unit that starts with a sequence header and holds one intra picture.
/// Corpus-gated, like every sample-backed test.
#[test]
fn corpus_streams_slice_to_one_intra_picture() {
    let mut seen = 0;
    for (name, codec) in [
        ("sample.mpg", Codec::Mpeg2),
        ("sample.m2v", Codec::Mpeg2),
        ("sample.mpeg", Codec::Mpeg1),
        ("sample.vob", Codec::Mpeg2),
        ("real.m2v", Codec::Mpeg2),
        ("real.vob", Codec::Mpeg2),
        ("real.m1v", Codec::Mpeg1),
        ("real-vcd.mpg", Codec::Mpeg1),
        ("real-es.m2v", Codec::Mpeg2),
        ("sample.ts", Codec::Mpeg2),
        ("sample.m2ts", Codec::Mpeg2),
        ("sample.mts", Codec::Mpeg2),
        ("real.mpg", Codec::Mpeg2),
    ] {
        let Ok(bytes) = std::fs::read(corpus(name)) else {
            continue;
        };
        seen += 1;
        let unit = intra_slice_bytes(&mut Cursor::new(&bytes), 0.30)
            .unwrap_or_else(|| panic!("{name}: no intra unit"));
        assert!(unit.starts_with(&[0x00, 0x00, 0x01, 0xB3]), "{name}");
        assert!(unit.len() <= MPEG_INPUT_CAP, "{name}");
        // The unit holds ONE frame. That is one picture for a frame-coded stream, and a
        // PAIR for a field-coded one, where the second field is legitimately coded as P
        // (predicted from the first field of the same frame) — `unit_bounds` takes the
        // partner field on purpose, because the two fields ARE one frame to the decoder.
        // The corpus's `real.mpg`, a field-coded DVB recording, is the sample that
        // proves it; every earlier sample was frame-coded, so this assertion used to
        // read "every picture is intra" and would have failed the moment one arrived.
        let mut pictures = Vec::new();
        let mut pos = 0;
        while let Some(p) = find_start_code(&unit, pos, |c| c == SC_PICTURE) {
            pictures.push((picture_type(&unit, p), picture_structure(&unit, p)));
            pos = p + 4;
        }
        assert!(
            (1..=2).contains(&pictures.len()),
            "{name}: {} pictures in the unit",
            pictures.len()
        );
        assert!(
            matches!(pictures[0].0, Some(PIC_I | PIC_D)),
            "{name}: the first picture is not intra"
        );
        if pictures.len() == 2 {
            assert!(
                pictures[0].1.is_some_and(|s| s != STRUCT_FRAME),
                "{name}: a second picture is only allowed as the partner FIELD"
            );
        }
        assert_eq!(identify(&mut Cursor::new(&bytes)), Some(codec), "{name}");
    }
    // The H.264 transport streams in the same corpus are recognised as transport
    // streams and then DECLINED, because their program map names stream_type 0x1B and
    // nothing in them is MPEG-1/2 video. Media Foundation decodes those in process and
    // runs long before this tier; an answer here would be the wrong one.
    for name in ["real.ts", "real.m2ts", "real.mts"] {
        let Ok(bytes) = std::fs::read(corpus(name)) else {
            continue;
        };
        assert!(
            matches!(shape(&bytes), Some(Shape::TransportStream(_))),
            "{name}: should still be recognised as a transport stream"
        );
        assert!(
            intra_slice_bytes(&mut Cursor::new(&bytes), 0.30).is_none(),
            "{name}: H.264 is not ours"
        );
        assert!(identify(&mut Cursor::new(&bytes)).is_none(), "{name}");
    }
    if seen > 0 {
        eprintln!("corpus_streams_slice_to_one_intra_picture: {seen} corpus streams sliced");
    }
}

/// The packet clock is read correctly at all three strides, from a head alone, and the
/// M2TS geometry is not mistaken for a plain one (its sync bytes also sit 188 apart
/// once, four bytes in — which is exactly why the probe demands four in a row).
#[test]
fn every_transport_geometry_is_recognised_and_demuxes_to_its_elementary_stream() {
    for mpeg2 in [false, true] {
        let es = fuzzseed::elementary(mpeg2);
        for (stride, offset) in [(188, 0), (192, 4), (204, 0)] {
            for tables in [true, false] {
                let ts = fuzzseed::transport_stream(&es, stride, tables);
                assert_eq!(
                    shape(&ts),
                    Some(Shape::TransportStream(TsLayout { stride, offset })),
                    "stride {stride}, tables {tables}"
                );
                let out = demux_transport_stream(&ts, TsLayout { stride, offset });
                assert_eq!(out, es, "stride {stride}, tables {tables}");
                let unit = intra_slice_bytes(&mut Cursor::new(&ts), 0.30)
                    .unwrap_or_else(|| panic!("stride {stride}: no unit"));
                assert!(unit.starts_with(&[0x00, 0x00, 0x01, 0xB3]));
                assert_eq!(
                    identify(&mut Cursor::new(&ts)),
                    Some(if mpeg2 { Codec::Mpeg2 } else { Codec::Mpeg1 })
                );
            }
        }
    }
}

/// A window cut at an arbitrary byte — which is what `intra_slice_bytes` hands the demux
/// for any file bigger than the read window — resynchronises on the packet clock instead
/// of losing the stream.
#[test]
fn a_transport_window_that_starts_mid_packet_resynchronises() {
    let es = fuzzseed::elementary(true);
    for stride in TS_STRIDES {
        let ts = fuzzseed::transport_stream(&es, stride, true);
        let layout = TsLayout { stride, offset: 0 };
        for cut in [1usize, 7, 93, stride - 1, stride + 5] {
            let out = demux_transport_stream(&ts[cut..], layout);
            assert!(
                !out.is_empty()
                    && es
                        .windows(4)
                        .any(|w| out.starts_with(w) || out.ends_with(w)),
                "stride {stride}, cut {cut}: lost the stream"
            );
            assert!(
                find_start_code(&out, 0, |c| c == SC_SEQUENCE).is_some(),
                "stride {stride}, cut {cut}: no sequence header survived"
            );
        }
    }
}

/// A transport stream whose program map names only H.264 is declined, and so is one
/// whose video PID is scrambled — neither may produce bytes for the decoder.
#[test]
fn a_transport_stream_that_is_not_ours_yields_nothing() {
    let es = fuzzseed::elementary(true);
    let layout = TsLayout {
        stride: 188,
        offset: 0,
    };
    // A program map whose only elementary streams are H.264 (0x1B) and AC-3 (0x81)
    // names no video of ours. Found by its own bytes rather than by arithmetic, so a
    // change to the seed's packing cannot quietly turn this into a test of nothing.
    let mut h264 = fuzzseed::transport_stream(&es, 188, true);
    let sec = h264
        .windows(5)
        .position(|w| w == [0x02, 0xB0, 0x17, 0x00, 0x01])
        .expect("the PMT section this test rewrites");
    assert_eq!(h264[sec + 12], 0x02, "the stream_type byte moved");
    h264[sec + 12] = 0x1B;
    h264[sec + 17] = 0x81;
    assert!(pmt_video_pid(&h264[sec - 1..], true).is_none());
    // A stream carrying a video PES that holds no MPEG sequence header at all — which
    // is what a real H.264 transport stream looks like to the PES sniff — is demuxed
    // and then DECLINED by the slicer rather than handed to the decoder.
    let junk = fuzzseed::transport_stream(&vec![0x5Au8; 600], 188, false);
    assert!(!demux_transport_stream(&junk, layout).is_empty());
    assert!(intra_slice_bytes(&mut Cursor::new(&junk), 0.30).is_none());
    // A scrambled video PID: transport_scrambling_control '10' on every packet of it.
    let mut scrambled = fuzzseed::transport_stream(&es, 188, false);
    for p in (0..scrambled.len() / 188).map(|n| n * 188) {
        if scrambled[p + 1] & 0x1F == 0x01 && scrambled[p + 2] == 0x00 {
            scrambled[p + 3] |= 0x80;
        }
    }
    assert!(demux_transport_stream(&scrambled, layout).is_empty());
}

/// A real stream decodes WHEN the helper is there, and declines cleanly when it is not.
/// Both are correct (see `vp9::tests` for why the assertion is conditional).
#[test]
fn a_real_stream_decodes_when_the_helper_exists() {
    let Ok(bytes) = std::fs::read(corpus("sample.mpeg")) else {
        return;
    };
    let got = mpeg_frame(&mut Cursor::new(&bytes), 0.30);
    match crate::sibling_of_dll(crate::CLI_EXE) {
        Some(exe) if exe.exists() => {
            let img = got.expect("the helper is present, so MPEG-1 must decode");
            assert_eq!((img.width(), img.height()), (640, 360));
        }
        _ => assert!(
            got.is_none(),
            "without the helper this must decline cleanly"
        ),
    }
}
