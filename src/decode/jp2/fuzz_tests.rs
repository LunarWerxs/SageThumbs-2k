//! Red team for the half of this module that is WIRED IN.
//!
//! `jp2_dimensions` runs on files arriving from Explorer, in-process, in a crate built
//! with `panic = "abort"` — so a panic here does not return an error, it takes down
//! explorer.exe. These tests assert the parser NEVER panics and always terminates,
//! whatever bytes it is handed. Correct output on garbage is not required; surviving is.

use super::*;

/// Deterministic xorshift, so a failure is reproducible from the seed alone.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn byte(&mut self) -> u8 {
        (self.next() >> 24) as u8
    }
}

fn corpus() -> Vec<Vec<u8>> {
    let dir = crate::testcorpus::dir();
    ["sample.jp2", "sample.j2k", "sample.jpf", "huge.jp2"]
        .iter()
        .filter_map(|n| std::fs::read(dir.join(n)).ok())
        .collect()
}

#[test]
fn never_panics_on_random_bytes() {
    let mut rng = Rng(0x5EED_1234_ABCD_0001);
    for _ in 0..2000 {
        let n = (rng.next() % 512) as usize;
        let mut v: Vec<u8> = (0..n).map(|_| rng.byte()).collect();
        // Bias towards things that LOOK like our formats, so the fuzz reaches real code
        // rather than bouncing off the magic check.
        if v.len() >= 4 && rng.next().is_multiple_of(2) {
            v[0..4].copy_from_slice(&[0xFF, 0x4F, 0xFF, 0x51]);
        }
        let _ = dimensions(&v);
        let _ = is_jp2(&v);
    }
}

#[test]
fn never_panics_on_mutated_real_files() {
    let files = corpus();
    if files.is_empty() {
        eprintln!("skipping: no JPEG 2000 samples in ../test-corpus");
        return;
    }
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    let (mut tried, mut parsed) = (0usize, 0usize);
    for base in &files {
        // Cap the mutation window: the point is to batter the HEADER, which is where all
        // the length and count fields that drive allocation live.
        let window = base.len().min(64 * 1024);
        for _ in 0..300 {
            let mut v = base[..window].to_vec();
            let flips = 1 + (rng.next() % 16) as usize;
            for _ in 0..flips {
                let i = (rng.next() as usize) % v.len();
                v[i] = rng.byte();
            }
            tried += 1;
            if dimensions(&v).is_some() {
                parsed += 1;
            }
        }
    }
    // A fuzz run where every input bounced off the magic check would pass while testing
    // nothing. Most single-byte header flips leave a still-parseable file, so a healthy
    // run gets deep into the parser on the large majority of inputs.
    assert!(
        parsed * 2 > tried,
        "only {parsed}/{tried} mutants reached a full parse — the fuzz is not exercising \
         the parser, so its 'no panic' result means nothing"
    );
}

#[test]
fn never_panics_on_truncation() {
    for base in corpus() {
        // Every prefix of a real file, thinned so the test stays quick on the 11 MB one.
        let step = (base.len() / 400).max(1);
        let mut n = 0;
        while n <= base.len().min(200_000) {
            let _ = dimensions(&base[..n]);
            n += step;
        }
    }
}

/// A `Psot` that does not clear its own SOT segment used to send the cursor backwards,
/// and the marker loop re-read the same SOT forever. Hand-built because no fuzz seed
/// reliably produces a valid SIZ plus a hostile SOT.
/// Same mutation strategy as `never_panics_on_mutated_real_files`, but exercises the
/// actual PIXEL decode (`decode_reduced`), not just header parsing. `dimensions` and
/// `is_jp2` never reach the tile / packet / tier-1 code the A005 (QCD guard/exp
/// arithmetic) and A009 (tile pyramid allocation) findings lived in, so this is the
/// seed that actually red-teams that code under adversarial marker values. Window-only
/// mutants (not appended with the file's remainder) so this stays fast even for the
/// multi-MB corpus files — a truncated tail mostly exercises the header/marker parsing
/// this test adds on top of, still with real quant/precinct/code-block bytes upstream.
#[test]
fn never_panics_decoding_mutated_real_files() {
    let files = corpus();
    if files.is_empty() {
        eprintln!("skipping: no JPEG 2000 samples in ../test-corpus");
        return;
    }
    let mut rng = Rng(0xFEED_C0DE_5A55_0002);
    for base in &files {
        let window = base.len().min(64 * 1024);
        for _ in 0..60 {
            let mut v = base[..window].to_vec();
            let flips = 1 + (rng.next() % 16) as usize;
            for _ in 0..flips {
                let i = (rng.next() as usize) % v.len();
                v[i] = rng.byte();
            }
            let _ = decode_reduced(&v, 64);
        }
    }
}

#[test]
fn hostile_psot_terminates() {
    let mut cs: Vec<u8> = vec![0xFF, 0x4F]; // SOC
                                            // SIZ: 1 component, 64x64, one tile.
    let mut siz = vec![0u8; 36];
    siz[0..2].copy_from_slice(&0u16.to_be_bytes()); // Rsiz
    siz[2..6].copy_from_slice(&64u32.to_be_bytes()); // Xsiz
    siz[6..10].copy_from_slice(&64u32.to_be_bytes()); // Ysiz
    siz[18..22].copy_from_slice(&64u32.to_be_bytes()); // XTsiz
    siz[22..26].copy_from_slice(&64u32.to_be_bytes()); // YTsiz
    siz[34..36].copy_from_slice(&1u16.to_be_bytes()); // Csiz
    siz.extend_from_slice(&[7, 1, 1]); // Ssiz, XRsiz, YRsiz
    cs.extend_from_slice(&[0xFF, 0x51]);
    cs.extend_from_slice(&((siz.len() + 2) as u16).to_be_bytes());
    cs.extend_from_slice(&siz);
    // SOT with Psot = 1: shorter than the SOT segment itself.
    cs.extend_from_slice(&[0xFF, 0x90, 0x00, 0x0A]);
    cs.extend_from_slice(&0u16.to_be_bytes()); // Isot
    cs.extend_from_slice(&1u32.to_be_bytes()); // Psot = 1
    cs.extend_from_slice(&[0x00, 0x01]); // TPsot, TNsot

    let start = std::time::Instant::now();
    let _ = codestream::find_codestream(&cs).and_then(codestream::parse);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(2),
        "a hostile Psot must be rejected, not looped on"
    );
}

/// Header-only bombs: a few hundred bytes declaring a MAX_PIXELS-sized image whose
/// layer x resolution x precinct product runs to billions of packets and tens of
/// millions of precinct-bands. Each must come back as an error promptly, before any
/// of that bookkeeping is allocated, whichever ceiling catches it.
#[test]
fn hostile_packet_products_are_refused_promptly() {
    let edge = crate::decode::limits::MAX_DIM;
    let body = [0u8; 64];
    let cases: [(&str, Vec<u8>); 4] = [
        (
            "65535 layers, no precinct segment",
            hostile_codestream(edge, 5, u16::MAX, 0, None, &body),
        ),
        (
            "256 layers, 4x4 precincts, LRCP walks every resolution",
            hostile_codestream(edge, 5, 256, 0, Some(0x22), &body),
        ),
        (
            "256 layers, 4x4 precincts, RPCL walks only the kept resolution",
            hostile_codestream(edge, 5, 256, 2, Some(0x22), &body),
        ),
        (
            "one layer, 4x4 precincts, single-resolution walk",
            hostile_codestream(edge, 3, 1, 1, Some(0x22), &body),
        ),
    ];
    for (name, cs) in cases {
        assert!(cs.len() < 512, "{name}: the bomb must be header-sized");
        let start = std::time::Instant::now();
        let result = decode_reduced(&cs, 64);
        let took = start.elapsed();
        assert!(
            result.is_err(),
            "{name}: a header-only bomb must not decode"
        );
        assert!(
            took < std::time::Duration::from_secs(2),
            "{name}: must be refused from the header, took {took:?}"
        );
    }
}

/// The same builder at a sane shape is not refused by the budget: a 64x64 image with
/// default precincts and one layer passes the ceilings, walks its three packets (an
/// all-zero body makes each one empty) and decodes to a flat image.
#[test]
fn sane_header_passes_the_walk_budget() {
    let cs = hostile_codestream(64, 2, 1, 0, None, &[0u8; 64]);
    let c = codestream::parse(&cs).expect("well-formed header");
    assert_eq!(validate_reduced_scope(&c, false).ok(), Some(1));
    match decode_reduced(&cs, 64) {
        Ok((rgb, w, h)) => assert_eq!((w, h, rgb.len()), (64, 64, 64 * 64 * 3)),
        Err(e) => panic!("a 64x64 single-layer file must not trip the walk budget: {e}"),
    }
}

/// `fuzzapi::seed()` must actually reach a real decode, not just survive one — a seed
/// its own parser rejects is worse than no seed (see `container::fuzzseed`'s
/// `every_seed_reaches_its_parser` doc for why this class of check matters).
#[test]
fn fuzzapi_seed_reaches_a_real_decode() {
    let cs = fuzzapi::seed();
    assert!(
        dimensions(&cs).is_some(),
        "fuzzapi seed must expose real dimensions"
    );
    assert!(
        decode_reduced(&cs, 64).is_ok(),
        "fuzzapi seed must decode cleanly"
    );
    fuzzapi::dimensions(&cs);
    fuzzapi::decode_reduced(&cs);
}
