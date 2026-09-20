//! Formats magick selects by file NAME, staged to a temp file with the right extension.

use super::*;

/// Extensions whose ImageMagick coder is chosen by FILE NAME and never by
/// content, so the stdin pipe above can never reach them.
///
/// `decode_via_magick_spec_alloc` hands magick a nameless stream. That is fine
/// for the ~93 formats carrying a signature magick can sniff (`magick -list
/// magic`) and is why this tier works at all. It is not fine for these:
/// `magick identify sample.rla` succeeds only because the extension named the
/// coder, and the identical bytes arriving on stdin come back "no decode
/// delegate for this image format". Each of these was a registered, advertised
/// format that could not thumbnail on any surface.
///
/// Membership comes from ImageMagick's own tables rather than taste: an entry
/// belongs here when `magick -list format` maps it to a reading coder that has
/// no `magick -list magic` signature AND no other tier of ours can read it.
/// Formats that DO sniff are deliberately absent — naming a coder for those
/// would bypass magick's own detection and could decode bytes as a format they
/// are not.
///
/// `sct` is the one entry the table rule alone would have missed (added 2026-09-17, when the
/// corpus got its first REAL Scitex files). `magick -list magic` does list SCT, as `CT` at
/// offset 0 - but a Scitex CT file opens with its 80-byte name field and carries `CT` at
/// offset 80, so that signature never matches a file anyone owns. Four real files from two
/// writers all came back "no decode delegate" on stdin and decoded by name. A signature in
/// the table is only evidence when a real file matches it.
pub(super) const NAME_SELECTED_EXTS: &[&str] =
    &["cut", "jnx", "mac", "pix", "rla", "scr", "sct", "tim"];

/// Camera RAW, which rides magick's equally name-selected `dng` coder.
///
/// Separate from [`NAME_SELECTED_EXTS`] because the justification differs: RAW
/// normally never reaches magick at all, since `tiers::largest_embedded_jpeg`
/// lifts the camera's own preview out first and far more cheaply. This is the
/// backstop for a RAW whose embedded preview is missing or unreadable, which is
/// exactly what `sample.mdc` (Minolta) turned out to be — magick demosaics it
/// fine, and before this it produced no thumbnail at all.
///
/// A SUPERSET of `formats::RAW_EXTS`, not a mirror: `rmf` and `sti` are filed under
/// Images in `FORMATS` but `magick -list format` routes both through the same `dng`
/// module as the real camera RAW, so they belong here on decode grounds. Membership
/// is about which coder reads the bytes, never about the Settings category.
/// A RAW extension missing here just keeps the old no-thumbnail behaviour, so the
/// lists drifting degrades rather than breaks.
pub(super) const RAW_CODER_EXTS: &[&str] = &[
    "3fr", "arw", "bay", "cap", "cr2", "cr3", "crw", "dcr", "dcs", "dng", "drf", "erf", "fff",
    "iiq", "k25", "kdc", "mdc", "mef", "mos", "mrw", "nef", "nrw", "orf", "ori", "pef", "ptx",
    "pxn", "raf", "rmf", "rw2", "rwl", "sr2", "srf", "srw", "sti", "x3f",
];

/// Is `ext` a camera RAW that magick reads through its name-selected `dng` coder?
///
/// Exposed so the full-fidelity path can ask "would the named coder do better than the generic
/// tier here", which is a different question from [`has_name_selected_coder`]: that one also
/// covers signature-less non-RAW formats like `.rla`, where there is no second opinion to seek.
pub(crate) fn is_raw_coder_ext(ext: &str) -> bool {
    RAW_CODER_EXTS.contains(&ext)
}

/// Would [`decode_named_extension`] have a coder to offer for `ext`?
pub(in super::super) fn has_name_selected_coder(ext: &str) -> bool {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    NAME_SELECTED_EXTS.contains(&ext.as_str()) || RAW_CODER_EXTS.contains(&ext.as_str())
}

/// An extension is only ever used to build a temp file NAME, so it must not be
/// able to steer that name anywhere. Real extensions are short and alphanumeric;
/// anything else is refused rather than escaped.
pub(super) fn safe_ext(ext: &str) -> Option<String> {
    let ext = ext.trim_start_matches('.').to_ascii_lowercase();
    let ok = !ext.is_empty() && ext.len() <= 8 && ext.bytes().all(|b| b.is_ascii_alphanumeric());
    ok.then_some(ext)
}

/// How many names to try before giving up. Only a genuinely hostile or wedged
/// %TEMP% can burn these, since the counter alone already makes a collision
/// improbable; the loop exists so `create_new` cannot turn a squatted name into a
/// permanent denial of the whole tier.
pub(super) const MAX_STAGE_ATTEMPTS: u32 = 8;

/// A temp file that deletes itself, named so ImageMagick's coder tables can see
/// the extension. Process-id suffixed like every other temp path in this repo, so
/// concurrent `cargo test` runs and a parallel `st2k batch` fan-out cannot collide.
pub(super) struct NamedTemp(pub(super) std::path::PathBuf);

impl NamedTemp {
    /// Claim `path` EXCLUSIVELY and fill it, or decline. Split out from [`Self::create`] so
    /// the exclusivity property is testable on a name the test controls: driving it through
    /// the shared counter instead made the test race its own siblings and quietly stop
    /// exercising anything (it passed against the very behaviour it was meant to catch).
    ///
    /// `create_new`, never `File::create`. Windows' create-and-truncate follows hard links and
    /// reparse points, so an existing name in `%TEMP%` would have our image bytes written
    /// straight THROUGH it into whatever it really points at. The name is predictable - the pid
    /// is public and the counter restarts at 0 each process - so refusing an existing name is
    /// the guard, not the obscurity of the name.
    pub(super) fn claim(path: std::path::PathBuf, bytes: &[u8]) -> Option<Self> {
        use std::io::Write;
        let mut file = std::fs::File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .ok()?;
        // The guard owns the path from HERE, BEFORE a single byte is written. Building it
        // after the write meant a write that failed part-way - a large RAW meeting a full
        // disk - returned early with the file already created and nothing to unlink it.
        let guard = Self(path);
        let wrote = file.write_all(bytes).is_ok();
        // Close before handing the name to a child process that is about to open it.
        drop(file);
        wrote.then_some(guard)
    }

    pub(super) fn create(bytes: &[u8], ext: &str) -> Option<Self> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir();
        let pid = std::process::id();
        for _ in 0..MAX_STAGE_ATTEMPTS {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            if let Some(staged) =
                Self::claim(dir.join(format!("st2k-coder-{pid}-{n}.{ext}")), bytes)
            {
                return Some(staged);
            }
        }
        crate::safety::log_debug("magick decode: could not claim a staging name in %TEMP%");
        None
    }
}

impl Drop for NamedTemp {
    fn drop(&mut self) {
        // Best effort: a leftover file in %TEMP% is a nuisance, a panic here (in a
        // `panic = "abort"` shell host) is a crash.
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Decode by handing ImageMagick a real file whose NAME carries `ext`, so its own
/// coder tables pick the reader — the one thing the nameless stdin pipe cannot do.
///
/// LAST RESORT ONLY. Every caller runs this after the ordinary tiers have already
/// declined, and that ordering is what makes naming a coder safe: when the name is
/// wrong the decode simply fails and the caller returns the error it already had.
///
/// A temp file rather than a forced `rla:-` stdin spec, because a coder prefix makes
/// magick read the pipe directly instead of spooling it, and the coders disagree about
/// whether they tolerate that: `rla:-` and `mdc:-` work, while `tim:-` dies with
/// "insufficient image data" on the very file `magick sample.tim` reads perfectly.
/// Magick already spools stdin to a temp file of its own on the auto-detect path, so
/// this adds no exposure the normal path does not already carry.
pub(in super::super) fn decode_named_extension(
    bytes: &[u8],
    ext: &str,
    max_edge: Option<u32>,
) -> Result<DynamicImage> {
    let edge = resize_spec(max_edge);
    decode_named_extension_spec(bytes, ext, &edge, TILE_CAPS, Fidelity::Tile)
}

/// As [`decode_named_extension`], at NATIVE resolution: the resize cap is the MAX_DIM bomb
/// guard (shrink-only), not the 4096 memory guard, with the matching re-decode allocation.
/// Exactly the pairing [`decode_psd_composite`] uses, and for the same reason — this is a
/// full-fidelity path, and the whole point is keeping the real pixels. A Mamiya `.mef`
/// through the 4096 guard came out 3078x4096; through this it is its native 4016x5344.
///
/// Only `decode_full_for_path` calls it, and that caller falls back to the capped variant on
/// failure: past roughly 65-90 MP the 16-bit PNG magick hands back can exceed
/// [`FULL_FIDELITY_PNG_CAP`], and a medium-format back at 4096 beats one at nothing.
pub(in super::super) fn decode_named_extension_native(
    bytes: &[u8],
    ext: &str,
) -> Result<DynamicImage> {
    decode_named_extension_spec(
        bytes,
        ext,
        limits::FULL_FIDELITY_EDGE,
        FULL_FIDELITY_CAPS,
        Fidelity::Full,
    )
}

pub(super) fn decode_named_extension_spec(
    bytes: &[u8],
    ext: &str,
    edge: &str,
    caps: DecodeCaps,
    fidelity: Fidelity,
) -> Result<DynamicImage> {
    let Some(ext) = safe_ext(ext).filter(|e| has_name_selected_coder(e)) else {
        return Err(Error::from(E_FAIL));
    };
    // Check for magick BEFORE staging: on the compact (no-ImageMagick) install this
    // path is reached for every one of these formats, and writing up to a few hundred
    // MB to %TEMP% only to discover there is no decoder is pure waste.
    if magick_exe().is_none() {
        return Err(Error::from(E_FAIL));
    }
    let Some(temp) = NamedTemp::create(bytes, &ext) else {
        crate::safety::log_debug("magick decode: could not stage a named temp file");
        return Err(Error::from(E_FAIL));
    };
    let Some(spec) = temp.0.to_str() else {
        return Err(Error::from(E_FAIL));
    };
    // Select the budget and the metafile limits from the REAL bytes, same as
    // `decode_via_magick_capped` — the stdin argument below stays empty (the child
    // reads the staged file, not the pipe), but that emptiness must not also blind
    // the metafile check, or a metafile routed here would silently run under the
    // wider general-purpose budget instead of its tighter one.
    let is_meta = looks_like_metafile(bytes);
    // Empty stdin on purpose: the child reads the file, so shovelling the bytes down a
    // pipe nobody drains would only duplicate the write (and, for a big RAW, the wait).
    let out = decode_via_magick_spec_alloc(&[], &[], spec, &[], edge, caps, fidelity, is_meta);
    drop(temp);
    out
}
