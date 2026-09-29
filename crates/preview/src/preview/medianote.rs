//! Why a video plays in silence or shows only a still picture: the stream Windows has no decoder
//! for, named in the caption (issue #49). Media Foundation plays what it can and says nothing
//! about the rest, so an MKV with DTS sound (no Windows decoder exists) played silently and one
//! with HEVC video (a Store extension) fell back to a still frame, with no word why; a user with
//! a codec pack installed reasonably blamed us, since those packs plug into DirectShow, which
//! Media Foundation never asks.

use std::ffi::c_void;

use st2k_base::i18n::t;
use st2k_codecs::vcodec::MissingDecoder;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT};

use super::window::{invalidate_caption, state, WM_APP_MEDIANOTE};

/// Ask Media Foundation about `path` off the UI thread (it opens the file) and post the note for
/// load generation `gen`, if there is one. A note that lands after the user has moved on is
/// dropped by [`on_note`].
pub(super) fn spawn(hwnd: HWND, path: String, gen: u64) {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::spawn(move || {
        let _com = st2k_base::parallel::ComGuard::mta();
        if let Some(note) = note_for(&st2k_codecs::vcodec::missing_decoders(&path)) {
            let hwnd = HWND(hwnd_raw as *mut c_void);
            unsafe {
                super::content::post_boxed(hwnd, WM_APP_MEDIANOTE, gen, Box::new((gen, note)))
            };
        }
    });
}

/// The note arrived: keep it if it is still for the file on screen, and repaint the caption.
pub(super) unsafe fn on_note(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let boxed = Box::from_raw(lparam.0 as *mut (u64, String));
    let (gen, note) = *boxed;
    let st = &*state(hwnd);
    if gen == st.decode_gen.get() {
        *st.media_note.borrow_mut() = Some(note);
        invalidate_caption(hwnd);
    }
    LRESULT(0)
}

/// The caption note for these missing decoders: the picture first (the bigger loss, and the one
/// a Store extension can fix), else the sound. `None` when nothing is missing.
pub(super) fn note_for(missing: &[MissingDecoder]) -> Option<String> {
    let pick = missing
        .iter()
        .find(|m| !m.audio)
        .or_else(|| missing.first())?;
    let key = match (pick.audio, pick.codec, pick.store_extension) {
        (false, Some(_), Some(_)) => "preview_note_no_picture_store",
        (false, Some(_), None) => "preview_note_no_picture",
        (false, None, _) => "preview_note_no_picture_unknown",
        (true, Some(_), _) => "preview_note_no_sound",
        (true, None, _) => "preview_note_no_sound_unknown",
    };
    Some(
        t(key)
            .replace("{codec}", pick.codec.unwrap_or_default())
            .replace("{extension}", pick.store_extension.unwrap_or_default()),
    )
}
