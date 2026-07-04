//! Embed VERSIONINFO into sagethumbs2k.dll so right-click -> Properties -> Details
//! shows a file version (critical for telling which build a given dllhost.exe loaded).
//!
//! Self-contained on purpose: build scripts can't share code across crates, so this
//! mirrors the `versioninfo_rc` helper in the core crate's `build/build.rs` (which now
//! only emits the EXE resources). Best-effort: if OUT_DIR/windres is unavailable, emit
//! a `cargo:warning` and move on — the DLL just lacks a version (REPORTED, never fatal).

fn main() {
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
    }

    let rc = versioninfo_rc("SageThumbs 2K shell extension", "sagethumbs2k.dll");
    if std::fs::write(format!("{out}/dll_version.rc"), rc).is_err() {
        println!("cargo:warning=DLL VERSIONINFO: couldn't write dll_version.rc; DLL will have no version");
        return;
    }
    let obj = format!("{out}/dll_version.o");
    for windres in ["windres", "x86_64-w64-mingw32-windres"] {
        let status = std::process::Command::new(windres)
            .args(["-I", &out, &format!("{out}/dll_version.rc"), "-O", "coff", "-o", &obj])
            .status();
        if matches!(status, Ok(s) if s.success()) {
            // This crate is cdylib-only, so `-arg` reaches the DLL (no bins to confuse).
            println!("cargo:rustc-link-arg={obj}");
            return;
        }
    }
    println!(
        "cargo:warning=DLL VERSIONINFO: windres unavailable; sagethumbs2k.dll will have no \
         file version. Install binutils/llvm-windres to enable it."
    );
}

/// A Windows `VERSIONINFO` `.rc` with FileVersion / ProductVersion pinned to
/// `CARGO_PKG_VERSION` (the shared workspace version). Mirrors the core build.rs.
fn versioninfo_rc(file_desc: &str, orig_name: &str) -> String {
    let ver = std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into());
    let mut nums = [0u32; 3];
    for (i, part) in ver.split(['.', '-', '+']).take(3).enumerate() {
        nums[i] = part.parse().unwrap_or(0);
    }
    let (maj, min, pat) = (nums[0], nums[1], nums[2]);
    format!(
        "1 VERSIONINFO\n\
         FILEVERSION {maj},{min},{pat},0\n\
         PRODUCTVERSION {maj},{min},{pat},0\n\
         FILEOS 0x40004\n\
         FILETYPE 0x2\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20\x20\x20 BLOCK \"040904b0\"\n\
         \x20\x20\x20 BEGIN\n\
         \x20\x20\x20\x20\x20 VALUE \"CompanyName\", \"LunarWerx\"\n\
         \x20\x20\x20\x20\x20 VALUE \"FileDescription\", \"{file_desc}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"FileVersion\", \"{ver}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"InternalName\", \"SageThumbs2K\"\n\
         \x20\x20\x20\x20\x20 VALUE \"LegalCopyright\", \"(C) 2026 LunarWerx\"\n\
         \x20\x20\x20\x20\x20 VALUE \"OriginalFilename\", \"{orig_name}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"ProductName\", \"SageThumbs 2K\"\n\
         \x20\x20\x20\x20\x20 VALUE \"ProductVersion\", \"{ver}\"\n\
         \x20\x20\x20 END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20\x20\x20 VALUE \"Translation\", 0x409, 1200\n\
         \x20 END\n\
         END\n",
    )
}
