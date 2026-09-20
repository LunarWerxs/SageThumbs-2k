#![cfg(test)]

/// `dimensions` must report the FILE's real size from the header alone, with no decode.
/// This is the part of the module that is wired in today, so it is the part that is
/// tested against every JPEG 2000 flavour in the corpus.
#[test]
fn dimensions_match_the_corpus() {
    let dir = crate::testcorpus::dir();
    let cases = [
        ("sample.jp2", 512u32, 384u32),
        ("sample.jpf", 512, 384),
        ("sample.jpx", 512, 384),
        ("sample.j2k", 512, 384),
        ("huge.jp2", 9958, 7686),
    ];
    let mut checked = 0;
    for (name, w, h) in cases {
        let Ok(bytes) = std::fs::read(dir.join(name)) else {
            continue;
        };
        assert_eq!(
            super::dimensions(&bytes),
            Some((w, h)),
            "{name} dimensions must come from the header"
        );
        checked += 1;
    }
    if checked == 0 {
        eprintln!("skipping: no JPEG 2000 samples in ../test-corpus");
    }
}
