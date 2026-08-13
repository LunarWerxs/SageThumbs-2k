//! Structure-aware mutation fuzzing of the pure-Rust binary parsers, run as ordinary
//! `cargo test` regression tests.
//!
//! Motivation, concretely: the MKV codec work shipped a real defect — an 8-byte EBML size
//! vint made `0xFFu8 >> 8` overflow, which panicked in debug and silently mis-parsed in
//! release. Every hand-written unit test passed; nothing fed the parser that one byte shape.
//! The class (a shift/index/arithmetic that a crafted or corrupt header pushes out of range)
//! is exactly what a mutation fuzzer finds cheaply and a fixed example suite does not.
//!
//! What this does: takes small VALID seeds (synthetic ones built here, plus real files from
//! the dev `test-corpus/` when present), applies deterministic structure-aware mutations
//! (bit flips, length-field blowups, truncations, region zeroing/filling), and runs each
//! through every pure-Rust parser entry point inside `catch_unwind`. A debug build has
//! overflow-checks and bounds-check panics ON, so "no panic across N mutations" is a real
//! assertion about the whole overflow/slice class, not just the inputs someone thought to
//! write down. Any panic is reported with the target, seed, and the mutated bytes' head so
//! it reproduces.
//!
//! Deliberately NOT fuzzed here: the tiers that shell out (ImageMagick) or call the OS
//! (Media Foundation, WinRT PDF/OCR) — those aren't ours to harden, and ImageMagick's
//! ~20 s kill-timeout alone would turn a run into hours. This targets the code WE parse
//! untrusted headers with. The corpus pass (dev-only) additionally drives
//! `decode::decode_menu_preview`, the same cascade minus those out-of-process tiers, so our
//! native JXL/DDS/container decoders get mutated real files too.
//!
//! Determinism: a fixed-seed xorshift PRNG (no `rand`, and `Date`/`Instant`-free per repo
//! rules), so a failure is always reproducible and CI is stable.

#![cfg(test)]

use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;

/// xorshift64* — deterministic, dependency-free, seeded per run.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed | 1) // never zero (xorshift's fixed point)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 24) as u8
    }
}

/// One parser entry point: a stable name and a closure that must never panic on any input.
/// The closure discards the result — we're asserting robustness, not correctness here.
type Target = (&'static str, fn(&[u8]));

/// Every pure-Rust parser that consumes untrusted bytes/headers directly. These are the
/// funnels a corrupt or hostile file reaches first; the corpus pass adds `decode_preview`.
fn header_targets() -> Vec<Target> {
    vec![
        ("mkv::keyframe_mini_mkv", |b| {
            let _ = crate::mkv::keyframe_mini_mkv(&mut Cursor::new(b), 0.30);
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
        ("mp4::video_codec_fourcc", |b| {
            let _ = crate::mp4::video_codec_fourcc(&mut Cursor::new(b));
        }),
        ("mp4::cover_art", |b| {
            let _ = crate::mp4::cover_art(&mut Cursor::new(b));
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
            let _ = crate::container::archive_covers(b, 4);
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
    ]
}

/// The header targets PLUS every per-format extractor, aimed at directly instead of through
/// `extract_cover`'s magic dispatch. See `container::fuzzseed` for why the indirection matters:
/// a mutation only reaches a parser through the dispatcher if it left that parser's magic
/// alone, which filters out most of the mutations worth trying.
fn all_targets() -> Vec<Target> {
    let mut v = header_targets();
    v.extend(crate::container::fuzzseed::targets());
    v
}

// ---------------------------------------------------------------------------------------------
// Synthetic seeds — self-contained so the always-on run needs no external files.
// ---------------------------------------------------------------------------------------------

/// One EBML element: id bytes (big-endian, leading zero bytes trimmed) + size vint + data.
fn ebml(id: u64, data: &[u8]) -> Vec<u8> {
    let idb = id.to_be_bytes();
    let start = idb.iter().position(|&b| b != 0).unwrap_or(7);
    let mut out = Vec::new();
    out.extend_from_slice(&idb[start..]);
    out.extend_from_slice(&ebml_vint(data.len() as u64));
    out.extend_from_slice(data);
    out
}

/// EBML size vint, shortest length whose all-ones value isn't the "unknown size" reserve.
fn ebml_vint(n: u64) -> Vec<u8> {
    for len in 1u32..=8 {
        let cap = (1u64 << (7 * len)) - 1;
        if n < cap {
            let mut v = vec![0u8; len as usize];
            let mut x = n;
            for i in (0..len as usize).rev() {
                v[i] = (x & 0xFF) as u8;
                x >>= 8;
            }
            v[0] |= 1u8 << (8 - len);
            return v;
        }
    }
    vec![0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFE]
}

/// A structurally valid Matroska file with the pieces every reader here walks: EBML header,
/// Segment, SeekHead, Info, Tracks (one HEVC video track), Cues, a Cluster, and Attachments
/// with a cover. Not a decodable video — a scaffold rich enough that mutations reach the
/// arithmetic-bearing code instead of bailing at the magic.
fn synthetic_mkv() -> Vec<u8> {
    const ID_EBML: u64 = 0x1A45_DFA3;
    const ID_SEGMENT: u64 = 0x1853_8067;
    const ID_INFO: u64 = 0x1549_A966;
    const ID_TIMECODE_SCALE: u64 = 0x2AD7B1;
    const ID_DURATION: u64 = 0x4489;
    const ID_TRACKS: u64 = 0x1654_AE6B;
    const ID_TRACK_ENTRY: u64 = 0xAE;
    const ID_TRACK_NUMBER: u64 = 0xD7;
    const ID_TRACK_TYPE: u64 = 0x83;
    const ID_CODEC_ID: u64 = 0x86;
    const ID_CUES: u64 = 0x1C53_BB6B;
    const ID_CUE_POINT: u64 = 0xBB;
    const ID_CUE_TIME: u64 = 0xB3;
    const ID_CUE_TRACK_POSITIONS: u64 = 0xB7;
    const ID_CUE_TRACK: u64 = 0xF7;
    const ID_CUE_CLUSTER_POSITION: u64 = 0xF1;
    const ID_CLUSTER: u64 = 0x1F43_B675;
    const ID_CLUSTER_TIMECODE: u64 = 0xE7;
    const ID_ATTACHMENTS: u64 = 0x1941_A469;
    const ID_ATTACHED_FILE: u64 = 0x61A7;
    const ID_FILE_NAME: u64 = 0x466E;
    const ID_FILE_MIME: u64 = 0x4660;
    const ID_FILE_DATA: u64 = 0x465C;

    let info = ebml(
        ID_INFO,
        &[
            ebml(ID_TIMECODE_SCALE, &[0x0F, 0x42, 0x40]),
            ebml(ID_DURATION, &1000.0f32.to_be_bytes()),
        ]
        .concat(),
    );
    let tracks = ebml(
        ID_TRACKS,
        &ebml(
            ID_TRACK_ENTRY,
            &[
                ebml(ID_TRACK_NUMBER, &[1]),
                ebml(ID_TRACK_TYPE, &[1]),
                ebml(ID_CODEC_ID, b"V_MPEGH/ISO/HEVC"),
            ]
            .concat(),
        ),
    );
    let cues = ebml(
        ID_CUES,
        &ebml(
            ID_CUE_POINT,
            &[
                ebml(ID_CUE_TIME, &[0]),
                ebml(
                    ID_CUE_TRACK_POSITIONS,
                    &[
                        ebml(ID_CUE_TRACK, &[1]),
                        ebml(ID_CUE_CLUSTER_POSITION, &[0]),
                    ]
                    .concat(),
                ),
            ]
            .concat(),
        ),
    );
    let cluster = ebml(ID_CLUSTER, &ebml(ID_CLUSTER_TIMECODE, &[0]));
    let attachments = ebml(
        ID_ATTACHMENTS,
        &ebml(
            ID_ATTACHED_FILE,
            &[
                ebml(ID_FILE_NAME, b"cover.jpg"),
                ebml(ID_FILE_MIME, b"image/jpeg"),
                ebml(ID_FILE_DATA, b"\xFF\xD8\xFF\xE0JPEGISHDATA\xFF\xD9"),
            ]
            .concat(),
        ),
    );
    let body = [info, tracks, cues, cluster, attachments].concat();
    let mut file = ebml(ID_EBML, &[0x42, 0x82, 0x84, b'w', b'e', b'b', b'm']);
    file.extend_from_slice(&ebml(ID_SEGMENT, &body));
    file
}

/// Big-endian ISO-BMFF box: `size(4) type(4) payload`.
fn mp4box(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut v = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(typ);
    v.extend_from_slice(payload);
    v
}
/// Full box: `size(4) type(4) version(1) flags(3) payload`.
fn mp4full(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut body = vec![0u8; 4];
    body.extend_from_slice(payload);
    mp4box(typ, &body)
}
fn concat32(vals: &[u32]) -> Vec<u8> {
    vals.iter().flat_map(|v| v.to_be_bytes()).collect()
}

/// A structurally valid MP4 with a single indexed video track, laid out the way
/// `mp4::keyframe_mini_mp4`/`video_codec_fourcc` walk: ftyp, then moov ▸ trak ▸ mdia ▸
/// { mdhd, hdlr='vide', minf ▸ stbl ▸ { stsd(avc1 visual entry), stts, stsc, stco, stsz,
/// stss } }, then a small mdat the stco points at.
fn synthetic_mp4() -> Vec<u8> {
    // mdhd v0: creation(4) modification(4) timescale(4) duration(4) lang(2) pre_defined(2).
    let mdhd = mp4full(b"mdhd", &concat32(&[0, 0, 1000, 100]));
    // hdlr: pre_defined(4) handler_type(4 = 'vide') reserved(12) name(1, nul).
    let mut hdlr_body = vec![0u8; 4];
    hdlr_body.extend_from_slice(b"vide");
    hdlr_body.extend_from_slice(&[0u8; 12]);
    hdlr_body.push(0);
    let hdlr = mp4full(b"hdlr", &hdlr_body);

    // VisualSampleEntry: standard fixed layout so width/height land at stsd[48],[50] and the
    // fourcc at stsd[20..24]. size(4) type(4) reserved(6) dri(2) pre(2) res(2) pre(12)
    // width(2) height(2) hres(4) vres(4) res(4) frame_count(2) compressorname(32) depth(2)
    // pre(2).
    let mut vse = Vec::new();
    vse.extend_from_slice(&[0u8; 6]); // reserved
    vse.extend_from_slice(&1u16.to_be_bytes()); // data_reference_index
    vse.extend_from_slice(&[0u8; 2]); // pre_defined
    vse.extend_from_slice(&[0u8; 2]); // reserved
    vse.extend_from_slice(&[0u8; 12]); // pre_defined[3]
    vse.extend_from_slice(&64u16.to_be_bytes()); // width
    vse.extend_from_slice(&64u16.to_be_bytes()); // height
    vse.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // horiz dpi 72
    vse.extend_from_slice(&0x0048_0000u32.to_be_bytes()); // vert dpi 72
    vse.extend_from_slice(&[0u8; 4]); // reserved
    vse.extend_from_slice(&1u16.to_be_bytes()); // frame_count
    vse.extend_from_slice(&[0u8; 32]); // compressorname
    vse.extend_from_slice(&24u16.to_be_bytes()); // depth
    vse.extend_from_slice(&0xFFFFu16.to_be_bytes()); // pre_defined -1
    let avc1 = mp4box(b"avc1", &vse);
    // stsd: version+flags(4) entry_count(4) then the entry.
    let mut stsd_body = 1u32.to_be_bytes().to_vec();
    stsd_body.extend_from_slice(&avc1);
    let stsd = mp4full(b"stsd", &stsd_body);

    let stts = mp4full(b"stts", &concat32(&[1, 4, 1000])); // 4 samples, 1000 ticks each
    let stsc = mp4full(b"stsc", &concat32(&[1, 1, 1, 1])); // one sample per chunk
    let stsz = mp4full(b"stsz", &concat32(&[8, 4])); // uniform 8-byte samples, 4 of them
    let stss = mp4full(b"stss", &concat32(&[1, 1])); // sample 1 is a sync sample

    // stco: 4 chunk offsets. Placeholder now; patched to the real mdat offset after layout.
    let stco = mp4full(b"stco", &concat32(&[4, 0, 0, 0, 0]));

    let stbl = mp4box(b"stbl", &[stsd, stts, stsc, stco, stsz, stss].concat());
    let minf = mp4box(b"minf", &stbl);
    let mdia = mp4box(b"mdia", &[mdhd, hdlr, minf].concat());
    let trak = mp4box(b"trak", &mdia);
    let moov = mp4box(b"moov", &trak);
    let ftyp = mp4box(b"ftyp", b"isom\0\0\x02\0isomiso2avc1mp41");

    let mut out = Vec::new();
    out.extend_from_slice(&ftyp);
    out.extend_from_slice(&moov);
    let mdat_data_off = (out.len() + 8) as u32;
    // Patch the four stco offsets to point at consecutive 8-byte samples inside the mdat.
    let stco_pat = find_subslice(&out, b"stco").expect("stco present");
    let first = stco_pat + 8 + 4 + 4; // box header + ver/flags + entry_count
    for i in 0..4u32 {
        let off = (mdat_data_off + i * 8).to_be_bytes();
        out[first + i as usize * 4..first + i as usize * 4 + 4].copy_from_slice(&off);
    }
    out.extend_from_slice(&mp4box(b"mdat", &[0xABu8; 32]));
    out
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Tiny format headers — enough magic to send each sniffer/extractor down its real path.
fn header_stubs() -> Vec<Vec<u8>> {
    let mut v: Vec<Vec<u8>> = vec![
        b"PK\x03\x04".to_vec(),              // zip local file header
        b"7z\xBC\xAF\x27\x1C".to_vec(),      // 7z
        b"Rar!\x1A\x07\x00".to_vec(),        // rar4
        b"Rar!\x1A\x07\x01\x00".to_vec(),    // rar5
        b"8BPS\0\x01".to_vec(),              // psd
        b"FORM\0\0\0\x10ILBM".to_vec(),      // iff/ilbm
        b"DDS \x7C\0\0\0".to_vec(),          // dds
        b"%!PS-Adobe-3.0 EPSF-3.0".to_vec(), // eps
        b"OggS\0\x02".to_vec(),              // ogg
        b"RIFF\0\0\0\0WEBP".to_vec(),        // webp/riff
        b"RIFF\0\0\0\0AVI ".to_vec(),        // avi
        b"ID3\x03\0".to_vec(),               // mp3/id3
        b"fLaC\0\0\0\x22".to_vec(),          // flac
        b"BLENDER-v300".to_vec(),            // blend
        b"\x89PNG\r\n\x1a\n".to_vec(),       // png sig
    ];
    // MP4 audio brand (M4A) — routes the ISO-BMFF path as audio, not video.
    v.push(mp4box(b"ftyp", b"M4A \0\0\0\0M4A mp42isom"));
    v
}

// ---------------------------------------------------------------------------------------------
// Mutation engine
// ---------------------------------------------------------------------------------------------

/// Apply one random structure-aware mutation to `seed`. Kept localized (a few bytes, one
/// length field, one truncation) so mutated inputs stay close enough to valid to reach deep
/// code rather than bouncing off the magic check.
fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut b = seed.to_vec();
    match rng.below(7) {
        _ if b.is_empty() => {
            b.push(rng.byte());
        }
        0 => {
            // 1..=8 single-bit flips.
            for _ in 0..=rng.below(8) {
                let i = rng.below(b.len());
                b[i] ^= 1u8 << rng.below(8);
            }
        }
        1 => {
            // Set a byte to a boundary value.
            let i = rng.below(b.len());
            b[i] = [0x00u8, 0xFF, 0x7F, 0x80, 0x01][rng.below(5)];
        }
        2 => {
            // Truncate to a random prefix (classic short-read panic finder).
            let n = rng.below(b.len());
            b.truncate(n);
        }
        3 => {
            // Blow up a would-be length field: fill 1/2/4/8 bytes with 0xFF.
            let width = [1usize, 2, 4, 8][rng.below(4)];
            let i = rng.below(b.len());
            for j in i..(i + width).min(b.len()) {
                b[j] = 0xFF;
            }
        }
        4 => {
            // Zero a region (a size going to 0, an id vanishing).
            let i = rng.below(b.len());
            let width = 1 + rng.below(8);
            for j in i..(i + width).min(b.len()) {
                b[j] = 0;
            }
        }
        5 => {
            // Duplicate a slice (grow without changing the head).
            let i = rng.below(b.len());
            let len = 1 + rng.below(b.len() - i);
            let chunk = b[i..i + len].to_vec();
            let at = rng.below(b.len());
            b.splice(at..at, chunk);
        }
        _ => {
            // Overwrite a run with random bytes.
            let i = rng.below(b.len());
            let len = 1 + rng.below(16);
            for j in i..(i + len).min(b.len()) {
                b[j] = rng.byte();
            }
        }
    }
    b
}

/// Run `iters` mutations of `seed` (named `label`) through `target`, capturing any panic.
/// Returns a human-readable failure string on the FIRST panic, else `None`.
fn hammer(target: Target, label: &str, seed: &[u8], iters: usize, rng: &mut Rng) -> Option<String> {
    let (name, f) = target;
    let deadline = std::time::Instant::now() + PAIR_BUDGET;
    for it in 0..iters {
        // Checked every 32 iterations: `Instant::now()` per iteration would itself be a
        // measurable share of the cost for the parsers that reject in nanoseconds.
        if it % 32 == 0 && std::time::Instant::now() > deadline {
            break;
        }
        let input = mutate(rng, seed);
        let res = catch_unwind(AssertUnwindSafe(|| f(&input)));
        if let Err(e) = res {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".into());
            let head: String = input
                .iter()
                .take(48)
                .map(|byte| format!("{byte:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            return Some(format!(
                "PANIC in {name} on seed '{label}' iter {it}: {msg}\n  input[{}] head: {head}",
                input.len()
            ));
        }
    }
    None
}

/// Truncate the pristine seed and feed every prefix (no PRNG) — the single most productive
/// class for short-read / off-by-one panics.
///
/// Exhaustive over the first [`TRUNC_EXHAUSTIVE`] bytes, which is where the headers, length
/// fields and index structures these parsers walk actually live, then strided over the rest.
/// Exhaustive everywhere is not affordable and buys nothing: a 96 KB seed would be 96,000
/// invocations for ONE (seed, target) pair, and the tail bytes are payload, not structure.
fn truncation_sweep(target: Target, label: &str, seed: &[u8]) -> Option<String> {
    const TRUNC_EXHAUSTIVE: usize = 2048;
    const TRUNC_STRIDE: usize = 257; // prime: never aligns with a power-of-two field width
    let (name, f) = target;
    let mut n = 0usize;
    while n <= seed.len() {
        let input = &seed[..n];
        if catch_unwind(AssertUnwindSafe(|| f(input))).is_err() {
            return Some(format!(
                "PANIC in {name} on seed '{label}' truncated to {n}/{} bytes",
                seed.len()
            ));
        }
        n += if n < TRUNC_EXHAUSTIVE {
            1
        } else {
            TRUNC_STRIDE
        };
    }
    None
}

/// Silence the panic hook for the duration of `body` so the thousands of intentionally
/// caught panics (in a failing run) don't flood stderr with backtraces; restore it after.
fn with_quiet_panics<T>(body: impl FnOnce() -> T) -> T {
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let out = body();
    std::panic::set_hook(prev);
    out
}

/// Per-target wall-clock budget for one (seed, target) pair. Mutation fuzzing is only
/// useful if it stays fast enough to run on every `cargo test`, and a few targets can be
/// pushed into genuinely expensive work by a mutation (an archive parser handed a plausible
/// central directory, WIC on a mutated raster). Stop early rather than let one pair dominate
/// the run; the iteration count is a target, not a contract.
const PAIR_BUDGET: std::time::Duration = std::time::Duration::from_millis(600);

fn run_all(seeds: &[(&str, Vec<u8>)], iters_per: usize, base_seed: u64) -> Vec<String> {
    let targets = all_targets();
    let mut failures = Vec::new();
    with_quiet_panics(|| {
        for (si, (label, seed)) in seeds.iter().enumerate() {
            for (ti, &target) in targets.iter().enumerate() {
                // Distinct stream per (seed,target) so a fix to one doesn't shift others.
                let mut rng = Rng::new(
                    base_seed ^ ((si as u64) << 32) ^ (ti as u64).wrapping_mul(0x9E37_79B9),
                );
                if let Some(f) = truncation_sweep(target, label, seed) {
                    failures.push(f);
                }
                if let Some(f) = hammer(target, label, seed, iters_per, &mut rng) {
                    failures.push(f);
                }
            }
        }
    });
    failures
}

/// The always-on, self-contained fuzz pass: synthetic seeds + random buffers, no external
/// files. Runs in CI and on every `cargo test`.
#[test]
fn parsers_survive_mutation_of_synthetic_seeds() {
    let mut seeds: Vec<(&str, Vec<u8>)> = vec![("mkv", synthetic_mkv()), ("mp4", synthetic_mp4())];
    // One structurally VALID file per container format. Without these the only container input
    // this gate ever saw was a handful-of-bytes magic stub, so a mutation had essentially
    // nothing to corrupt — and on CI, which has no `test-corpus` to fall back on, that was the
    // whole of the coverage. See `container::fuzzseed`.
    seeds.extend(crate::container::fuzzseed::seeds());
    for (i, stub) in header_stubs().into_iter().enumerate() {
        seeds.push((Box::leak(format!("stub{i}").into_boxed_str()), stub));
    }
    // A few pure-random buffers of assorted sizes — covers the shallow reject paths.
    let mut rng = Rng::new(0xDEAD_BEEF_CAFE_1234);
    for (i, &sz) in [0usize, 1, 2, 3, 4, 8, 16, 64, 200, 999].iter().enumerate() {
        let buf: Vec<u8> = (0..sz).map(|_| rng.byte()).collect();
        seeds.push((Box::leak(format!("rand{i}_{sz}").into_boxed_str()), buf));
    }

    // 3000 mutations per (seed, target) was right when there were 16 targets and 27 mostly-tiny
    // seeds. With the container seeds and their parsers added the matrix is ~5x bigger, and at
    // 3000 this gate cost 22.6 s of every `cargo test`. 1000 holds it near 10 s while KEEPING
    // the exhaustive truncation sweep, which is the higher-yield half: short-read and
    // off-by-one panics are what a prefix walk finds, and it is deterministic rather than
    // sampled. The deep run (`--ignored`, real corpus) is where depth belongs.
    let failures = run_all(&seeds, 1000, 0x5A5A_1234_9E37_79B9);
    assert!(
        failures.is_empty(),
        "{} parser panic(s) found by mutation fuzzing:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Deep pass over REAL files from the dev `test-corpus/` + `test-corpus-real/` (which live
/// outside the repo and are not committed). Mutates a size-capped sample of each and drives
/// the header parsers plus the hermetic decode cascade.
///
/// `#[ignore]` because it is minutes, not seconds: real seeds are far larger and more varied
/// than the synthetic ones, which is exactly what makes it worth running — just not on every
/// inner-loop `cargo test`. Run it deliberately:
///
/// ```text
/// cargo test --lib fuzz:: -- --ignored --nocapture
/// ```
///
/// The always-on [`parsers_survive_mutation_of_synthetic_seeds`] is the CI gate; this is the
/// deeper sweep for a dev machine that has the corpus. It skips cleanly if the corpus is absent.
#[test]
#[ignore = "deep corpus fuzz (minutes); run with --ignored"]
fn parsers_survive_mutation_of_corpus_samples() {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let roots = [
        manifest.join("..").join("test-corpus"),
        manifest.join("..").join("test-corpus-real"),
    ];
    // Cap per-file bytes so a multi-MB RAW doesn't make the mutation loop crawl; the header
    // parsers only ever look near the start, and decode caps its own input anyway.
    const CAP: usize = 96 * 1024;
    const MAX_FILES: usize = 60;

    let mut seeds: Vec<(&str, Vec<u8>)> = Vec::new();
    for root in &roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        let mut names: Vec<_> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .collect();
        names.sort(); // stable order → deterministic seed selection
        for p in names {
            if seeds.len() >= MAX_FILES {
                break;
            }
            let Ok(mut bytes) = std::fs::read(&p) else {
                continue;
            };
            if bytes.len() > CAP {
                bytes.truncate(CAP);
            }
            if bytes.is_empty() {
                continue;
            }
            let name = p
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("?")
                .to_string();
            seeds.push((Box::leak(name.into_boxed_str()), bytes));
        }
    }
    if seeds.is_empty() {
        eprintln!("corpus fuzz: no test-corpus present — skipping");
        return;
    }

    // Header parsers on the real files (fewer iters — there are many seeds).
    let mut failures = run_all(&seeds, 400, 0x00C0_FFEE_1234_5678);

    // Plus the DECODE cascade — but via `decode_menu_preview`, which is the same tier stack
    // minus the three that leave this process: the ImageMagick subprocess, Media Foundation,
    // and the WinRT PDF rasterizer. That keeps the fuzz hermetic and fast (magick alone has
    // an 18-20 s kill-timeout, so a full `decode_preview` fuzz would take hours and would be
    // testing someone else's parser) while still driving OUR pure-Rust JXL, DDS, container
    // and image tiers on mutated real files.
    let preview: Target = ("decode::decode_menu_preview", |b| {
        let _ = crate::decode::decode_menu_preview(b);
    });
    with_quiet_panics(|| {
        let mut rng = Rng::new(0x1122_3344_5566_7788);
        for (label, seed) in &seeds {
            if let Some(f) = hammer(preview, label, seed, 120, &mut rng) {
                failures.push(f);
            }
        }
    });

    assert!(
        failures.is_empty(),
        "{} parser panic(s) found by corpus mutation fuzzing:\n{}",
        failures.len(),
        failures.join("\n")
    );
    eprintln!("corpus fuzz: {} seed files, no panics", seeds.len());
}
