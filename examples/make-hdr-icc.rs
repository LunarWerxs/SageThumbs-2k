//! Write the BT.2020 / PQ ICC profile the TIFF HDR twin fixture carries
//! (`tests/fixtures/tiff/bt2020-pq.icc`), built by the same `moxcms` the decoder reads it
//! back with, so the fixture is a real v4 profile with a `cicp` tag rather than bytes typed
//! by hand. Run `cargo run --example make-hdr-icc -- <out.icc>`, then
//! `python scripts/make-tiff-hdr-fixtures.py tests/fixtures/tiff` to embed it.

fn main() {
    let out = std::env::args()
        .nth(1)
        .expect("usage: make-hdr-icc <out.icc>");
    let icc = moxcms::ColorProfile::new_bt2020_pq()
        .encode()
        .expect("encode the BT.2020 PQ profile");
    std::fs::write(&out, &icc).expect("write the profile");
    println!("wrote {out} ({} bytes)", icc.len());
}
