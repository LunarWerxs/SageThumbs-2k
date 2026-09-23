//! Command-line / agent API — the verbs the `st2k` console binary exposes.
//!
//! Every verb reuses the exact same engine the shell extension uses (every format
//! we decode via `decode_full`, the convert/rotate/strip/OCR/PDF logic), so an
//! installed SageThumbs 2K doubles as an offline image toolbox for scripts and
//! AI agents — no extra installs. Each verb returns `Ok(stdout text)` or
//! `Err(message)`; the binary prints and maps to an exit code.
//!
//! Split into a directory module (2026-09-14): this hub keeps the registration/devmode
//! verbs; `convert.rs` (thumbnail/convert/resize/rotate/compress/PDF/CBZ), `batch.rs`
//! (the batch runner + prebuild + input expansion), `report.rs` (info/formats/bench) and
//! `actions.rs` (clipboard/wallpaper/folder-icon/strip/OCR/upload) hold the rest.
//! `helpers.rs` carries the shared output-alias/extension/atomic-write/size-parsing code.
//! Parent-hub import model: each child does `use super::*;`, and this hub does a private
//! `use child::*;`, re-exporting the public names by name below.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::E_FAIL;

use st2k_actions::{topdf, verbs};
use st2k_base::{formats, settings};
use st2k_codecs::{decode, ocr, strip};

mod actions;
mod batch;
mod convert;
mod helpers;
mod report;

use convert::*;
use helpers::*;

pub use actions::{
    clip_pixels, folder_icon, ocr, strip_meta, upload, upload_history, upload_hosts,
    wallpaper_prepare,
};
pub use batch::{batch, prebuild};
pub use convert::{cbz, compress, convert, pdf, rotate, thumbnail, view_png, CombineOpts};
pub use helpers::{parse_resize, parse_size};
pub use report::{bench_decode, info, list_formats};

/// Turn Explorer thumbnails on/off for THIS USER, pointing at the DLL shipped beside this
/// exe. This is what makes the portable zip more than a bag of tools: the handler is COM, so
/// it has to be registered somewhere, and `HKCU\Software\Classes` is the somewhere that needs
/// no installer and no admin. See `register::register_user` for what a per-user registration
/// can and cannot cover.
pub fn register_portable(off: bool, status: bool) -> Result<String, String> {
    let current = crate::register::user_registration_path();

    if status {
        return Ok(match current {
            Some(p) => format!("Explorer thumbnails: ON for this user\n  handler: {p}"),
            None => "Explorer thumbnails: OFF for this user".into(),
        });
    }

    if off {
        crate::register::unregister_user().map_err(|e| format!("could not unregister: {e}"))?;
        return Ok("Explorer thumbnails turned OFF for this user.".into());
    }

    // PORTABLE ONLY, and "the DLL is beside us" is NOT a good enough test for that: an installed
    // build has sagethumbs2k.dll right next to st2k.exe in Program Files, so the exists-check
    // below passes there too. Registering from an installed copy writes OUR CLSID into
    // HKCU\Software\Classes, which the shell merges AHEAD of the machine-wide view — and the
    // uninstaller only removes HKCU\Software\SageThumbs2K (the settings), never these class keys.
    // The result is a per-user handler that outlives the uninstall, still pointing at a deleted
    // Program Files DLL, silently killing thumbnails for that user with nothing to blame.
    if !settings::portable() {
        return Err(
            "this is an installed copy, which already registers thumbnails machine-wide.\n\
             `st2k register` is for the portable zip only — using it here would leave a per-user \
             registration behind that survives uninstall and blocks thumbnails.\n\
             Use Settings ▸ Diagnostics ▸ Repair file associations instead.\n\
             (If a previous run already did this, `st2k register --off` clears it.)"
                .into(),
        );
    }

    let exe = std::env::current_exe().map_err(|e| format!("could not locate this exe: {e}"))?;
    let dll = exe
        .parent()
        .ok_or("this exe has no parent directory")?
        .join("sagethumbs2k.dll");
    if !dll.exists() {
        return Err(format!(
            "{} is not here.\nThis verb is for the portable zip, which ships that DLL beside the exes.",
            dll.display()
        ));
    }
    let dll = dll.to_string_lossy().into_owned();

    crate::register::register_user(&dll).map_err(|e| format!("could not register: {e}"))?;
    let mut out = format!("Explorer thumbnails turned ON for this user.\n  handler: {dll}");
    // Moving the folder later leaves the keys aimed at a path that no longer exists, and the
    // symptom is thumbnails quietly not drawing, so say it once here where it is cheap.
    out.push_str("\n\nIf you move or delete this folder, run `st2k register --off` first.");
    Ok(out)
}

/// `st2k devmode on|off|status`: toggle the developer-test-box flag (the HKCU `DevMachine`
/// value). When ON, this machine's startup manifest request carries `&dev=1`. A plain
/// machine-local flag, not an identifier; OFF on every real install.
pub fn devmode(sub: &str) -> Result<String, String> {
    match sub {
        "on" | "enable" | "1" => {
            settings::set_dev_machine(true)
                .map_err(|_| "couldn't write the DevMachine flag".to_string())?;
            Ok("dev mode ON (this machine's manifest request carries &dev=1).".into())
        }
        "off" | "disable" | "0" => {
            settings::set_dev_machine(false)
                .map_err(|_| "couldn't clear the DevMachine flag".to_string())?;
            Ok("dev mode OFF (this machine's manifest request is unmodified).".into())
        }
        "status" | "" => Ok(format!(
            "dev mode is {} (HKCU\\Software\\SageThumbs2K\\DevMachine)",
            if settings::is_dev_machine() {
                "ON"
            } else {
                "OFF"
            }
        )),
        other => Err(format!(
            "unknown devmode '{other}' (use: on | off | status)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A broad smoke test spanning the thumbnail/convert verbs (`convert.rs`) and the
    /// info/formats reporting verbs (`report.rs`) together — kept at the hub rather than
    /// split, since it is not specific to either module's own internal logic.
    #[test]
    fn cli_thumbnail_and_info_and_formats() {
        let dir = std::env::temp_dir().join(format!("st2k_cli_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("a.png");
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(400, 300))
            .save(&src)
            .unwrap();
        let sp = src.to_str().unwrap();

        let out = dir.join("t.png");
        thumbnail(sp, out.to_str().unwrap(), 128).unwrap();
        let d = image::open(&out).unwrap();
        assert!(d.width() <= 128 && d.height() <= 128 && d.width() == 128);

        let cv = dir.join("a.jpg");
        convert(
            sp,
            cv.to_str().unwrap(),
            85,
            None,
            verbs::Resize::Fit(100, 100),
            false,
        )
        .unwrap();
        assert!(image::open(&cv).unwrap().width() <= 100);

        assert!(info(sp, true).unwrap().contains("\"width\":400"));
        assert!(list_formats(false).contains(".png"));
        assert!(list_formats(true).starts_with('['));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
