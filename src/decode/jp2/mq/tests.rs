//! The MQ decoder on hostile and degenerate input: it must never panic, only ever answer 0
//! or 1, and be deterministic - the properties every later JPEG 2000 stage relies on, and
//! the ones a crafted codestream would attack first.

use super::*;

fn symbols(data: &[u8], n: usize) -> Vec<u32> {
    let mut mq = MqDecoder::new(data);
    (0..n).map(|i| mq.decode(i % NUM_CONTEXTS)).collect()
}

#[test]
fn every_symbol_is_a_bit_and_nothing_panics_on_degenerate_streams() {
    let streams: [&[u8]; 5] = [
        &[],
        &[0xFF; 64],
        &[0x00; 64],
        &[0xFF, 0x7F, 0xFF, 0x7F],
        &[0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00, 0x00, 0x00],
    ];
    for data in streams {
        for s in symbols(data, 4096) {
            assert!(s <= 1, "decoded {s} from {data:?}");
        }
    }
}

#[test]
fn a_patterned_stream_decodes_deterministically_and_keeps_answering_past_its_end() {
    let data: Vec<u8> = (0..=255u8).cycle().take(3_000).collect();
    let a = symbols(&data, 50_000);
    let b = symbols(&data, 50_000);
    assert_eq!(a, b, "the same bytes must decode to the same symbols");
    assert!(
        a.iter().any(|&s| s == 1) && a.iter().any(|&s| s == 0),
        "a varied stream yields both bits"
    );
}

#[test]
fn a_fresh_decoder_and_a_fresh_decoder_agree_and_reset_contexts_still_decodes() {
    let data: Vec<u8> = (0..=255u8).rev().cycle().take(512).collect();
    let mut mq = MqDecoder::new(&data);
    let first: Vec<u32> = (0..200).map(|i| mq.decode(i % NUM_CONTEXTS)).collect();
    let mut fresh = MqDecoder::new(&data);
    let again: Vec<u32> = (0..200).map(|i| fresh.decode(i % NUM_CONTEXTS)).collect();
    assert_eq!(first, again);
    // Contexts drift as symbols are decoded; the reset puts every one of the 19 back to its
    // initial state and decoding carries on from the current byte position.
    mq.reset_contexts();
    for cx in [CTX_UNI, CTX_RL, 0, NUM_CONTEXTS - 1] {
        assert!(mq.decode(cx) <= 1);
    }
}
