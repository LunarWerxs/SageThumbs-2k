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

/// Ceiling on how much of the archive we'll decompress-and-throw-away while walking TOWARDS the
/// picked covers. RAR is sequential (a solid archive can't be entered mid-stream), so entries
/// physically before a cover have to be decompressed even though their output is discarded — and
/// the attacker picks the physical order. Without this, a small `.cbr` holding one huge highly
/// compressible junk file ahead of a normally-named cover burns unbounded CPU in the thumbnail
/// host. Mirrors 7z's `SOLID_SCAN_BUDGET`, which caps exactly the same drain.
const SKIP_SCAN_BUDGET: u64 = 8 * 1024 * 1024;

/// Ceiling on the SUM of bytes captured across every picked cover in one contact-sheet
/// pull (`extract_n`'s `want` targets share ONE pass). Each entry is already capped
/// individually at `MAX_COVER` (32 MiB) by [`CapBuf`], but nothing previously bounded
/// their total — up to 4 picks each hitting `MAX_COVER` could synchronously buffer 128
/// MiB for one shell thumbnail. Mirrors 7z's `NON_SOLID_COVERS_BUDGET`, which caps
/// exactly the same drain (see `sevenz.rs`).
const COVERS_AGGREGATE_BUDGET: u64 = 8 * 1024 * 1024;

/// A `Write` sink that appends into a shared buffer, capped at `MAX_COVER` per entry AND
/// charged against a shared [`COVERS_AGGREGATE_BUDGET`] across every pick in the pass.
struct CapBuf {
    buf: Rc<RefCell<Vec<u8>>>,
    aggregate_remaining: Rc<std::cell::Cell<u64>>,
}

/// A `Write` sink that DISCARDS its input but charges it against a shared budget, erroring out
/// (which aborts the whole `extract_to` walk) once the budget is spent. `std::io::sink()` costs
/// nothing to write to but still costs full CPU to produce — this is what makes that bounded.
struct BudgetSink(Rc<std::cell::Cell<u64>>);

impl Write for BudgetSink {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let spent = self.0.get().saturating_add(data.len() as u64);
        self.0.set(spent);
        if spent > SKIP_SCAN_BUDGET {
            return Err(std::io::Error::other("skip-scan budget exhausted"));
        }
        Ok(data.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Write for CapBuf {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let mut b = self.buf.borrow_mut();
        if (b.len() + data.len()) as u64 > super::MAX_COVER {
            // A rejected write must not leave a TRUNCATED prefix behind: the final
            // filter in `extract_n` only checks length <= MAX_COVER, so an incomplete
            // (and possibly corrupt) image under that cap would otherwise silently pass
            // as the chosen cover.
            b.clear();
            return Err(std::io::Error::other("cover too large"));
        }
        let remaining = self.aggregate_remaining.get();
        if data.len() as u64 > remaining {
            // Same poisoning as the per-entry cap above: an aggregate-budget cutoff
            // mid-write must not leave a truncated buffer that still passes the filter.
            b.clear();
            return Err(std::io::Error::other("covers aggregate budget exhausted"));
        }
        self.aggregate_remaining.set(remaining - data.len() as u64);
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
    let mut targets: std::collections::HashMap<&str, usize> = picks
        .iter()
        .enumerate()
        .map(|(rank, &i)| (entries[i].name.as_str(), rank))
        .collect();

    // Stream extraction: capture ONLY the picked entries, then abort (Err stops the
    // run once every target has been seen; targets are normally the first pages, so
    // little extra is decompressed).
    let bufs: Vec<Rc<RefCell<Vec<u8>>>> = (0..picks.len())
        .map(|_| Rc::new(RefCell::new(Vec::new())))
        .collect();
    let aggregate_remaining = Rc::new(std::cell::Cell::new(COVERS_AGGREGATE_BUDGET));
    let mut remaining = picks.len();
    let drained = Rc::new(std::cell::Cell::new(0u64));
    // Sequential on purpose: `rars` also exposes `extract_to_parallel_buffered`, which
    // spins up a rayon thread pool. rayon already LINKS into the DLL (rars 0.6 made it a
    // mandatory dependency, see the `rars` comment in Cargo.toml), but no pool actually
    // runs today because we never call the parallel entry point. Swapping this call for
    // the parallel one would put a global thread pool inside explorer.exe, the exact
    // thing `src/parallel.rs`'s hand-rolled pool exists to avoid. Guarded by a test below.
    let _ = archive.extract_to(None, |meta| {
        if remaining == 0 {
            return Err(std::io::Error::other("covers captured").into());
        }
        let name = String::from_utf8_lossy(&meta.name).into_owned();
        if let Some(rank) = targets.remove(name.as_str()) {
            remaining -= 1;
            Ok(Box::new(CapBuf {
                buf: Rc::clone(&bufs[rank]),
                aggregate_remaining: Rc::clone(&aggregate_remaining),
            }) as Box<dyn Write>)
        } else {
            // Not a pick — discard, but on the clock (see `SKIP_SCAN_BUDGET`).
            Ok(Box::new(BudgetSink(Rc::clone(&drained))) as Box<dyn Write>)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A rejected per-entry write (over `MAX_COVER`) must not leave a TRUNCATED prefix in
    /// the shared buffer — the final `extract_n` filter only checks length, so a partial
    /// image under the cap would otherwise pass as a valid, if corrupt, cover.
    #[test]
    fn capbuf_clears_its_buffer_when_the_per_entry_cap_is_exceeded() {
        let buf = Rc::new(RefCell::new(Vec::new()));
        let aggregate_remaining = Rc::new(std::cell::Cell::new(COVERS_AGGREGATE_BUDGET));
        let mut cap = CapBuf {
            buf: Rc::clone(&buf),
            aggregate_remaining,
        };
        // A first, small write succeeds and is visible in the shared buffer.
        cap.write_all(b"partial jpeg bytes").unwrap();
        assert!(!buf.borrow().is_empty());

        // A write that would push the buffer past MAX_COVER is rejected...
        let huge = vec![0u8; (crate::container::MAX_COVER + 1) as usize];
        assert!(cap.write(&huge).is_err());
        // ...and the truncated prefix must be gone, not left behind for the caller to
        // mistake for a complete (if oddly small) cover.
        assert!(buf.borrow().is_empty());
    }

    /// The SUM of bytes captured across every picked cover in one pass must be bounded,
    /// even though each individual entry stays under `MAX_COVER`.
    #[test]
    fn covers_aggregate_budget_is_shared_and_charged_across_picks() {
        let aggregate_remaining = Rc::new(std::cell::Cell::new(COVERS_AGGREGATE_BUDGET));
        let buf_a = Rc::new(RefCell::new(Vec::new()));
        let buf_b = Rc::new(RefCell::new(Vec::new()));
        let mut cap_a = CapBuf {
            buf: Rc::clone(&buf_a),
            aggregate_remaining: Rc::clone(&aggregate_remaining),
        };
        let mut cap_b = CapBuf {
            buf: Rc::clone(&buf_b),
            aggregate_remaining: Rc::clone(&aggregate_remaining),
        };

        // First pick spends most of the shared budget; well under MAX_COVER on its own.
        let first = vec![0u8; (COVERS_AGGREGATE_BUDGET - 1024) as usize];
        cap_a.write_all(&first).unwrap();
        assert_eq!(buf_a.borrow().len(), first.len());

        // Second pick, also under MAX_COVER individually, no longer fits the SHARED
        // remaining budget — before this fix only the per-entry MAX_COVER cap applied,
        // so this write would have succeeded and let two picks buffer far more than the
        // aggregate ceiling.
        let second = vec![0u8; 4096];
        assert!(cap_b.write(&second).is_err());
        assert!(
            buf_b.borrow().is_empty(),
            "the truncated pick must be poisoned, not partial"
        );
    }

    /// `rars` exposes both a sequential `extract_to` and a `extract_to_parallel_buffered`
    /// that spins up a rayon thread pool. rayon already links into the shell DLL (rars 0.6
    /// made it a mandatory dependency, see the `rars` comment in Cargo.toml), so calling
    /// the parallel entry point from here would put a live global thread pool inside
    /// explorer.exe, which `src/parallel.rs` exists specifically to avoid. This is a
    /// source-text guard, not a runtime one: it fails the moment someone swaps the call,
    /// before it ever ships.
    #[test]
    fn rar_extraction_never_calls_the_parallel_rars_api() {
        let src = include_str!("rar.rs");
        // Built from two joined pieces on purpose: `include_str!` pulls in this very
        // test file, and the parallel method's NAME is unavoidably spelled out in the
        // explanatory comments above (and in this doc-comment). Writing the call's
        // dot-prefixed, paren-suffixed form as ONE contiguous literal would make this
        // check trip on its own documentation the moment it's read back via include_str!.
        let banned_call = [".extract_to_parallel_", "buffered("].concat();
        assert!(
            !src.contains(&banned_call),
            "rar.rs must keep using the sequential extract_to, not the parallel rars API \
             that spins up a rayon thread pool inside explorer.exe"
        );
    }
}
