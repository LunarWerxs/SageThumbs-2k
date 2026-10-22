//! `djvu bzz-encode` and `djvu bzz-decode`.

use super::*;

pub(super) fn cmd_bzz_encode(file: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(file)?;
    let compressed = djvu_rs::bzz_encode::bzz_encode(&data);
    std::fs::write(output, &compressed)?;
    eprintln!(
        "{}: {} → {} bytes ({:.1}%)",
        file.display(),
        data.len(),
        compressed.len(),
        if data.is_empty() {
            0.0
        } else {
            compressed.len() as f64 / data.len() as f64 * 100.0
        }
    );
    Ok(())
}

pub(super) fn cmd_bzz_decode(file: &Path, output: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(file)?;
    let decoded = djvu_rs::bzz::bzz_decode(&data)?;
    std::fs::write(output, &decoded)?;
    eprintln!(
        "{}: {} → {} bytes",
        file.display(),
        data.len(),
        decoded.len(),
    );
    Ok(())
}
