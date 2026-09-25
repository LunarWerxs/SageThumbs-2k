//! Embed VERSIONINFO into sagethumbs2k.dll so right-click -> Properties -> Details
//! shows a file version (critical for telling which build a given dllhost.exe loaded).
//!
//! The SDK-rc lookup, the windres dispatch and the VERSIONINFO `.rc` template live in the
//! shared `build-support` crate (a `[build-dependencies]`-only crate under
//! `crates/build-support`, so none of it reaches the shipped DLL) - build scripts can't share
//! code across crates any other way. Best-effort: if the `.rc` cannot be written, or windres
//! is unavailable on x64, emit a `cargo:warning` and move on - the DLL just lacks a version
//! (REPORTED, never fatal). The one refusal is ARM64 without the SDK `rc.exe`, which has no
//! fallback compiler at all.

fn main() {
    delay_load_media_foundation();
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let out = match std::env::var("OUT_DIR") {
        Ok(o) => o,
        Err(_) => return,
    };

    // The app EXE's bin target is named `SageThumbs2K` (so `cargo build` emits
    // `SageThumbs2K.exe` directly). That basename case-folds to THIS cdylib's default
    // `sagethumbs2k.pdb` on Windows' case-insensitive FS, so a combined debug/test build
    // used to die with LNK1201 (two concurrent links contending for one PDB file).
    // Redirect the CDYLIB's PDB (a single artifact — unlike the bin, it has no `--test`
    // twin) to a distinct name so nothing case-collides. `-cdylib` so it can't touch this
    // crate's test harness. MSVC-only; harmless in release (no debuginfo → no PDB written).
    // Fixes both `cargo build` and `cargo test`; see Cargo.toml `[[bin]]` note in the core crate.
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg-cdylib=/PDB:{out}\\sagethumbs2k_dll.pdb");
        // MSVC recommends marking the four standard COM server exports PRIVATE in
        // a hand-written .def so they stay out of the import library. rustc generates
        // the cdylib .def for us and offers no per-export PRIVATE control; the exports
        // are intentionally public to LoadLibrary/GetProcAddress callers. Silence only
        // that known LNK4104 diagnostic rather than muting linker warnings globally.
        println!("cargo:rustc-link-arg-cdylib=/IGNORE:4104");
    }

    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let rc = build_support::versioninfo_rc(
        "SageThumbs 2K shell extension",
        "sagethumbs2k.dll",
        &ver,
        build_support::FileType::Dll,
    );
    use build_support::RcFailure;
    match build_support::compile_rc(&out, "dll_version", &rc) {
        // This crate is cdylib-only, so `-arg` reaches the DLL (no bins to confuse).
        Ok(arg) => println!("cargo:rustc-link-arg={arg}"),
        Err(RcFailure::NoSdkRc) => panic!(
            "ARM64 resource compilation requires Windows SDK rc.exe; refusing a \
             version-metadata-free shell DLL"
        ),
        Err(RcFailure::Write(why)) => {
            println!("cargo:warning=DLL VERSIONINFO: {why}; DLL will have no version")
        }
        Err(RcFailure::Windres(why)) => println!(
            "cargo:warning=DLL VERSIONINFO: sagethumbs2k.dll will have no file version.\n  \
             {why}\n  Install binutils/llvm-windres (or put it on PATH) to enable it."
        ),
    }
}

/// Delay-load Media Foundation (`mfplat.dll` / `mfreadwrite.dll`).
///
/// By default these are STATIC imports, so the Windows loader resolves them before any of
/// our code runs and REFUSES to load the binary at all if they are missing. They ARE
/// missing on the "N" and "KN" Windows editions (sold in the EU and Korea without media
/// features) and on Server core installs. There, the shell extension would fail to load
/// entirely: no thumbnails for ANY of the 300+ formats, no context menu, no property
/// handler, and no error message anywhere explaining why. One optional tier (video frame
/// grabbing) must not be able to take the whole product down.
///
/// Delay-loading defers resolution to the first actual CALL, so the video tier degrades to
/// "unavailable" and everything else keeps working. Every MF call site is gated on
/// `video::media_foundation_available()`, because a delay-load stub for a DLL that cannot
/// be found raises a STRUCTURED EXCEPTION, and this crate builds with `panic = "abort"` --
/// an unguarded call would kill the host process instead of degrading.
///
/// (Mirrored in the other package's build script, which scopes each flag to its two bins;
/// here they stay package-wide.)
fn delay_load_media_foundation() {
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    for dll in ["mfplat.dll", "mfreadwrite.dll"] {
        println!("cargo:rustc-link-arg=/DELAYLOAD:{dll}");
    }
    // /DELAYLOAD is inert without the helper that performs the deferred resolution.
    println!("cargo:rustc-link-arg=delayimp.lib");
    // The package-wide `-arg` above also reaches this crate's test harness, whose
    // dead-code elimination may remove every MF import: the benign
    // "delay-load DLL ignored; no imports found" case.
    println!("cargo:rustc-link-arg=/IGNORE:4199");
}
