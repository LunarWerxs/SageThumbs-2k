#![cfg(test)]

//! The tier-1 debugging harness. The tiny corpus files are LOSSLESS (5/3, no
//! quantization, verified `magick compare` AE = 0 against their source PNGs), so a
//! correct decoder must reproduce them BIT-EXACTLY at full resolution. Any mismatch is
//! proof of a bug, and 8x8 means a single code-block to trace. Reports rather than
//! asserts while tier-1 is under repair.

#[test]
fn lossless_tiny_files_decode_bit_exactly() {
    let dir = crate::testcorpus::dir();
    for name in [
        "tiny8-gray",
        "tiny16-rgb",
        "tiny16-plasma",
        "tiny16-gplasma",
        "tiny32-grad",
        "tiny32-plasma",
    ] {
        let Ok(jp2) = std::fs::read(dir.join(format!("{name}.jp2"))) else {
            continue;
        };
        let Ok(png) = image::open(dir.join(format!("{name}.png"))) else {
            continue;
        };
        let truth = png.to_rgb8();
        // A huge target keeps every resolution level: a full, lossless decode.
        match super::decode_reduced(&jp2, u32::MAX) {
            Ok((rgb, w, h)) => {
                if (w, h) != (truth.width(), truth.height()) {
                    eprintln!(
                        "  {name}: SIZE {}x{} want {}x{}",
                        w,
                        h,
                        truth.width(),
                        truth.height()
                    );
                    continue;
                }
                let t = truth.as_raw();
                let n = t.len();
                let bad = (0..n).filter(|&i| t[i] != rgb[i]).count();
                let worst = (0..n).map(|i| t[i].abs_diff(rgb[i])).max().unwrap_or(0);
                assert_eq!(
                    bad, 0,
                    "{name}: {bad}/{n} bytes wrong (worst {worst}) — reversible 5/3 has                          no rounding excuse, a single differing byte is a real decoder bug"
                );
            }
            Err(e) => panic!("{name}: lossless corpus file failed to decode: {e}"),
        }
    }
}
