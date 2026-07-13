//! Headless settings-window capture (run_shot / run_shot_gif) (extracted from settings_dlg; parent-hub pattern).

use super::*;

// ===== Headless self-capture (verification + README/site assets) =====

/// Build the Settings window OFF-SCREEN (invisible, steals no focus), returning the HWND once
/// its controls have realized + painted. Shared by `run_shot` (one pane → PNG) and
/// `run_shot_gif` (all panes → animated GIF).
pub(super) unsafe fn build_settings_shot_window(hinst: HINSTANCE, dark: bool) -> Option<HWND> {
    let hwnd = crate::win::create_shot_window(
        hinst,
        dark,
        w!("SageThumbs2KOptions"),
        Some(wndproc),
        "SageThumbs 2K — Settings",
        772,
        588,
    )?;
    // Let the controls — the ListView especially — realize + paint before we drive panes.
    crate::win::pump_msgs(20);
    Some(hwnd)
}

/// Switch to category `tab` and settle it for capture. The shared owner-drawn chrome (nav
/// rail, pane header, footer buttons) only fully repaints on a *real* category transition —
/// re-selecting the current tab is a no-op that leaves them blank in a headless grab. So we
/// PRIME with a switch to a different tab first, making the switch to `tab` a real transition,
/// then double `RDW_UPDATENOW` around pumps so every control has actually painted.
pub(super) unsafe fn settle_pane(hwnd: HWND, tab: usize) {
    let tab = tab.min(NCAT - 1);
    let prime = if tab == 0 { NCAT - 1 } else { 0 };
    switch_category(hwnd, prime);
    crate::win::pump_msgs(5);
    switch_category(hwnd, tab);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(12);
    crate::win::force_repaint(hwnd);
    crate::win::pump_msgs(4);
}

/// The app's `--shot` mode: build the Settings window off-screen, switch to category `tab`,
/// render it to a PNG at `out` via `PrintWindow`, then tear it down. Lets a UI change be
/// screenshotted programmatically — no window ever appears and the desktop is never driven.
/// Returns whether the PNG was written.
pub(crate) unsafe fn run_shot(hinst: HINSTANCE, dark: bool, out: &str, tab: usize) -> bool {
    let Some(hwnd) = build_settings_shot_window(hinst, dark) else {
        return false;
    };
    settle_pane(hwnd, tab);
    let ok = crate::screenshot::capture_hwnd_to_png(hwnd, std::path::Path::new(out));
    let _ = DestroyWindow(hwnd);
    ok
}

/// The app's `--shot-gif` mode: build the Settings window off-screen ONCE, walk every category
/// tab capturing each as a frame, and encode them into an animated (infinite-loop) GIF at
/// `out` — the regenerable README/site asset that cycles the Settings tabs. Frames are
/// downscaled to the 96-dpi design width so the GIF stays crisp + small. Returns whether the
/// GIF was written.
pub(crate) unsafe fn run_shot_gif(_hinst: HINSTANCE, _dark: bool, out: &str) -> bool {
    // Capture each frame in a FRESH PROCESS (`--shot --tab N`), then assemble the GIF from the
    // PNGs. This is deliberate: the single-shot path is the ONLY one that reliably renders every
    // tab correctly. Reusing one window across tabs in-process raced the owner-drawn nav-rail
    // highlight (the capture grabbed the PREVIOUS tab's selection off a trailing DWM surface),
    // and churning fresh windows in a tight in-process loop left some frames blank (each window
    // hadn't finished painting). A separate, fully-initialized process per frame sidesteps both.
    // ~1 s each; this is a rarely-run asset-regen path, so the extra spawns don't matter.
    use std::os::windows::process::CommandExt;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let mut frames = Vec::with_capacity(NCAT);
    for tab in 0..NCAT {
        let png = tmp.join(format!("st2k_gifframe_{pid}_{tab}.png"));
        let Some(png_s) = png.to_str() else { continue };
        let ok = std::process::Command::new(&exe)
            .args(["--shot", png_s, "--tab", &tab.to_string()])
            .creation_flags(sagethumbs2k_core::CREATE_NO_WINDOW)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            if let Ok(img) = image::open(&png) {
                frames.push(crate::screenshot::downscale_to_width(img.to_rgba8(), 772));
            }
        }
        let _ = std::fs::remove_file(&png);
    }
    if frames.is_empty() {
        return false;
    }
    // ~1.6 s per tab so a reader can take each pane in before it advances.
    crate::screenshot::encode_gif(&frames, std::path::Path::new(out), 1600)
}
