//! Video playback in the Quick preview viewer (Phase 5), via Media Foundation's
//! `IMFMediaEngine` in WINDOWED mode: the engine renders the decoded video into a child HWND we
//! place over the content area and plays the audio itself — using the OS's installed codecs, so
//! ZERO bundled bytes (same "use the OS" stance as the video frame-grab / WinRT-PDF tiers).
//! Autoplays + loops; a click toggles play/pause. AUDIO tracks ride the same engine and transport
//! strip (an audio file is a video with no picture), but their render child is created hidden so
//! the viewer can paint the track's embedded cover art in that space instead.

use core::cell::Cell;
use core::ffi::c_void;

use windows::core::{implement, Result, BSTR};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaEngine, IMFMediaEngineClassFactory, IMFMediaEngineNotify,
    IMFMediaEngineNotify_Impl, MFCreateAttributes, MFStartup, MFSTARTUP_LITE,
    MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_EVENT_CANPLAY, MF_MEDIA_ENGINE_PLAYBACK_HWND,
    MF_VERSION,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, PostMessageW, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
    WINDOW_EX_STYLE, WM_APP, WS_CHILD, WS_VISIBLE,
};

/// Posted to the viewer on an engine event (WPARAM = the `MF_MEDIA_ENGINE_EVENT` value).
pub(super) const WM_APP_VIDEO: u32 = WM_APP + 6;

/// `CLSID_MFMediaEngineClassFactory` (not surfaced as a const by windows-rs at this path).
const CLSID_MF_MEDIA_ENGINE_CLASS_FACTORY: windows::core::GUID =
    windows::core::GUID::from_u128(0xb44392da_499b_446b_a4cb_005fead0e6d5);

/// A live video player: the engine + its child render window + the kept-alive callback.
pub(super) struct VideoPlayer {
    engine: IMFMediaEngine,
    child: HWND,
    /// The volume / mute the VIEWER wants, mirrored out of the engine. Two reasons it lives here
    /// rather than being read back with `GetVolume`: the engine can come up at its own default
    /// once a freshly-set source finishes loading (so [`VideoPlayer::on_event`] re-asserts these
    /// at CANPLAY), and the transport strip paints from them, which keeps the slider showing what
    /// the user chose even in the window between `SetSource` and CANPLAY.
    vol: Cell<f64>,
    muted: Cell<bool>,
    _notify: IMFMediaEngineNotify,
}

#[implement(IMFMediaEngineNotify)]
struct Notify {
    hwnd: isize, // the viewer window (HWND isn't Send; ferry the raw handle)
}

impl IMFMediaEngineNotify_Impl for Notify_Impl {
    fn EventNotify(&self, event: u32, _param1: usize, _param2: u32) -> Result<()> {
        // Runs on an MF worker thread — do NOTHING but post to the UI thread.
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut c_void)),
                WM_APP_VIDEO,
                WPARAM(event as usize),
                LPARAM(0),
            );
        }
        Ok(())
    }
}

/// Start Media Foundation once per process (lite = no socket/full init).
fn ensure_mf() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    });
}

/// Create a player for `path`: a `WS_CHILD` render window over `rc` (client coords of `parent`),
/// with the engine set to render into it. Events post to `viewer` (WM_APP_VIDEO). Autoplay
/// happens on the CANPLAY event (see [`VideoPlayer::on_event`]).
///
/// `audio_only` marks a track with no video stream; its render window is created hidden so the
/// viewer can paint cover art in that space instead.
pub(super) unsafe fn create(
    parent: HWND,
    viewer: HWND,
    rc: &RECT,
    hinst: windows::Win32::Foundation::HINSTANCE,
    path: &str,
    audio_only: bool,
) -> Option<VideoPlayer> {
    // mfplat is delay-loaded (see the build scripts): on a Windows edition without Media
    // Foundation, calling MFStartup would raise a structured exception under `panic = "abort"`.
    // No player here just means the file shows as a card instead of playing.
    if !sagethumbs2k_core::video::media_foundation_available() {
        return None;
    }
    ensure_mf();
    // The child render window (plain STATIC; the engine owns the swap chain on it).
    //
    // For an audio track it is created HIDDEN: the engine still needs a real, correctly-sized
    // playback HWND, but an audio file has no video stream to draw into it, so leaving it visible
    // would do nothing except clip the parent and hide the cover-art backdrop the viewer paints
    // there instead. Hidden (rather than zero-sized) so the handle stays valid and `place` keeps
    // working unchanged.
    let child = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        windows::core::w!("STATIC"),
        windows::core::w!(""),
        if audio_only {
            WS_CHILD
        } else {
            WS_CHILD | WS_VISIBLE
        },
        rc.left,
        rc.top,
        rc.right - rc.left,
        rc.bottom - rc.top,
        Some(parent),
        None,
        Some(hinst),
        None,
    )
    .ok()?;

    let factory: IMFMediaEngineClassFactory = match CoCreateInstance(
        &CLSID_MF_MEDIA_ENGINE_CLASS_FACTORY,
        None,
        CLSCTX_INPROC_SERVER,
    ) {
        Ok(f) => f,
        Err(_) => {
            let _ = DestroyWindow(child);
            return None;
        }
    };

    let notify: IMFMediaEngineNotify = Notify {
        hwnd: viewer.0 as isize,
    }
    .into();
    let mut attrs: Option<IMFAttributes> = None;
    if MFCreateAttributes(&mut attrs, 2).is_err() {
        let _ = DestroyWindow(child);
        return None;
    }
    let attrs = attrs?;
    let _ = attrs.SetUnknown(&MF_MEDIA_ENGINE_CALLBACK, &notify);
    let _ = attrs.SetUINT64(&MF_MEDIA_ENGINE_PLAYBACK_HWND, child.0 as u64);

    let engine = match factory.CreateInstance(0, &attrs) {
        Ok(e) => e,
        Err(_) => {
            let _ = DestroyWindow(child);
            return None;
        }
    };
    let _ = engine.SetLoop(true);
    // Open at the level the transport strip was last left on, instead of the engine's full-volume
    // default — a player that forgets the volume you set on the previous file is the whole reason
    // this is persisted (see `settings::preview_volume`). Set BEFORE `SetSource` so the very first
    // audio frame is already at the right level; re-asserted at CANPLAY (see `on_event`).
    let vol = (sagethumbs2k_core::settings::preview_volume() as f64 / 100.0).clamp(0.0, 1.0);
    let muted = sagethumbs2k_core::settings::preview_muted();
    let _ = engine.SetVolume(vol);
    let _ = engine.SetMuted(muted);
    let url = BSTR::from(path);
    if engine.SetSource(&url).is_err() {
        let _ = engine.Shutdown();
        let _ = DestroyWindow(child);
        return None;
    }
    Some(VideoPlayer {
        engine,
        child,
        vol: Cell::new(vol),
        muted: Cell::new(muted),
        _notify: notify,
    })
}

impl VideoPlayer {
    /// Handle an engine event forwarded from the notify callback.
    pub(super) unsafe fn on_event(&self, event: u32) {
        if event == MF_MEDIA_ENGINE_EVENT_CANPLAY.0 as u32 {
            // Re-assert the wanted level from our own copy, not from `settings`: a source that has
            // just finished loading can come up at the engine's default, and by the time CANPLAY
            // arrives the user may already have moved the slider on this very clip.
            let _ = self.engine.SetVolume(self.vol.get());
            let _ = self.engine.SetMuted(self.muted.get());
            let _ = self.engine.Play(); // autoplay once buffered
        }
    }

    /// Reposition/resize the render window to `rc` (the engine follows the HWND in windowed mode).
    pub(super) unsafe fn place(&self, rc: &RECT) {
        let _ = SetWindowPos(
            self.child,
            None,
            rc.left,
            rc.top,
            rc.right - rc.left,
            rc.bottom - rc.top,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }

    // ---- transport controls (scrub bar + volume) — thin IMFMediaEngine wrappers ----

    /// Total duration in seconds; `NaN` until metadata has loaded (guard before seeking).
    pub(super) unsafe fn duration(&self) -> f64 {
        self.engine.GetDuration()
    }
    /// Current playback position in seconds.
    pub(super) unsafe fn current_time(&self) -> f64 {
        self.engine.GetCurrentTime()
    }
    /// Seek to `secs` (no-op if duration isn't known yet — caller should guard).
    pub(super) unsafe fn seek(&self, secs: f64) {
        let _ = self.engine.SetCurrentTime(secs);
    }
    pub(super) unsafe fn is_paused(&self) -> bool {
        self.engine.IsPaused().as_bool()
    }
    pub(super) unsafe fn toggle_play(&self) {
        if self.engine.IsPaused().as_bool() {
            let _ = self.engine.Play();
        } else {
            let _ = self.engine.Pause();
        }
    }
    /// The wanted volume (0..=1) — our mirror, see the `vol` field.
    pub(super) unsafe fn volume(&self) -> f64 {
        self.vol.get()
    }
    pub(super) unsafe fn set_volume(&self, v: f64) {
        let v = v.clamp(0.0, 1.0);
        self.vol.set(v);
        let _ = self.engine.SetVolume(v);
    }
    /// The wanted mute state — our mirror, see the `vol` field.
    pub(super) unsafe fn muted(&self) -> bool {
        self.muted.get()
    }
    pub(super) unsafe fn set_muted(&self, m: bool) {
        self.muted.set(m);
        let _ = self.engine.SetMuted(m);
    }
}

impl Drop for VideoPlayer {
    fn drop(&mut self) {
        unsafe {
            let _ = self.engine.Shutdown();
            let _ = DestroyWindow(self.child);
        }
    }
}
