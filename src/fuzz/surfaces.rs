#![cfg(test)]

//! Seeds for the newer surfaces (meshes, EXR, APK) and the sessions that prove each seed reaches its parser.

use super::*;

/// Seeds for the surfaces this release added: the VP9 container walk, the Flash-codec tag
/// walk, and the APK / H.264 sub-parsers that a verifying container hides from any whole-file
/// mutation. Shared by the always-on gate and the deep session so the two cannot diverge.
/// A 12-triangle unit cube as binary STL — a structurally VALID mesh so mutations reach
/// the vertex reads, not just the length-equation sniff.
pub(super) fn mesh_seed_stl() -> Vec<u8> {
    let mut out = vec![0u8; 80];
    out.extend_from_slice(&12u32.to_le_bytes());
    let q: [[f32; 3]; 8] = [
        [0., 0., 0.],
        [1., 0., 0.],
        [1., 1., 0.],
        [0., 1., 0.],
        [0., 0., 1.],
        [1., 0., 1.],
        [1., 1., 1.],
        [0., 1., 1.],
    ];
    let faces: [[usize; 3]; 12] = [
        [0, 1, 2],
        [0, 2, 3],
        [4, 5, 6],
        [4, 6, 7],
        [0, 1, 5],
        [0, 5, 4],
        [3, 2, 6],
        [3, 6, 7],
        [0, 3, 7],
        [0, 7, 4],
        [1, 2, 6],
        [1, 6, 5],
    ];
    for f in faces {
        out.extend_from_slice(&[0u8; 12]);
        for &vi in &f {
            for c in q[vi] {
                out.extend_from_slice(&c.to_le_bytes());
            }
        }
        out.extend_from_slice(&[0u8; 2]);
    }
    out
}

pub(super) fn mesh_seed_obj() -> Vec<u8> {
    b"v 0 0 0
v 1 0 0
v 0.5 1 0
v 0.5 0.5 1
f 1 2 3
f 1 2 4
f 2 3 4
f 1 3 4
"
    .to_vec()
}

pub(super) fn mesh_seed_ply() -> Vec<u8> {
    b"ply
format ascii 1.0
element vertex 4
property float x
property float y
property float z
element face 4
property list uchar int vertex_indices
end_header
0 0 0
1 0 0
0.5 1 0
0.5 0.5 1
3 0 1 2
3 0 1 3
3 1 2 3
3 0 2 3
"
    .to_vec()
}

/// A minimal uncompressed OpenEXR scanline image, built with the `exr` crate's own writer —
/// the same technique `exrscale::tests::ramp_exr` uses for the non-fuzz downscale tests.
/// Hand-assembling a valid attribute list, offset table and chunk layout byte-by-byte is
/// exactly the class of format this harness exists to stress rather than to reimplement, and
/// a seed built by hand risks being wrong in a way that only bounces off the real parser's
/// magic/header checks — worse than no seed at all. Uncompressed so a mutation lands on a raw
/// scanline instead of a compressed block's own internal framing.
pub(super) fn synthetic_exr() -> Vec<u8> {
    use exr::prelude::{Encoding, Image, SpecificChannels, Vec2, WritableImage};
    let (w, h) = (8usize, 6usize);
    let pixels = SpecificChannels::rgba(|p: Vec2<usize>| {
        (p.x() as f32 * 0.1, p.y() as f32 * 0.1, 0.25f32, 1.0f32)
    });
    let image = Image::from_encoded_channels((w, h), Encoding::UNCOMPRESSED, pixels);
    let mut out = Cursor::new(Vec::new());
    if image.write().non_parallel().to_buffered(&mut out).is_err() {
        return Vec::new();
    }
    out.into_inner()
}

pub(super) fn new_surface_seeds() -> Vec<(&'static str, Vec<u8>)> {
    use crate::container::fuzzseed as fs;
    const ICON: &str = "res/mipmap/ic_launcher.png";
    vec![
        ("webm-vp9", synthetic_webm_vp9()),
        ("flv-sorenson", synthetic_flash_flv(2)),
        ("flv-vp6", synthetic_flash_flv(4)),
        // The three below are INNER files, handed over as seeds in their own right rather
        // than wrapped in the zip that would checksum them out of reach.
        ("axml-raw", fs::apk_axml(ICON)),
        ("arsc-raw", fs::apk_arsc(ICON)),
        ("respool-raw", fs::apk_pool_utf8(&["", "application", ICON])),
        ("stl-cube", mesh_seed_stl()),
        ("obj-tetra", mesh_seed_obj()),
        ("ply-tetra", mesh_seed_ply()),
        ("h264-avcc", h264_avcc()),
        ("h264-sps", H264_SPS.to_vec()),
        // The JPEG XL shapes, from the committed regression fixtures rather than a synthetic
        // stub: a JPEG-transcoded 4:2:0 file (the exact shape that crashed Explorer in issue
        // #43, which is 4:2:0 chroma through the 1:8 render), its 4:2:2 twin, a modular file
        // with an embedded colour profile, and an HDR PQ/BT.2020 one. Four codepaths through
        // one tier, all tiny, all real.
        (
            "jxl-jpeg420",
            include_bytes!("../../tests/fixtures/jxl/jpeg420_transcode.jxl").to_vec(),
        ),
        (
            "jxl-jpeg422",
            include_bytes!("../../tests/fixtures/jxl/jpeg422_transcode.jxl").to_vec(),
        ),
        (
            "jxl-modular-icc",
            include_bytes!("../../tests/fixtures/jxl/adobergb_modular.jxl").to_vec(),
        ),
        (
            "jxl-hdr-pq",
            include_bytes!("../../tests/fixtures/jxl/scene-pq2020.jxl").to_vec(),
        ),
        // The MPEG-1/2 shapes `mpeg12` walks: a bare MPEG-2 and MPEG-1 elementary stream (two
        // GOPs, an I and a P picture, extensions), the MPEG-1 SYSTEM wrapping (MPEG-1 pack +
        // PES headers, an audio and a padding packet) and the MPEG-2 PROGRAM wrapping
        // (MPEG-2 pack + PES headers, a private-stream packet, a zero-length PES).
        ("mpeg2-es", crate::mpeg12::fuzzseed::elementary(true)),
        ("mpeg1-es", crate::mpeg12::fuzzseed::elementary(false)),
        (
            "mpeg1-ss",
            crate::mpeg12::fuzzseed::mpeg1_system(&crate::mpeg12::fuzzseed::elementary(false)),
        ),
        (
            "mpeg2-ps",
            crate::mpeg12::fuzzseed::mpeg2_program(&crate::mpeg12::fuzzseed::elementary(true)),
        ),
        // The TRANSPORT wrapping at both interesting geometries: 188-byte broadcast packets
        // with a PAT and a PMT to walk, and M2TS's 192-byte stride with NO tables, so the
        // video-PES sniff and the adaptation-field stuffing are mutated too.
        (
            "mpeg2-ts",
            crate::mpeg12::fuzzseed::transport_stream(
                &crate::mpeg12::fuzzseed::elementary(true),
                188,
                true,
            ),
        ),
        (
            "mpeg2-m2ts",
            crate::mpeg12::fuzzseed::transport_stream(
                &crate::mpeg12::fuzzseed::elementary(true),
                192,
                false,
            ),
        ),
        // The audio-shaped seeds `audio_art_from_reader` had NONE of before: WAV/AIFF
        // PCM (drives `container::waveform`'s chunk walk) and ASF/WMA (drives
        // `container::audio::asf`'s GUID-object walk + `WM/Picture` parse).
        // Structurally valid DDS surfaces, so a mutation reaches the block reader instead of
        // dying at the header. One per block family that parses differently: DXT1's
        // punch-through alpha, DXT5's interpolated alpha, BC7's mode/partition parsing, and a
        // real mip CHAIN, whose level offsets accumulate from file-supplied sizes.
        (
            "dds-dxt1",
            crate::decode::dds_fuzzapi::seed(b"DXT1", 0, 64, 64, 1),
        ),
        (
            "dds-dxt5",
            crate::decode::dds_fuzzapi::seed(b"DXT5", 0, 64, 64, 1),
        ),
        (
            "dds-bc7",
            crate::decode::dds_fuzzapi::seed(b"DX10", 98, 64, 64, 1),
        ),
        (
            "dds-bc6h",
            crate::decode::dds_fuzzapi::seed(b"DX10", 95, 64, 64, 1),
        ),
        (
            "dds-mips",
            crate::decode::dds_fuzzapi::seed(b"DXT1", 0, 128, 128, 5),
        ),
        ("wav-pcm", synthetic_wav()),
        ("aiff-pcm", synthetic_aiff()),
        ("asf-wm-picture", synthetic_asf()),
        ("exr-scanline", synthetic_exr()),
        ("jp2-codestream", crate::decode::jp2_fuzzapi::seed()),
    ]
}

/// A seed its own parser rejects is worse than no seed (the fuzzer mutates it happily while
/// every iteration dies at the first gate) — same rule `container::fuzzseed` enforces, and the
/// reason its `every_seed_reaches_its_parser` is described there as load-bearing.
///
/// Each assertion below is one seed proving it gets past the front door into the code being
/// fuzzed. `arsc-raw` is the one asserted a step short, and deliberately: the seed's manifest
/// takes the direct-string rung, so nothing in a PRISTINE table is reachable by resolution —
/// the table exists for the mutations (one flipped dataType byte turns the manifest attribute
/// into a reference). Asserting it parses is the strongest claim that is actually true.
#[test]
pub(super) fn every_new_surface_seed_reaches_its_parser() {
    assert!(
        crate::flv::keyframe_mini_mp4(&mut Cursor::new(synthetic_flv())).is_some(),
        "synthetic FLV seed no longer reaches flv::keyframe_mini_mp4's happy path"
    );
    // The VP9 container walk: codec gate, Cues index, cluster, SimpleBlock keyframe flag.
    let webm = synthetic_webm_vp9();
    assert_eq!(
        crate::mkv::video_codec_id(&mut Cursor::new(&webm[..])).as_deref(),
        Some("V_VP9"),
        "webm-vp9 seed must declare a VP9 track or vp9_keyframe returns before parsing"
    );
    assert!(
        crate::mkv::vp9_keyframe(&mut Cursor::new(&webm[..]), 0.30).is_some(),
        "webm-vp9 seed no longer reaches a keyframe SimpleBlock"
    );
    // The Flash tag walk, both codecs. A keyframe AFTER an inter frame, so the walk has to
    // keep looking rather than stopping on the first video tag.
    for (label, id) in [("flv-sorenson", 2u8), ("flv-vp6", 4u8)] {
        let flv = synthetic_flash_flv(id);
        assert_eq!(
            crate::flv::video_codec_id(&mut Cursor::new(&flv[..])),
            Some(id),
            "{label} seed must present codec {id} as its first video tag"
        );
        assert!(
            matches!(
                crate::flv::scan_flash_keyframe(&flv),
                crate::flv::FlashScan::Keyframe(..)
            ),
            "{label} seed no longer reaches scan_flash_keyframe's keyframe arm"
        );
    }
    // The inner APK parsers, on their own bytes.
    use crate::container::apk_fuzzapi as apk;
    use crate::container::fuzzseed as fs;
    const ICON: &str = "res/mipmap/ic_launcher.png";
    assert_eq!(
        apk::manifest_icon_path(&fs::apk_axml(ICON)).as_deref(),
        Some(ICON),
        "axml-raw seed no longer resolves its icon attribute"
    );
    assert!(
        apk::arsc_parses(&fs::apk_arsc(ICON)),
        "arsc-raw seed no longer parses as a resource table"
    );
    // And the H.264 bit readers, on theirs.
    assert_eq!(
        crate::flv::fuzzapi::sps_dims_ret(&h264_avcc()),
        Some((64, 48)),
        "h264-avcc seed no longer parses to its known 64x48 geometry"
    );
    // The JPEG XL seeds: each fixture must actually DECODE through the tier being fuzzed,
    // or the mutator spends every iteration dying at the container header. The 4:2:0 one is
    // issue #43's file, so this doubles as the always-on regression for that crash.
    for (label, name) in [
        ("jxl-jpeg420", "jpeg420_transcode.jxl"),
        ("jxl-jpeg422", "jpeg422_transcode.jxl"),
        ("jxl-modular-icc", "adobergb_modular.jxl"),
        ("jxl-hdr-pq", "scene-pq2020.jxl"),
    ] {
        let bytes = std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("jxl")
                .join(name),
        )
        .unwrap_or_else(|e| panic!("{label}: read {name}: {e}"));
        assert!(
            crate::decode::decode_preview(&bytes).is_ok(),
            "{label} seed no longer decodes through the JPEG XL tier"
        );
    }
    // The MPEG-1/2 seeds: every wrapping demuxes to its elementary stream and slices to a
    // unit that starts with the sequence header, through the same reader entry point the
    // cascades call.
    for (label, seed) in new_surface_seeds() {
        if !label.starts_with("mpeg") {
            continue;
        }
        let unit = crate::mpeg12::intra_slice_bytes(&mut Cursor::new(&seed[..]), 0.30)
            .unwrap_or_else(|| panic!("{label} seed no longer reaches an intra picture"));
        assert!(
            unit.starts_with(&[0, 0, 1, 0xB3]),
            "{label} unit must open with a sequence header"
        );
        assert!(
            crate::mpeg12::identify(&mut Cursor::new(&seed[..])).is_some(),
            "{label} seed no longer identifies its codec"
        );
    }
    // The audio seeds, all via the same entry point `header_targets` fuzzes.
    assert!(
        crate::container::audio_art_from_reader(Cursor::new(synthetic_wav())).is_some(),
        "wav-pcm seed no longer reaches the waveform renderer"
    );
    assert!(
        crate::container::audio_art_from_reader(Cursor::new(synthetic_aiff())).is_some(),
        "aiff-pcm seed no longer reaches the waveform renderer"
    );
    assert!(
        crate::container::audio_art_from_reader(Cursor::new(synthetic_asf())).is_some(),
        "asf-wm-picture seed no longer reaches asf_cover's WM/Picture parse"
    );
    assert!(
        crate::container::audio_asf_tags(&mut Cursor::new(synthetic_asf())).is_some(),
        "asf-wm-picture seed no longer reaches asf_tags's object walk"
    );
    assert!(
        crate::decode::exr_scaled_from_reader(Cursor::new(synthetic_exr()), 64).is_ok(),
        "exr-scanline seed no longer reaches exrscale::decode_scaled"
    );
    assert!(
        crate::decode::jp2_fuzzapi::seed_decodes(),
        "jp2-codestream seed no longer reaches the JPEG 2000 decoder"
    );
}

/// The measurement behind [`inner_targets`]: mutating an APK does not fuzz the APK parsers.
///
/// A zip CHECKSUMS what it hands out, so a mutation landing in `AndroidManifest.xml` is
/// rejected by CRC32 before `manifest_icon` ever sees it — the parser is not hardened by that
/// run, it is simply never called. This counts, over a real mutation run, how many mutants
/// delivered MUTATED manifest bytes to the parser.
///
/// It asserts the number rather than describing it so the comment cannot go stale: if a future
/// `zip` release stopped verifying, this fails loudly and the direct entry points could be
/// reconsidered instead of quietly duplicating coverage.
#[test]
pub(super) fn apk_mutations_do_not_reach_the_inner_parsers_through_the_zip() {
    let apk = crate::container::fuzzseed::seeds()
        .into_iter()
        .find(|(n, _)| *n == "apk")
        .map(|(_, b)| b)
        .expect("apk seed");
    let pristine = read_zip_entry(&apk, "AndroidManifest.xml").expect("pristine manifest reads");

    let mut rng = Rng::new(0x00A9_1CE5_D00D_F00D);
    let (mut opened, mut emptied, mut real_structure) = (0u32, 0u32, 0u32);
    for _ in 0..20_000 {
        let m = mutate(&mut rng, &apk);
        let Some(bytes) = read_zip_entry(&m, "AndroidManifest.xml") else {
            continue;
        };
        opened += 1;
        if bytes == pristine {
            continue;
        }
        // The one way past the checksum, and it carries nothing: the central directory stores
        // crc32 and the two size fields ADJACENT, so a single zero-a-region mutation can null
        // all three at once. The entry then declares zero bytes, the reader returns an empty
        // buffer, and CRC32 of nothing really is 0 — a legitimate pass over a legitimately
        // empty file. `manifest_icon` rejects that at its first length check, which the
        // degenerate-buffer seeds already covered.
        if bytes.is_empty() {
            emptied += 1;
        } else {
            real_structure += 1;
        }
    }
    eprintln!(
        "apk zip reach: 20000 mutants, {opened} manifests read back, \
         {emptied} emptied by a nulled size/crc triple, \
         {real_structure} carrying mutated STRUCTURE"
    );
    assert_eq!(
        real_structure, 0,
        "the zip layer stopped verifying entry contents — mutating an APK now DOES deliver \
         mutated structure to the AXML parser, so `inner_targets` may be redundant rather \
         than load-bearing"
    );
}

/// Read one zip entry fully, or `None` if the archive, the entry, or its CRC is bad. The
/// operation `apk::manifest_icon_path` performs, without reaching into `container::zipfmt`.
pub(super) fn read_zip_entry(archive: &[u8], name: &str) -> Option<Vec<u8>> {
    use std::io::Read as _;
    let mut zip = zip::ZipArchive::new(Cursor::new(archive)).ok()?;
    let mut entry = zip.by_name(name).ok()?;
    let mut buf = Vec::new();
    entry.read_to_end(&mut buf).ok()?;
    Some(buf)
}

/// A DELIBERATE deep fuzz session over the parsers this release added — not a gate.
///
/// The always-on run is tuned to stay under ten seconds of every `cargo test`, which buys
/// breadth (every parser, every seed) at the cost of depth: one mutation per iteration, a
/// 600 ms ceiling per pair. That combination cannot find a bug needing two coordinated
/// corruptions, and it gives a brand-new parser the same handful of milliseconds it gives one
/// that has been fuzzed on every commit for months.
///
/// This runs the twelve NEW entry points against their own seeds plus any real corpus samples,
/// with stacked mutations and cross-format grafts, round-robin so no pair starves, until a
/// wall-clock budget expires. Round-robin rather than sequential specifically so a cheap
/// parser cannot consume the session while an expensive one goes unvisited.
///
/// ```text
/// ST2K_FUZZ_SECS=600 cargo test --lib fuzz::deep_session -- --ignored --nocapture
/// ```
#[test]
#[ignore = "deliberate deep fuzz session (minutes); set ST2K_FUZZ_SECS and run with --ignored"]
pub(super) fn deep_session_over_the_new_parsers() {
    let secs: u64 = std::env::var("ST2K_FUZZ_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(180);

    let mut seeds = new_surface_seeds();
    // Real samples where the corpus has them: a synthetic seed is a scaffold this code wrote
    // for itself, so it can only ever contain structures this code already thought of.
    let corpus = crate::testcorpus::dir();
    for name in [
        "sample.apk",
        "sample.xapk",
        "sample-vp6.flv",
        "sample.flv",
        "sample-vp9p2.webm",
        "sample-vp9p3.webm",
        "sample.webm",
        // The MPEG shapes, beside their FLV/VP9 siblings: an MPEG-1 system stream and an
        // MPEG-2 program stream (both demuxed here, in the shell, out of file-supplied PES
        // lengths) and the two bare elementary streams. The real files matter more than the
        // synthetic seeds for this tier — `mpeg1_system`/`mpeg2_program` are scaffolds this
        // module wrote for itself, so they only ever hold packet layouts it already thought of.
        "sample.mpeg",
        "sample.vob",
        "real-vcd.mpg",
        "real.m1v",
        "real-es.m2v",
        // The transport shapes, for the same reason: real packet clocks, real adaptation
        // fields, real PAT/PMT sections, a second program's tables, and in `real.mpg` a
        // FIELD-coded stream. `real.ts` is H.264 in a transport stream — the demux has to
        // walk it all and hand back nothing, which is its own worth fuzzing.
        "sample.ts",
        "sample.m2ts",
        "real.mpg",
        "real.ts",
    ] {
        if let Ok(mut bytes) = std::fs::read(corpus.join(name)) {
            bytes.truncate(512 * 1024);
            if !bytes.is_empty() {
                seeds.push((Box::leak(name.to_string().into_boxed_str()), bytes));
            }
        }
    }
    // Every seed is also a graft donor for every other, which is how an arsc chunk ends up
    // inside an AXML body and a VP9 block ends up inside an FLV tag.
    let donors: Vec<&[u8]> = seeds.iter().map(|(_, b)| b.as_slice()).collect();

    let mut targets = inner_targets();
    let before_header_targets = targets.len();
    for t in header_targets() {
        if matches!(
            t.0,
            "mkv::vp9_keyframe"
                | "flv::scan_flash_keyframe"
                | "flv::video_codec_id"
                | "mpeg12::intra_slice_bytes"
        ) {
            targets.push(t);
        }
    }
    // The four names above are matched by string, not carried as a slice from a shared
    // constant, so a rename of any one of them would silently drop that target from this
    // session instead of failing to compile. Assert the count instead of trusting the match.
    const EXPECTED_NAMED_HEADER_TARGETS: usize = 4;
    assert_eq!(
        targets.len() - before_header_targets,
        EXPECTED_NAMED_HEADER_TARGETS,
        "deep_session_over_the_new_parsers expected {EXPECTED_NAMED_HEADER_TARGETS} header \
         targets by name (mkv::vp9_keyframe, flv::scan_flash_keyframe, flv::video_codec_id, \
         mpeg12::intra_slice_bytes) — a rename dropped one silently"
    );
    targets.push(("container::extract_cover", |b| {
        let _ = crate::container::extract_cover(b);
    }));
    targets.push(("apk::extract", crate::container::apk_fuzzapi::extract));

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut counts = vec![0u64; targets.len()];
    let mut failures: Vec<String> = Vec::new();
    let mut rounds = 0u64;

    eprintln!(
        "deep fuzz: {} targets x {} seeds, {secs}s budget",
        targets.len(),
        seeds.len()
    );
    with_quiet_panics(|| {
        'session: loop {
            rounds += 1;
            for (ti, &target) in targets.iter().enumerate() {
                for (si, (label, seed)) in seeds.iter().enumerate() {
                    // A fresh stream per (round, seed, target): distinct inputs each round,
                    // and still fully determined by the fixed base seed, so any failure this
                    // reports reproduces exactly.
                    let mut rng = Rng::new(
                        0x51A5_D00D_1234_ABCD
                            ^ rounds.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                            ^ ((si as u64) << 32)
                            ^ (ti as u64).wrapping_mul(0x0BAD_C0DE),
                    );
                    // Stack depth cycles 1..=6 across rounds: shallow mutations stay close to
                    // valid (deep code, narrow exploration), deep ones roam further (wide
                    // exploration, more early rejects). Both are wanted, so alternate rather
                    // than pick.
                    let stack = 1 + (rounds as usize + si) % 6;
                    if let Some(f) = hammer_n(
                        target,
                        label,
                        seed,
                        &donors,
                        4_000,
                        stack,
                        &mut rng,
                        std::time::Duration::from_millis(120),
                        &mut counts[ti],
                    ) {
                        failures.push(f);
                    }
                    if std::time::Instant::now() > deadline {
                        break 'session;
                    }
                }
            }
        }
    });

    let total: u64 = counts.iter().sum();
    eprintln!("deep fuzz: {total} inputs across {rounds} rounds");
    for (t, n) in targets.iter().zip(&counts) {
        eprintln!("  {:>34}  {n:>10}", t.0);
    }
    assert!(
        failures.is_empty(),
        "{} parser panic(s) found by the deep session:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
