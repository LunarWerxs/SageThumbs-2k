//! Compress-to-size: the batch, the per-file search, and the wording when the target cannot be met.

use super::*;

/// `VerbAction::CompressToSize` - per-image, on the batch pool. Routed per file to
/// `st2k compress` (helper-if-present, same `compress_to_size` engine), else
/// in-process `compress_one_to_size`; see the module doc's routing list.
pub(super) fn handle_compress_to_size(paths: &[String], size: CompressSize) -> ActionReport {
    let imgs = images_in(paths);
    let exe = st2k_exe();
    compress_batch_report(exe.as_deref(), &imgs, size.target_bytes())
}

/// The byte-target half of [`handle_compress_to_size`], split out so the shortfall
/// aggregation below is testable against a REAL unmeetable target (no `CompressSize`
/// preset is small enough to trigger one - the smallest is 1 MB).
///
/// Per audit F32 (2026-09-05): `compress_to_size` refuses an unmeetable target and names
/// the smallest size it could reach ([`compress_to_size`]'s doc comment); the CLI/MCP
/// `compress` tool surface that text verbatim, but this verb used to just log the error
/// and report a generic "couldn't compress some images" - a right-click on an unmeetable
/// target told the user nothing they could act on, unlike its CLI/MCP siblings. It now
/// names the same numbers, in the same units (bytes), that `st2k compress` would.
pub(super) fn compress_batch_report(
    exe: Option<&Path>,
    imgs: &[String],
    target: u64,
) -> ActionReport {
    let results = st2k_base::parallel::map(imgs, |_, p| compress_one(exe, p, target));
    let attempted = imgs.len();
    let mut outs = Vec::with_capacity(results.len());
    // The largest per-image "smallest reachable" size across the failures that were a size
    // refusal (the error text carries the marker). A failure that was NOT a size refusal
    // (decode/write error, `None`) leaves this untouched and only sets `generic_failure`,
    // so a shortfall note is only built when at least one failure really was about size.
    let mut worst_achievable: Option<u64> = None;
    let mut generic_failure = false;
    for r in results {
        match r {
            Ok(p) => outs.push(p),
            Err(Some(achievable)) => {
                worst_achievable = Some(match worst_achievable {
                    Some(w) => w.max(achievable),
                    None => achievable,
                });
            }
            Err(None) => generic_failure = true,
        }
    }
    let done = outs.len();
    let first = outs.into_iter().next();
    let mut rep = ActionReport::applied(attempted, done);
    if let Some(achievable) = worst_achievable {
        rep.note = Some(compress_shortfall_note(
            target,
            achievable,
            attempted - done,
        ));
    } else if generic_failure {
        rep.note = Some("couldn't compress some images".into());
    }
    rep.output = first;
    rep
}

/// One image's compress attempt for [`compress_batch_report`]'s batch map. `Ok` carries the
/// written "(compressed)" sibling's path; `Err(Some(n))` carries the smallest byte count THIS
/// image could reach, parsed out of [`compress_to_size`]'s error text via
/// [`parse_smallest_achievable`]; `Err(None)` is a failure that was NOT a size refusal (a
/// decode/write error, or error text of an unexpected shape) - the batch then does not
/// attribute the failure to size.
pub(super) fn compress_one_to_size(
    path: &str,
    target: u64,
) -> std::result::Result<PathBuf, Option<u64>> {
    compress_to_size(path, target).map_err(|e| {
        let msg = e.to_string();
        st2k_base::safety::log(&format!("Compress to size failed for {path}: {msg}"));
        parse_smallest_achievable(&msg)
    })
}

/// Pull the "the smallest JPEG this can make is N bytes" number out of
/// [`compress_to_size`]'s unmeetable-target error text (see its doc comment for the exact
/// wording). `None` for any other failure (decode error, write failure, ...) or if the text
/// doesn't match - callers treat `None` as "this failure was not a size refusal", not as a
/// size number. Pure and panic-free: worst case on malformed input is `None`.
pub(super) fn parse_smallest_achievable(message: &str) -> Option<u64> {
    const MARKER: &str = "the smallest JPEG this can make is ";
    let after = message.split_once(MARKER)?.1;
    let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        None
    } else {
        digits.parse().ok()
    }
}

/// The Explorer verb's note when one or more images in the batch couldn't be compressed
/// under `target_bytes` - same wording and units (bytes) as the CLI/MCP `compress` error
/// text, so a right-click failure reads the same story as `st2k compress`'s: the target
/// that couldn't be met, and the smallest size that WAS reachable. See
/// [`compress_batch_report`] for how `achievable_bytes` is picked across a multi-file batch.
pub(super) fn compress_shortfall_note(
    target_bytes: u64,
    achievable_bytes: u64,
    failed: usize,
) -> String {
    let plural = if failed == 1 { "image" } else { "images" };
    format!(
        "cannot fit {failed} {plural} in {target_bytes} bytes: the smallest reachable was \
         {achievable_bytes} bytes. Ask for at least {achievable_bytes} bytes."
    )
}
