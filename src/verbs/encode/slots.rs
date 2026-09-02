//! Output-file slots and atomic writes.
//!
//! Every verb that produces a file goes through here, and the reason is a race: a batch
//! run picks output names in PARALLEL, so a name has to be RESERVED (created empty) the
//! moment it is chosen, not when the bytes are ready. [`OutSlot`] owns that reservation
//! and deletes it again if the encode fails, so a failed convert never leaves a 0-byte
//! file behind. Writes land on a temp sibling and are renamed into place, so a killed
//! process cannot leave a half-written image where a whole one was.

use super::*;

/// A reserved, collision-free output path. Creating it makes an EMPTY placeholder
/// file with `create_new`, so concurrent workers — even in separate processes (the
/// DLL pre-reserves a name, then `st2k.exe` writes it) — can never pick the same
/// name. (A plain `while path.exists()` check is a TOCTOU race once batches run in
/// parallel.) The writer renames its finished temp ON TOP of the placeholder
/// (`write_atomic` does exactly this), turning it into the real file.
///
/// On drop the placeholder is removed IFF it's still a zero-byte file: an
/// abandoned/failed reservation never litters, while a real (non-empty) output is
/// never touched. No explicit "commit" is needed — a successful write leaves a
/// non-empty file behind, which drop keeps.
pub(crate) struct OutSlot {
    path: PathBuf,
    /// True when `reserve` created the placeholder itself. A slot handed back after a
    /// failed `create_new` (missing directory, permission) names a path this process
    /// does not own, so neither `Drop` nor a caller's failed-write cleanup may delete
    /// whatever is there.
    created: bool,
}

impl OutSlot {
    /// The reserved path — hand this to the encoder / `st2k`.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this process created the placeholder at [`path`](Self::path). `false`
    /// means the reservation failed and the path is unowned: callers that delete a
    /// destination after a failed write must check this first.
    pub(crate) fn created(&self) -> bool {
        self.created
    }

    /// Consume the slot without running its drop cleanup, returning the reserved
    /// path. For callers where the placeholder is replaced by something OTHER than
    /// an encoder write — e.g. `fs::rename`/`fs::copy` landing an existing file on
    /// top of it — where a legitimately empty source would otherwise trip the
    /// zero-byte-placeholder heuristic and get deleted right after the move.
    ///
    /// `ManuallyDrop` skips `Drop`; `take` moves the path's allocation out so it is
    /// returned rather than leaked (a `forget` of `self` would leak the buffer).
    pub(crate) fn release(self) -> PathBuf {
        let mut me = std::mem::ManuallyDrop::new(self);
        std::mem::take(&mut me.path)
    }
}

impl Drop for OutSlot {
    fn drop(&mut self) {
        // A path this process never created is not ours to clean up.
        if !self.created {
            return;
        }
        // Remove only a still-empty placeholder: a successful write replaced it with
        // a non-empty file (keep it); a failed/abandoned one left it at zero bytes
        // (clean it up). Image encoders never produce a 0-byte success.
        if std::fs::metadata(&self.path)
            .map(|m| m.len() == 0)
            .unwrap_or(false)
        {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Atomically reserve the first free path produced by `name(n)` for n = 0, 1, 2…,
/// by creating an empty placeholder with `create_new`. See [`OutSlot`].
pub(crate) fn reserve(name: impl Fn(u32) -> PathBuf) -> OutSlot {
    let mut n = 0u32;
    loop {
        let cand = name(n);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&cand)
        {
            // The placeholder handle closes here.
            Ok(_) => {
                return OutSlot {
                    path: cand,
                    created: true,
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            // Couldn't create (permission / missing dir): hand the name back anyway —
            // the encode will surface the real error. Don't loop on a non-Exists error.
            // The slot records that it owns nothing at that path, and the io error is
            // logged here because the encode's own failure message names only E_FAIL.
            Err(e) => {
                crate::safety::log(&format!("cannot reserve {}: {e}", cand.display()));
                return OutSlot {
                    path: cand,
                    created: false,
                };
            }
        }
    }
}

/// The collision number a `reserve()` closure stamps onto its Nth retry, shared by every
/// namer in this module: `n` is `reserve`'s zero-based retry counter (0 = the bare name,
/// already free), and this turns the first ACTUAL collision (n=1) into "(2)" rather than
/// "(1)", matching Windows Explorer's own duplicate-file naming. `unique_output` used to
/// stamp the bare `n` instead (first collision "(1)") while `reserve_unique_suffix` already
/// used `n + 1` ("(2)") — two conventions for the same concept in the same file.
fn collision_number(n: u32) -> u32 {
    n + 1
}

/// Reserve a free `<stem>.<ext>` next to `src` (`<stem> (2).<ext>` if taken, `<stem>
/// (3).<ext>` after that — see [`collision_number`]), atomically (see [`reserve`]).
/// Replaces the old existence-check picker.
pub(crate) fn unique_output(src: &Path, ext: &str) -> OutSlot {
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string();
    let dir = src.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let ext = ext.to_string();
    reserve(move |n| {
        let name = if n == 0 {
            format!("{stem}.{ext}")
        } else {
            format!("{stem} ({}).{ext}", collision_number(n))
        };
        dir.join(name)
    })
}

/// `<out>.st2ktmp` — the temp path a write goes to before the atomic rename.
pub(crate) fn with_tmp_suffix(out: &Path) -> PathBuf {
    let mut s = out.to_path_buf().into_os_string();
    s.push(".st2ktmp");
    PathBuf::from(s)
}

/// Atomic write: run `write` against a same-volume `<out>.st2ktmp`, then rename
/// it over `out`. Owns the temp naming ([`with_tmp_suffix`]), the on-error temp
/// cleanup (a failed/partial write leaves no `.st2ktmp` and never an `out`), and
/// a short bounded rename retry (strip.rs-style: 5×40 ms) so a transient
/// Explorer/thumbnail-cache lock (os error 5/32) doesn't fail an otherwise good
/// write. `write` receives the temp path and must produce the finished file there.
pub(crate) fn write_atomic(out: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    let tmp = with_tmp_suffix(out);
    write(&tmp).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    crate::fsutil::rename_retrying(&tmp, out).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::new(
            E_FAIL,
            format!("rename {} -> {}: {e}", tmp.display(), out.display()),
        )
    })
}

/// If "preserve original file date" is enabled (Options), stamp the source file's
/// modified-time onto a freshly-saved output. Best-effort — never fails a save.
pub(crate) fn preserve_src_time(src: &Path, out: &Path) {
    if !crate::settings::preserve_file_date() {
        return;
    }
    if let Ok(mtime) = std::fs::metadata(src).and_then(|m| m.modified()) {
        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(out) {
            let _ = f.set_modified(mtime);
        }
    }
}

/// Reserve a free `<stem> (<suffix>).<ext>` next to `src` (`<stem> (<suffix> n)`
/// if taken), atomically (see [`reserve`]). Used by the IN-PROCESS edit verbs
/// (rotate/resize/email) and the DLL's routed resize/email (which pass the reserved
/// path to `st2k`).
pub(crate) fn reserve_unique_suffix(src: &Path, suffix: &str, ext: &str) -> OutSlot {
    let stem = src
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("image")
        .to_string();
    let dir = src.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
    let (suffix, ext) = (suffix.to_string(), ext.to_string());
    reserve(move |n| {
        let name = if n == 0 {
            format!("{stem} ({suffix}).{ext}")
        } else {
            format!("{stem} ({suffix} {}).{ext}", collision_number(n))
        };
        dir.join(name)
    })
}

/// PREDICT (read-only, no reservation) the `<stem> (<suffix>).<ext>` name that
/// [`reserve_unique_suffix`] would currently pick. Used ONLY by the DLL's routed
/// `st2k rotate`, where `st2k` auto-names the sibling itself — the DLL must guess
/// the name to reveal it WITHOUT creating a placeholder (a placeholder would push
/// st2k's own picker to `(… 2)`). Rotate names derive from the distinct source
/// stem, so parallel rotates over a selection don't collide on the prediction.
pub(crate) fn predict_unique_suffix(src: &Path, suffix: &str, ext: &str) -> PathBuf {
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let dir = src.parent().unwrap_or_else(|| Path::new("."));
    let mut cand = dir.join(format!("{stem} ({suffix}).{ext}"));
    let mut n = 2u32;
    while cand.exists() {
        cand = dir.join(format!("{stem} ({suffix} {n}).{ext}"));
        n += 1;
    }
    cand
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `unique_output` and `reserve_unique_suffix` must agree on the SAME first-collision
    /// numbering ("(2)", matching Explorer) — before this fix `unique_output` picked "(1)"
    /// while `reserve_unique_suffix` already picked "(… 2)", two conventions for one concept
    /// in this file.
    #[test]
    fn unique_output_first_collision_matches_explorer_numbering() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_slots_collision_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("img.png");
        std::fs::write(&src, b"x").unwrap();

        // The bare name is already taken by `src` itself, so the FIRST reservation collides.
        let slot = unique_output(&src, "png");
        assert_eq!(slot.path(), dir.join("img (2).png"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A reservation that could not create its placeholder (here: the directory does not
    /// exist yet) names a path this process does not own. It must say so, and its `Drop`
    /// must leave whatever later appears at that path alone — before this, a zero-byte
    /// file someone else created there was deleted on drop.
    #[test]
    fn a_failed_reservation_owns_nothing_and_drop_leaves_the_path_alone() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_slots_unowned_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let missing = dir.join("not-yet");
        let target = missing.join("out.png");

        let slot = reserve(|_| target.clone());
        assert!(
            !slot.created(),
            "create_new cannot have succeeded in a missing dir"
        );
        assert_eq!(slot.path(), target);

        // Someone else now creates the directory and an empty file at that exact path.
        std::fs::create_dir_all(&missing).unwrap();
        std::fs::write(&target, b"").unwrap();
        drop(slot);
        assert!(
            target.exists(),
            "Drop deleted a zero-byte file the slot never created"
        );

        // The normal case still cleans up its own abandoned placeholder.
        let owned = reserve(|_| missing.join("mine.png"));
        assert!(owned.created());
        let p = owned.path().to_path_buf();
        drop(owned);
        assert!(!p.exists(), "an abandoned placeholder must be removed");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `release` hands the path back without running `Drop`: the placeholder survives
    /// even at zero bytes, which is the whole reason the method exists.
    #[test]
    fn release_returns_the_path_and_keeps_an_empty_placeholder() {
        let dir = std::env::temp_dir().join(format!(
            "st2k_slots_release_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let slot = reserve(|_| dir.join("kept.png"));
        let expected = slot.path().to_path_buf();
        let released = slot.release();
        assert_eq!(released, expected);
        assert!(released.exists(), "release must not delete the placeholder");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
