#![cfg(test)]

//! The measurement behind [`super::reduced_ifd0_has_content`]'s threshold.
//!
//! Run it, do not trust it from memory:
//!
//!   cargo test --release --lib reduced_ifd0_evidence -- --ignored --nocapture

#[test]
#[ignore = "prints corpus measurements; needs ../test-corpus"]
fn what_every_raw_sample_holds_in_its_reduced_ifd0() {
    let corpus = st2k_base::testcorpus::dir();
    let Ok(rd) = std::fs::read_dir(&corpus) else {
        eprintln!("no corpus at {}", corpus.display());
        return;
    };
    let mut rows: Vec<String> = Vec::new();
    for e in rd.flatten() {
        let p = e.path();
        let Ok(bytes) = std::fs::read(&p) else {
            continue;
        };
        if !crate::streamsrc::tiff_ifd0_is_reduced(&bytes) {
            continue;
        }
        let name = p
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match super::decode_with_image(&bytes) {
            Ok(img) => rows.push(format!(
                "{name:<20} {:>5}x{:<5} luma sd {:>7.2}",
                image::GenericImageView::width(&img),
                image::GenericImageView::height(&img),
                super::luma_sd(&img)
            )),
            Err(e) => rows.push(format!("{name:<20} IFD0 did not decode: {e}")),
        }
    }
    rows.sort();
    eprintln!("\nreduced-resolution IFD0 across the corpus:\n");
    for r in &rows {
        eprintln!("  {r}");
    }
    eprintln!("\n{} sample(s)\n", rows.len());
}
