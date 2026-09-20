#![cfg(test)]

use super::*;

/// **The load-bearing half of adding a fuzz seed.** A seed its own parser REJECTS is worse
/// than no seed at all: the fuzzer mutates it happily, every iteration dies at the header,
/// and the suite stays green having tested nothing. That is exactly the state the DDS
/// decoder was in until 2026-08-19, when its only seed was an eight-byte magic stub.
///
/// This is the same discipline as `container::fuzzseed::every_seed_reaches_its_parser`,
/// and it is why the DX10 seeds carry a real `DDS_HEADER_DXT10`: without those 20 bytes
/// they die on "truncated DX10 header" and BC7 and BC6H go untested.
#[test]
fn every_dds_fuzz_seed_really_decodes() {
    for (label, fourcc, dxgi) in [
        ("dxt1", b"DXT1", 0u32),
        ("dxt5", b"DXT5", 0),
        ("bc7", b"DX10", 98),
        ("bc6h", b"DX10", 95),
    ] {
        let s = fuzzapi::seed(fourcc, dxgi, 64, 64, 1);
        assert!(
            fuzzapi::seed_decodes(&s),
            "the {label} fuzz seed does not decode, so mutating it tests nothing"
        );
    }
    // The mip-chain seed, which exists to reach `select_mip`'s offset arithmetic.
    let chain = fuzzapi::seed(b"DXT1", 0, 128, 128, 5);
    assert!(
        fuzzapi::seed_decodes(&chain),
        "the mip-chain seed must decode"
    );

    // And it must really CARRY a chain: a header claiming 5 mips with only level 0 behind
    // it would decode fine and never exercise the walk.
    let one = fuzzapi::seed(b"DXT1", 0, 128, 128, 1);
    assert!(
        chain.len() > one.len(),
        "the mip-chain seed must actually contain more levels than a single-level one"
    );
}
