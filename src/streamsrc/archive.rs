//! The generic project-archive (.zip/.rar/.7z) probe and its streamed cover reads.
//!
//! Deliberately stricter than the dedicated comic/ebook cover paths: those know they
//! are looking at one picture, while an arbitrary project archive can carry a large
//! encoded header, tens of thousands of paths, and no meaningful image at all. Hence
//! the size gate BEFORE any parser runs, and the requirement that the name really say
//! zip/rar/7z rather than the magic alone.

use super::*;

/// Outcome of the generic-archive probe: not a generic archive at all (continue
/// the normal cascade), a generic archive with no readable image (fail to the
/// stock icon), or the picked cover image(s).
pub(super) enum ArchiveProbe {
    NotGeneric,
    NoCover,
    Found(StreamSource),
}

/// A generic project archive is parsed only when its total size is known and
/// inside both the user's preference and the hard decoder ceiling. Unlike a
/// dedicated comic/ebook cover, there is no safe reason to probe an unbounded
/// ZIP/7z directory over an opaque provider stream.
pub(super) fn checked_generic_archive_size(size: Option<u64>, max_file_bytes: u64) -> Option<u64> {
    let size = size?;
    (size <= decode::effective_input_cap(max_file_bytes)).then_some(size)
}

/// The generic-archive (.zip/.rar/.7z) branch of [`stream_source`]. Fires only
/// when BOTH the magic is an archive signature AND the Stat-recovered file name
/// carries a generic-archive extension — cbz/epub/office/kra packages share the
/// zip magic and must keep their dedicated single-cover paths, and a stream with
/// no recoverable name (rare virtual sources) also falls through to those. ZIP
/// and 7z read the entry list + picked entries over the seekable IStream; RAR
/// must buffer because `rars` accepts no reader. All three honor the caller's
/// MaxSize BEFORE parsing. This is deliberately stricter than dedicated comic/
/// ebook cover extraction: generic project archives can have huge encoded headers,
/// tens of thousands of paths, and no meaningful image at all.
pub(super) unsafe fn generic_archive(
    stream: &IStream,
    head: &StreamHead,
    cfg: &ThumbSettings,
    who: &str,
) -> ArchiveProbe {
    let max_file_bytes = cfg.max_file_bytes;
    let first = head.first(8);
    if first.len() < 8 || !crate::container::is_generic_archive_magic(first) {
        return ArchiveProbe::NotGeneric;
    }
    // The extension comes from the head's `Stat`, NOT from `stream_path`: this only needs
    // to know the file TYPE, and a shell stream reports a bare leaf name rather than a
    // path. `stream_path` deliberately rejects a name it cannot resolve to a real file (a
    // relative name would otherwise be resolved against our own working directory), so
    // asking it for an extension here meant this gate answered "not a generic archive"
    // for every stream Explorer ever handed us.
    if !head.extension_is(|ext| matches!(ext, "zip" | "rar" | "7z")) {
        return ArchiveProbe::NotGeneric;
    }

    // This gate is intentionally before ArchiveReader/ZipArchive/RAR parsing.
    // A real 909 MB solid 7z on an SMB share had 18,037 entries and a 235 KB
    // encoded header; despite the decompression budget, merely parsing it issued
    // thousands of tiny remote reads and blocked the shell for minutes. Apply
    // the hard decoder ceiling as well as the user's MaxSize: Settings represents
    // "0 / unlimited" as u64::MAX, but it is only unlimited within that ceiling.
    let max = decode::effective_input_cap(max_file_bytes);
    let reported_size = head.size;
    let Some(size) = checked_generic_archive_size(reported_size, max_file_bytes) else {
        let detail = reported_size
            .map(|n| format!("{n} > {max} bytes"))
            .unwrap_or_else(|| "stream size unavailable".to_string());
        safety::log_debug(&format!(
            "{who}: refusing generic archive before parse ({detail})"
        ));
        return ArchiveProbe::NoCover;
    };

    // Contact sheet (up to 4 images) or classic single cover, per Settings.
    // `ArchiveCollage` is stored as a DWORD; nonzero means on.
    let want = if cfg.archive_collage != 0 { 4 } else { 1 };

    let covers = if crate::container::archive_needs_buffer(first) {
        // RAR: same bounded whole-file read as the normal path, then the one-pass
        // multi-target extraction over the buffer. Bounded by the effective cap the gate
        // above was computed from, not the hard ceiling: a stream that delivers more than
        // its `Stat` size declared stops at the user's MaxSize.
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);
        let Ok(bytes) = read_all(stream, rar_buffer_cap(max_file_bytes), Some(size)) else {
            return ArchiveProbe::NoCover;
        };
        let prefs = crate::container::select::CoverPrefs::from_thumb_settings(cfg);
        crate::container::archive_covers(&bytes, want, &prefs)
    } else {
        let _ = stream.Seek(0, STREAM_SEEK_SET, None);
        // Buffered: the zip central directory and the 7z header are read in small pieces,
        // and each piece would otherwise be one marshaled `IStream::Read` round trip.
        let prefs = crate::container::select::CoverPrefs::from_thumb_settings(cfg);
        crate::container::archive_covers_seek(
            std::io::BufReader::with_capacity(
                READ_AHEAD_BYTES,
                IStreamReader {
                    stream: stream.clone(),
                },
            ),
            first,
            want,
            &prefs,
        )
    };

    match covers {
        None => ArchiveProbe::NoCover,
        Some(covers) if covers.is_empty() => ArchiveProbe::NoCover,
        Some(mut covers) if covers.len() == 1 => {
            // One image: the normal aspect-preserving single-cover pipeline.
            safety::log_debug(&format!("{who}: generic archive single cover"));
            ArchiveProbe::Found(StreamSource::Bytes(covers.swap_remove(0)))
        }
        Some(covers) => {
            safety::log_debug(&format!("{who}: generic archive {} covers", covers.len()));
            ArchiveProbe::Found(StreamSource::Covers(covers))
        }
    }
}

/// For an OVERSIZED file (past the in-memory cap), sniff whether it's a seek-
/// streamable container — a ZIP comic archive (CBZ: central directory + one
/// cover entry) or a Clip Studio `.clip` (the tail SQLite database holding the
/// canvas preview) — and, if so, pull just the cover over the IStream, never the
/// whole file. Oversized 7z/CB7 is deliberately excluded: unlike ZIP, even
/// discovering its entries may require decoding a large encoded header through
/// a name-less shell stream, where we cannot distinguish a comic from an
/// arbitrary project backup. Returns None for everything else (including CBR,
/// which `rars` can't read without a full buffer), so the caller skips it.
pub(super) unsafe fn archive_cover_streamed(
    stream: &IStream,
    head: &StreamHead,
) -> Option<Vec<u8>> {
    let first = head.first(8);
    if first.len() < 8 || crate::container::is_7z(first) {
        return None;
    }
    let _ = stream.Seek(0, STREAM_SEEK_SET, None);
    // Buffered for the same reason as the generic-archive probe: the central directory is
    // read in small pieces over a marshaled stream. No `ThumbSettings` snapshot is
    // reachable from this call chain (see `streamsrc::oversized_rescue`), so the
    // preferences are read here rather than threaded from further up.
    let prefs = crate::container::select::CoverPrefs::from_settings();
    crate::container::archive_cover_seek(
        std::io::BufReader::with_capacity(
            READ_AHEAD_BYTES,
            IStreamReader {
                stream: stream.clone(),
            },
        ),
        first,
        &prefs,
    )
}

/// The buffer cap for the RAR read in [`generic_archive`]: the effective input cap
/// (the user's MaxSize, never above the hard ceiling), the same value the size gate
/// before the read was computed from.
pub(super) fn rar_buffer_cap(max_file_bytes: u64) -> usize {
    let max = decode::effective_input_cap(max_file_bytes);
    usize::try_from(max).map_or(MAX_BYTES, |max| max.min(MAX_BYTES))
}
