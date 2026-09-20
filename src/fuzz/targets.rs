#![cfg(test)]

//! The parser entry points the fuzzer hammers: header sniffers and the inner parsers.

use super::*;

/// One parser entry point: a stable name and a closure that must never panic on any input.
/// The closure discards the result — we're asserting robustness, not correctness here.
pub(super) type Target = (&'static str, fn(&[u8]));

/// Every pure-Rust parser that consumes untrusted bytes/headers directly. These are the
/// funnels a corrupt or hostile file reaches first; the corpus pass adds `decode_preview`.
pub(super) fn header_targets() -> Vec<Target> {
    vec![
        ("mkv::keyframe_mini_mkv", |b| {
            let _ = crate::mkv::keyframe_mini_mkv(&mut Cursor::new(b), 0.30);
        }),
        // Added with the VP9 Profile 2/3 tier: this walks the container's own Cues index and
        // then a Cluster's block list, all out of file-supplied offsets, and it runs
        // IN-PROCESS in the shell (only the decode itself is out of process). It was not a
        // target when it shipped.
        ("mkv::vp9_keyframe", |b| {
            let _ = crate::mkv::vp9_keyframe(&mut Cursor::new(b), 0.30);
        }),
        ("mkv::video_codec_id", |b| {
            let _ = crate::mkv::video_codec_id(&mut Cursor::new(b));
        }),
        ("mkv::attached_cover", |b| {
            let _ = crate::mkv::attached_cover(&mut Cursor::new(b));
        }),
        ("mp4::keyframe_mini_mp4", |b| {
            let _ = crate::mp4::keyframe_mini_mp4(&mut Cursor::new(b), 0.30);
        }),
        ("flv::keyframe_mini_mp4", |b| {
            let _ = crate::flv::keyframe_mini_mp4(&mut Cursor::new(b));
        }),
        // The VP6/Sorenson tag walk and its codec gate. Both are IN-PROCESS: only the decode
        // crosses to `st2k flv-frame`, so the bytes that pick the keyframe are parsed inside
        // the shell. Neither was a target when the Flash codecs shipped.
        ("flv::scan_flash_keyframe", |b| {
            let _ = crate::flv::scan_flash_keyframe(b);
        }),
        ("flv::video_codec_id", |b| {
            let _ = crate::flv::video_codec_id(&mut Cursor::new(b));
        }),
        // The MPEG-1/2 demux + intra-picture slicer (2026-09-17): the program-stream walk
        // (pack / system / PES headers with file-supplied lengths) and the start-code scan
        // that picks the GOP and picture both run IN-PROCESS in the shell; only the decode
        // crosses to `st2k mpeg-frame`. A target from the day it landed.
        ("mpeg12::intra_slice_bytes", |b| {
            let _ = crate::mpeg12::intra_slice_bytes(&mut Cursor::new(b), 0.30);
        }),
        ("mpeg12::identify", |b| {
            let _ = crate::mpeg12::identify(&mut Cursor::new(b));
        }),
        ("mp4::video_codec_fourcc", |b| {
            let _ = crate::mp4::video_codec_fourcc(&mut Cursor::new(b));
        }),
        ("mp4::cover_art", |b| {
            let _ = crate::mp4::cover_art(&mut Cursor::new(b));
        }),
        // Issue #32's display-matrix read. New parser over untrusted bytes, running
        // in-process in the shell under `panic = "abort"`, so it belongs here from the day it
        // lands rather than after something goes wrong. Writing it already turned up one
        // latent abort in `box_body`, which this exercises from the other direction.
        ("mp4::display_rotation", |b| {
            let _ = crate::mp4::display_rotation(&mut Cursor::new(b));
        }),
        ("vcodec::cover_art", |b| {
            let _ = crate::vcodec::cover_art(&mut Cursor::new(b));
        }),
        ("vcodec::identify", |b| {
            let _ = crate::vcodec::identify(&mut Cursor::new(b));
        }),
        ("video::is_video_magic", |b| {
            let _ = crate::video::is_video_magic(b);
        }),
        ("container::extract_cover", |b| {
            let _ = crate::container::extract_cover(b);
        }),
        ("container::archive_covers", |b| {
            let prefs = crate::container::select::CoverPrefs::from_settings();
            let _ = crate::container::archive_covers(b, 4, &prefs);
        }),
        ("container::list_archive", |b| {
            let _ = crate::container::list_archive(b);
        }),
        ("container::is_generic_archive_magic", |b| {
            let _ = crate::container::is_generic_archive_magic(b);
        }),
        ("container::looks_like_audio", |b| {
            let _ = crate::container::looks_like_audio(b);
        }),
        ("container::has_head_preview", |b| {
            let _ = crate::container::has_head_preview(b);
        }),
        ("container::audio_art_from_reader", |b| {
            let _ = crate::container::audio_art_from_reader(Cursor::new(b));
        }),
        // The ASF/WMA tag reader (artist/album/title/track), which walks the same
        // GUID-tagged object stream as `audio_art_from_reader`'s WM/Picture path but had no
        // target of its own before this: a mutation that broke the picture parse while
        // leaving the tag walk reachable would never have been exercised here.
        ("container::audio_asf_tags", |b| {
            let _ = crate::container::audio_asf_tags(&mut Cursor::new(b));
        }),
        // Streaming OpenEXR: reads only the chunks a downscale samples, straight off a
        // Read + Seek source. No target existed at all before this.
        ("decode::exr_scaled_from_reader", |b| {
            let _ = crate::decode::exr_scaled_from_reader(Cursor::new(b), 64);
        }),
    ]
}

/// The header targets PLUS every per-format extractor, aimed at directly instead of through
/// `extract_cover`'s magic dispatch. See `container::fuzzseed` for why the indirection matters:
/// a mutation only reaches a parser through the dispatcher if it left that parser's magic
/// alone, which filters out most of the mutations worth trying.
pub(super) fn all_targets() -> Vec<Target> {
    let mut v = header_targets();
    v.extend(crate::container::fuzzseed::targets());
    v.extend(crate::strip::fuzzseed::targets());
    v.extend(inner_targets());
    v
}

/// The parsers that sit BELOW a verifying container, aimed at directly.
///
/// The dispatcher argument in `container::fuzzseed` (a mutation only reaches a parser if it
/// left that parser's magic alone) has a stronger form here. An APK's inner files live in a
/// zip, and a zip CHECKSUMS what it hands out — so a mutation to `AndroidManifest.xml` or
/// `resources.arsc` is rejected by CRC32 one layer above the AXML and arsc parsers, which
/// then never run at all. `apk_mutations_do_not_reach_the_inner_parsers_through_the_zip`
/// measures it: of 20,000 mutations of a valid APK, the number that delivered mutated
/// manifest bytes to the parser is ZERO. Fuzzing `apk::extract` is fuzzing the `zip` crate.
///
/// The FLV case is the same shape, softer: the tag walk rejects a file whose sizes stop
/// adding up, so only mutations that left every length intact reach the Exp-Golomb SPS
/// reader — filtering out most of what is worth trying against bit-level code.
///
/// These entry points hand each parser its own bytes, which is the only way a mutation ever
/// reaches the chunk walk, the string pool, the attribute stride, the resource-id resolver,
/// or the SPS bit reader.
pub(super) fn inner_targets() -> Vec<Target> {
    use crate::container::apk_fuzzapi as apk;
    use crate::decode::cicp_fuzzapi as cicp;
    use crate::decode::dds_fuzzapi as dds;
    use crate::decode::jp2_fuzzapi as jp2;
    use crate::decode::jxl_fuzzapi as jxl;
    use crate::decode::mesh_fuzzapi as mesh;
    use crate::flv::fuzzapi as flv;
    vec![
        ("apk::manifest_icon", apk::manifest_icon),
        ("apk::arsc_resolve", apk::arsc_resolve),
        ("apk::string_pool", apk::string_pool),
        ("apk::type_chunk", apk::type_chunk),
        // The lossless JPEG transform: a hand-written Huffman/scan decoder fed the user's own
        // file by the Rotate/Flip verbs, whose output is written back over that file. It had
        // no target at all; a non-Kraft DHT reached a wrong symbol before build_dec learned to
        // refuse one.
        ("jpegtran::transform", |b| {
            let _ = crate::jpegtran::transform(b, crate::jpegtran::Op::Rot90);
        }),
        // The DDS block decoder, which had NO target at all until 2026-08-19. It indexes a
        // compressed payload using a width, height and mip offset that all come out of the
        // file, and the classic right-click preview tile reaches it IN-PROCESS inside
        // explorer.exe under `panic = "abort"`. The only DDS the harness carried was an
        // eight-byte magic stub, which cannot survive `parse_header`, so every mutation died
        // at the door. Both arms are listed because they are different code: the targeted one
        // selects a mip and takes the block-average fast path, the other expands level 0.
        ("dds::decode_targeted", dds::decode_targeted),
        ("dds::decode_untargeted", dds::decode_untargeted),
        // The 3D-mesh parsers (STL/OBJ/PLY, 2026-08-24). Geometry from untrusted bytes,
        // reached by the isolated thumbnail host for three newly-registered extensions —
        // the exact profile this harness exists for. `parse_mesh_sniffed` covers the
        // sniffers + dispatch; the per-format entries keep a mutation that breaks one
        // sniffer from silently un-fuzzing that parser.
        ("mesh::sniffed", mesh::sniffed),
        ("mesh::binary_stl", mesh::binary_stl),
        ("mesh::ascii_stl", mesh::ascii_stl),
        ("mesh::obj", mesh::obj),
        ("mesh::ply", mesh::ply),
        // The PNG `cICP` chunk walk (2026-09-08): runs ahead of EVERY PNG decode in the
        // thumbnail host, on the raw bytes, so it is fuzzed like the other pre-decode peeks.
        ("cicp::png_cicp", cicp::png_cicp),
        // The JPEG XL tier (2026-09-17). Issue #43: the 1:8 reduced render panicked inside
        // the vendored decoder on a JPEG-transcoded 4:2:0 file and took Explorer down with it,
        // and nothing in the always-on gate had ever fed this tier a byte. Both arms, because
        // the reduced path and the 1:1 path it falls back to are different code.
        ("jxl::reduced", jxl::reduced),
        ("jxl::full", jxl::full),
        // JPEG 2000 codestream walk, reached in-process by the thumbnail host.
        ("jp2::dimensions", jp2::dimensions),
        ("jp2::decode_reduced", jp2::decode_reduced),
        ("flv::sps_dims", flv::sps_dims),
        ("flv::parse_sps", flv::parse_sps),
        // The two MPEG stages on their own bytes: a mutation that breaks the four-byte magic
        // would otherwise never reach the PES-length arithmetic or the GOP/picture walk.
        ("mpeg12::demux_program_stream", |b| {
            let _ = crate::mpeg12::demux_program_stream(b);
        }),
        ("mpeg12::intra_slice", |b| {
            let _ = crate::mpeg12::intra_slice(b, b.len() / 3);
        }),
        // The transport walk on its own bytes, at every stride — including a layout that does
        // NOT describe the buffer, which is what a head that lied about its packet clock
        // hands it. Adaptation-field lengths, PSI section lengths and PMT descriptor lengths
        // are all file-supplied and all walked here.
        ("mpeg12::demux_transport_stream", |b| {
            for stride in [188usize, 192, 204] {
                let _ = crate::mpeg12::demux_transport_stream(
                    b,
                    crate::mpeg12::TsLayout { stride, offset: 0 },
                );
            }
        }),
        (
            "flv::strip_emulation_prevention",
            flv::strip_emulation_prevention,
        ),
    ]
}
