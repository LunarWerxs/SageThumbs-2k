//! Shared read-only SQLite low-level primitives: varint decoding, serial-type sizing, and the
//! overflow-page local-size formula.
//!
//! Two independent read-only SQLite b-tree walkers ride on top of these — `container::clip`
//! (finds the largest/`CanvasPreview` PNG cell in an embedded `.clip` database) and
//! `preview::dbdoc` (renders a full schema + row markdown view of a standalone `.db`) — each
//! with its own walk strategy and its own hardening against corrupt/hostile input, but the
//! underlying SQLite-FORMAT math (how a varint decodes, how big a serial type's data is, how
//! many payload bytes a cell keeps locally before overflowing) must not drift between them, so
//! it is written once here instead of twice.
//!
//! `pub` (not `pub(crate)`) only because `preview::dbdoc` lives in the companion app EXE — a
//! separate binary crate that reaches this lib's public surface as
//! `sagethumbs2k_core::sqlite_prim` — the same arrangement as `ocr`/`parallel`/`flv` in
//! `lib.rs`. `#[doc(hidden)]` there because this isn't a stable public API, just a shared
//! internal.

/// SQLite big-endian base-128 varint (1-9 bytes) -> (value, bytes consumed).
pub fn varint(b: &[u8], off: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    for i in 0..9 {
        let byte = *b.get(off + i)?;
        if i == 8 {
            return Some(((result << 8) | byte as u64, 9));
        }
        result = (result << 7) | (byte & 0x7F) as u64;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
    }
    Some((result, 9))
}

/// Byte length of a SQLite serial type's column data.
pub fn serial_size(s: u64) -> usize {
    match s {
        0 | 8 | 9 => 0,
        1 => 1,
        2 => 2,
        3 => 3,
        4 => 4,
        5 => 6,
        6 | 7 => 8,
        10 | 11 => 0, // reserved by the format; treated as empty
        n if n >= 12 => ((n - 12) / 2) as usize, // even = BLOB, odd = TEXT
        _ => 0,
    }
}

/// Bytes of a cell payload stored in the cell itself, before it spills into the overflow-page
/// chain, per the SQLite format's local-size formula. `table_leaf` selects the table-leaf
/// threshold (`usable - 35`) over the index-page one (`(usable-12)*64/255 - 23`) — callers that
/// only ever walk table b-trees (never index/WITHOUT-ROWID pages) always pass `true`.
pub fn local_size(total: usize, usable: usize, table_leaf: bool) -> usize {
    let max_local = if table_leaf {
        usable - 35
    } else {
        (usable - 12) * 64 / 255 - 23
    };
    if total <= max_local {
        return total;
    }
    let min_local = (usable - 12) * 32 / 255 - 23;
    let k = min_local + (total - min_local) % (usable - 4);
    if k <= max_local {
        k
    } else {
        min_local
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_decodes_multi_byte_and_rejects_out_of_bounds() {
        assert_eq!(varint(&[0x09], 0), Some((9, 1))); // 1-byte
        assert_eq!(varint(&[0x81, 0x00], 0), Some((128, 2))); // (1<<7)|0
        assert_eq!(varint(&[0x82, 0x01], 0), Some((257, 2))); // (2<<7)|1
        assert_eq!(varint(&[0xFF], 1), None); // out of bounds
    }

    #[test]
    fn serial_size_covers_fixed_and_blob_text_kinds() {
        assert_eq!(serial_size(0), 0);
        assert_eq!(serial_size(6), 8);
        assert_eq!(serial_size(24), 6); // BLOB: (24-12)/2
        assert_eq!(serial_size(25), 6); // TEXT: (25-13)/2
        assert_eq!(serial_size(10), 0); // reserved
        assert_eq!(serial_size(11), 0); // reserved
    }

    /// The overflow threshold formulas differ between table-leaf and index pages; both must
    /// agree with the format for a payload that fits entirely in the cell, and both must clamp
    /// an oversized payload to at most the table-leaf/index max-local bound.
    #[test]
    fn local_size_thresholds() {
        let u = 4096usize;
        assert_eq!(local_size(100, u, true), 100);
        assert_eq!(local_size(u - 35, u, true), u - 35);
        assert!(local_size(u * 4, u, true) <= u - 35);
        let idx_max = (u - 12) * 64 / 255 - 23;
        assert_eq!(local_size(idx_max, u, false), idx_max);
        assert!(local_size(u * 4, u, false) <= idx_max);
    }
}
