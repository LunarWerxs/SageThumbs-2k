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

use super::select::{pick_cover, Entry};

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
    let archive = ArchiveReader::read(bytes).ok()?;

    // List members → pick the cover entry (no decompression here).
    let entries: Vec<Entry> = archive
        .members()
        .map(|m| Entry {
            name: String::from_utf8_lossy(&m.meta.name).into_owned(),
            is_dir: m.meta.is_directory,
            size: m.meta.unpacked_size,
        })
        .collect();
    let idx = pick_cover(&entries)?;
    let target = entries[idx].name.clone();

    // Stream extraction: capture ONLY the cover, then abort (Err stops the run; the
    // cover is normally page 1 / the first entry, so nothing extra is decompressed).
    let buf = Rc::new(RefCell::new(Vec::new()));
    let mut captured = false;
    let _ = archive.extract_to(None, |meta| {
        if captured {
            return Err(std::io::Error::other("cover captured").into());
        }
        if String::from_utf8_lossy(&meta.name) == target {
            captured = true;
            Ok(Box::new(CapBuf(Rc::clone(&buf))) as Box<dyn Write>)
        } else {
            Ok(Box::new(std::io::sink()) as Box<dyn Write>)
        }
    });

    let out = std::mem::take(&mut *buf.borrow_mut());
    (!out.is_empty() && out.len() as u64 <= super::MAX_COVER).then_some(out)
}
