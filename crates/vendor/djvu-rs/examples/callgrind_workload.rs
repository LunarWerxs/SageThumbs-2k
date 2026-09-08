//! Fixed corpus workloads for the dependency-free Callgrind CI harness.
//!
//! Usage: `callgrind_workload <bzz|jb2|iw44>`.

use std::path::PathBuf;

fn assets() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("references/djvujs/library/assets")
}

fn find(chunks: &[djvu_rs::iff::Chunk], id: &[u8; 4]) -> Option<Vec<u8>> {
    for chunk in chunks {
        match chunk {
            djvu_rs::iff::Chunk::Leaf { id: found, data } if found == id => {
                return Some(data.clone());
            }
            djvu_rs::iff::Chunk::Form { children, .. } => {
                if let Some(data) = find(children, id) {
                    return Some(data);
                }
            }
            _ => {}
        }
    }
    None
}

fn chunk(file: &str, id: &[u8; 4]) -> Vec<u8> {
    let bytes = std::fs::read(assets().join(file)).expect("fixture");
    let parsed = djvu_rs::iff::parse(&bytes).expect("parse fixture");
    find(parsed.root.children(), id).expect("chunk")
}

fn main() {
    match std::env::args().nth(1).as_deref() {
        Some("bzz") => {
            let payload = chunk("carte.djvu", b"TXTz");
            std::hint::black_box(djvu_rs::bzz::bzz_decode(&payload).expect("decode"));
        }
        Some("jb2") => {
            std::hint::black_box(
                djvu_rs::jb2::decode(&chunk("boy_jb2.djvu", b"Sjbz"), None).expect("decode"),
            );
        }
        Some("iw44") => {
            let mut image = djvu_rs::iw44::Iw44Image::new();
            image
                .decode_chunk(&chunk("boy.djvu", b"BG44"))
                .expect("decode");
            std::hint::black_box(image);
        }
        _ => panic!("usage: callgrind_workload <bzz|jb2|iw44>"),
    };
}
