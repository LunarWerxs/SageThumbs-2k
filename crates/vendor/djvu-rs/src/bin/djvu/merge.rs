//! `djvu merge` and `djvu split`.

use super::*;

pub(super) fn cmd_merge(
    files: &[PathBuf],
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if files.is_empty() {
        return Err("no input files".into());
    }

    let docs: Vec<Vec<u8>> = files
        .iter()
        .map(|f| std::fs::read(f).map_err(|e| format!("{}: {e}", f.display())))
        .collect::<Result<_, _>>()?;

    let refs: Vec<&[u8]> = docs.iter().map(|d| d.as_slice()).collect();
    let merged = djvu_rs::djvm::merge(&refs)?;

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, merged)?;
    eprintln!("Merged {} files → {}", files.len(), output.display());
    Ok(())
}

pub(super) fn cmd_split(
    path: &Path,
    page: Option<usize>,
    pages: Option<&str>,
    output: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(path)?;

    let (start, end) = if let Some(p) = page {
        if p == 0 {
            return Err("page numbers are 1-based".into());
        }
        (p - 1, p)
    } else if let Some(range) = pages {
        parse_page_range(range)?
    } else {
        return Err("specify --page or --pages".into());
    };

    let result = djvu_rs::djvm::split(&data, start, end)?;

    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(output, result)?;
    eprintln!("Split pages {}–{} → {}", start + 1, end, output.display());
    Ok(())
}

/// Parse "1-50" into (0, 50) — 0-based start, exclusive end.
pub(super) fn parse_page_range(s: &str) -> Result<(usize, usize), Box<dyn std::error::Error>> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 2 {
        return Err(format!("invalid page range: {s} (expected N-M)").into());
    }
    let start: usize = parts[0].parse()?;
    let end: usize = parts[1].parse()?;
    if start == 0 || end == 0 || start > end {
        return Err(format!("invalid page range: {s}").into());
    }
    Ok((start - 1, end))
}
