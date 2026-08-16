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
    MF_MEDIA_ENGINE_CALLBACK, MF_MEDIA_ENGINE_EVENT_CANPLAY, MF_MEDIA_ENGINE_EVENT_ERROR,
    MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA, MF_MEDIA_ENGINE_PLAYBACK_HWND, MF_VERSION,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, PostMessageW, SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER,
    WINDOW_EX_STYLE, WM_APP, WS_CHILD, WS_VISIBLE,
};

/// Posted to the viewer on an engine event (WPARAM = the `MF_MEDIA_ENGINE_EVENT` value).
pub(super) const WM_APP_VIDEO: u32 = WM_APP + 6;

/// `CLSID_MFMediaEngineClassFactory` (not surfaced as a const by windows-rs at this path).
const CLSID_MF_MEDIA_ENGINE_CLASS_FACTORY: windows::core::GUID =
    windows::core::GUID::from_u128(0xb44392da_499b_446b_a4cb_005fead0e6d5);

/// What the viewer must DO about an engine event. The wndproc holds `st.video.borrow()` while it
/// dispatches, so the player can never tear itself down or resize the window from inside
/// [`VideoPlayer::on_event`]; it reports what happened and the caller acts once the borrow is gone.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum VideoEvent {
    /// Nothing the viewer needs to do (the common case: buffering, timeupdate, seeked, ...).
    None,
    /// Metadata is loaded, so [`VideoPlayer::native_size`] now answers. The viewer re-sizes itself
    /// to the clip's REAL aspect at this point; until now it only had a placeholder 16:9 shell.
    Metadata,
    /// The engine failed AFTER opening the source (a codec it could not actually decode, a
    /// truncated file). Nothing will ever render, so the viewer must drop the player and fall back
    /// to the still-frame decode path rather than sit on a permanently blank surface.
    Error,
}

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
    /// Loop + speed, mirrored here for the same reason as `vol`: the transport strip paints from
    /// them, and a freshly-loaded source resets the engine's own rate, so both are re-asserted at
    /// CANPLAY.
    looping: Cell<bool>,
    /// Playback rate as a multiplier (1.0 = normal). Persisted as a percent, see `settings`.
    speed: Cell<f64>,
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

/// Start Media Foundation once per process (lite = no socket/full init). Returns whether
/// the (first, and only) attempt succeeded, so a caller whose MFStartup failed can bail
/// before wasting a CoCreateInstance on a subsystem that was never initialized, instead of
/// discovering that indirectly as a generic "engine creation failed".
fn ensure_mf() -> bool {
    use std::sync::OnceLock;
    static OK: OnceLock<bool> = OnceLock::new();
    *OK.get_or_init(|| unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE).is_ok() })
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
    // CoCreateInstance(CLSID_MF_MEDIA_ENGINE_CLASS_FACTORY) below wants a COM-initialized
    // calling thread. It has worked without this only because that class factory happens
    // to be free-threaded today; webview.rs inits COM on this same preview UI thread but
    // only when HTML content loads, so a plain video/audio file never got it. Idempotent
    // (S_FALSE if already STA) and never uninitialized, same as webview.rs, leaving the
    // apartment for the thread's life.
    let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    if !ensure_mf() {
        return None;
    }
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
    let looping = sagethumbs2k_core::settings::preview_loop();
    let speed = (sagethumbs2k_core::settings::preview_speed() as f64 / 100.0).clamp(0.25, 4.0);
    let _ = engine.SetLoop(looping);
    let _ = engine.SetPlaybackRate(speed);
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
        looping: Cell::new(looping),
        speed: Cell::new(speed),
        _notify: notify,
    })
}

impl VideoPlayer {
    /// Handle an engine event forwarded from the notify callback, reporting what the VIEWER must
    /// do about it (see [`VideoEvent`] for why this returns rather than acts).
    pub(super) unsafe fn on_event(&self, event: u32) -> VideoEvent {
        if event == MF_MEDIA_ENGINE_EVENT_CANPLAY.0 as u32 {
            // Re-assert the wanted level from our own copy, not from `settings`: a source that has
            // just finished loading can come up at the engine's default, and by the time CANPLAY
            // arrives the user may already have moved the slider on this very clip.
            let _ = self.engine.SetVolume(self.vol.get());
            let _ = self.engine.SetMuted(self.muted.get());
            let _ = self.engine.SetLoop(self.looping.get());
            let _ = self.engine.SetPlaybackRate(self.speed.get());
            let _ = self.engine.Play(); // autoplay once buffered
            return VideoEvent::None;
        }
        if event == MF_MEDIA_ENGINE_EVENT_LOADEDMETADATA.0 as u32 {
            return VideoEvent::Metadata;
        }
        if event == MF_MEDIA_ENGINE_EVENT_ERROR.0 as u32 {
            return VideoEvent::Error;
        }
        VideoEvent::None
    }

    /// The clip's real presentation size in pixels, once metadata has loaded. `None` for an audio
    /// track (no video stream, so the engine reports 0x0) and before LOADEDMETADATA arrives.
    ///
    /// This is the DISPLAY size, so a phone clip carrying a 90-degree rotation flag reports its
    /// rotated (portrait) dimensions and the viewer sizes itself portrait. That is the whole point:
    /// without it every clip opened into the same 16:9 shell and portrait video sat letterboxed.
    pub(super) unsafe fn native_size(&self) -> Option<(i32, i32)> {
        let (mut cx, mut cy) = (0u32, 0u32);
        self.engine
            .GetNativeVideoSize(Some(&mut cx), Some(&mut cy))
            .ok()?;
        if cx == 0 || cy == 0 {
            return None;
        }
        Some((cx as i32, cy as i32))
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

    /// Whether the clip repeats at the end (our mirror, see the `looping` field).
    pub(super) fn looping(&self) -> bool {
        self.looping.get()
    }

    /// Flip repeat on/off. With loop OFF the engine simply stops on the last frame.
    pub(super) unsafe fn set_looping(&self, on: bool) {
        self.looping.set(on);
        let _ = self.engine.SetLoop(on);
    }

    /// Playback rate as a multiplier (our mirror, see the `speed` field).
    pub(super) fn speed(&self) -> f64 {
        self.speed.get()
    }

    /// Set the playback rate. Media Foundation clamps what it will actually honour per codec, so a
    /// refused rate leaves playback at the previous speed rather than failing the clip.
    pub(super) unsafe fn set_speed(&self, mult: f64) {
        let m = mult.clamp(0.25, 4.0);
        self.speed.set(m);
        let _ = self.engine.SetPlaybackRate(m);
    }

    /// Nudge the position by `secs` (negative rewinds), clamped inside the clip. No-op until the
    /// duration is known, which is also what stops a keyboard seek from working on a live stream.
    pub(super) unsafe fn seek_by(&self, secs: f64) {
        let dur = self.duration();
        if !dur.is_finite() || dur <= 0.0 {
            return;
        }
        let cur = self.current_time();
        let cur = if cur.is_finite() { cur } else { 0.0 };
        self.seek((cur + secs).clamp(0.0, dur));
    }

    /// Nudge the volume by `delta` (0..=1 scale), un-muting on the way up so the key does what it
    /// looks like it does rather than silently raising a muted level.
    pub(super) unsafe fn nudge_volume(&self, delta: f64) {
        let v = (self.vol.get() + delta).clamp(0.0, 1.0);
        self.set_volume(v);
        if v > 0.0 {
            self.set_muted(false);
        }
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
