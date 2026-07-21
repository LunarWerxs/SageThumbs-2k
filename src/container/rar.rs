//! RAR (CBR) cover extraction via the PURE-RUST `rars` crate (MIT OR Apache-2.0;
//! no C, no proprietary UnRAR, `#![forbid(unsafe_code)]`). Validated byte-identical
//! to UnRAR on real RAR3/RAR5 + a real multi-page comic.
//!
//! `rars` reads from the in-memory bytes directly (no temp-file spill). We list the
//! members to pick the cover (natural-sort + filler-skip, same heuristic as zip/7z/
//! tar), then stream extraction but COLLECT ONLY the cover and abort right after — so
//! a 100-page comic doesn't fully decompress in Explorer's thumbnail host.

use std::cell::RefCell;
use std::io::Write;
use std::rc::Rc;

use rars::ArchiveReader;

use super::select::{dedupe_by_name, pick_covers, Entry};

/// A `Write` sink that appends into a shared buffer, capped at `MAX_COVER`.
struct CapBuf(Rc<RefCell<Vec<u8>>>);

impl Write for CapBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut b = self.0.borrow_mut();
        if (b.len() + data.len()) as u64 > super::MAX_COVER {
            return Err(std::io::Error::other("cover too large"));
        }
        b.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn extract(bytes: &[u8]) -> Option<Vec<u8>> {
    extract_n(bytes, 1).and_then(|mut v| (!v.is_empty()).then(|| v.swap_remove(0)))
}

/// Up to `want` cover images — the multi-image generalization of [`extract`],
/// feeding the generic-archive contact sheet. RAR decompresses sequentially
/// (`extract_to` walks entries in archive order; a solid archive can't be entered
/// mid-stream at all), so this is ONE pass that captures each picked entry as it
/// streams by and aborts right after the last one — a 100-file archive whose
/// pictures sort early never fully decompresses in Explorer's thumbnail host.
pub fn extract_n(bytes: &[u8], want: usize) -> Option<Vec<Vec<u8>>> {
    let archive = ArchiveReader::read(bytes).ok()?;

    // List members → pick the cover entries (no decompression here).
    let entries: Vec<Entry> = archive
        .members()
        .take(super::MAX_LIST_ENTRIES)
        .map(|m| Entry {
            name: String::from_utf8_lossy(&m.meta.name).into_owned(),
            is_dir: m.meta.is_directory,
            size: m.meta.unpacked_size,
        })
        .collect();
    let picks = dedupe_by_name(pick_covers(&entries, want), &entries);
    if picks.is_empty() {
        return None;
    }
    // rank = position in `picks` (cover first), so the output keeps pick order
    // even though the stream yields entries in archive order. Mutable: each name
    // is REMOVED as it's captured, so a later duplicate-named physical entry
    // (legal in RAR) drains to the sink instead of appending into — and
    // corrupting — an already-captured buffer.
    let mut targets: std::collections::HashMap<&str, usize> =
        picks.iter().enumerate().map(|(rank, &i)| (entries[i].name.as_str(), rank)).collect();

    // Stream extraction: capture ONLY the picked entries, then abort (Err stops the
    // run once every target has been seen; targets are normally the first pages, so
    // little extra is decompressed).
    let bufs: Vec<Rc<RefCell<Vec<u8>>>> =
        (0..picks.len()).map(|_| Rc::new(RefCell::new(Vec::new()))).collect();
    let mut remaining = picks.len();
    let _ = archive.extract_to(None, |meta| {
        if remaining == 0 {
            return Err(std::io::Error::other("covers captured").into());
        }
        let name = String::from_utf8_lossy(&meta.name).into_owned();
        if let Some(rank) = targets.remove(name.as_str()) {
            remaining -= 1;
            Ok(Box::new(CapBuf(Rc::clone(&bufs[rank]))) as Box<dyn Write>)
        } else {
            Ok(Box::new(std::io::sink()) as Box<dyn Write>)
        }
    });

    let out: Vec<Vec<u8>> = bufs
        .into_iter()
        .map(|b| std::mem::take(&mut *b.borrow_mut()))
        .filter(|b| !b.is_empty() && b.len() as u64 <= super::MAX_COVER)
        .collect();
    (!out.is_empty()).then_some(out)
}

/// List up to `max` of a RAR archive's members from headers only (no decompression).
pub fn list(bytes: &[u8], max: usize) -> Option<Vec<Entry>> {
    let archive = ArchiveReader::read(bytes).ok()?;
    Some(
        archive
            .members()
            .take(max)
            .map(|m| Entry {
                name: String::from_utf8_lossy(&m.meta.name).into_owned(),
                is_dir: m.meta.is_directory,
                size: m.meta.unpacked_size,
            })
            .collect(),
    )
}
