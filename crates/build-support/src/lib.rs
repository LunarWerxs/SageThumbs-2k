//! Shared Windows resource-compilation helpers for this workspace's three build scripts
//! (`src/build.rs`, `crates/dll/build.rs`, `crates/dlghook/build.rs`). This crate is consumed
//! only via `[build-dependencies]`, so nothing here reaches a shipped binary.
//!
//! Extracted because the SDK `rc.exe` lookup, the windres/SDK-rc dispatch, and the
//! VERSIONINFO `.rc` template were byte-for-byte duplicated across all three build scripts.
//! Build scripts can't `use` each other directly (they aren't part of the normal crate
//! graph), so a shared `[build-dependencies]` crate is the only way to de-duplicate them.
//!
//! The three original copies were identical except for one field: `versioninfo_rc`'s
//! `FILETYPE` value (`VFT_APP` for the two EXEs built by `src/build.rs`, `VFT_DLL` for the
//! two DLLs built by `crates/dll` and `crates/dlghook`). That is now the `FileType`
//! parameter below; the version string itself is also a plain parameter rather than being
//! read from `CARGO_PKG_VERSION` inside this crate, which keeps every function here free of
//! environment access and independently testable.

use std::path::PathBuf;

/// The Windows `VERSIONINFO` `FILETYPE` field. The three original build scripts differed
/// only here: `src/build.rs` emits `VFT_APP` for its two EXE bin targets, while both DLL
/// build scripts (`crates/dll`, `crates/dlghook`) emit `VFT_DLL`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileType {
    /// VFT_APP - an executable.
    App,
    /// VFT_DLL - a dynamic-link library.
    Dll,
}

impl FileType {
    fn rc_value(self) -> &'static str {
        match self {
            FileType::App => "0x1",
            FileType::Dll => "0x2",
        }
    }
}

/// Build a Windows `VERSIONINFO` resource statement (as `.rc` text) with FileVersion /
/// ProductVersion pinned to `version`, CompanyName `LunarWerx`, ProductName
/// `SageThumbs 2K`. `file_desc` is the per-artifact FileDescription, `orig_name` the
/// OriginalFilename, and `file_type` the VERSIONINFO FILETYPE (the one field that differed
/// between the three original copies - see the module docs). The four numeric version
/// fields come from the `MAJOR.MINOR.PATCH` cargo version (4th field always 0); callers
/// pass `CARGO_PKG_VERSION` (falling back to `"0.0.0"` if unset) as `version`.
pub fn versioninfo_rc(
    file_desc: &str,
    orig_name: &str,
    version: &str,
    file_type: FileType,
) -> String {
    // Split "MAJOR.MINOR.PATCH[-pre]" → numeric quad "MAJOR,MINOR,PATCH,0".
    let mut nums = [0u32; 3];
    for (i, part) in version.split(['.', '-', '+']).take(3).enumerate() {
        nums[i] = part.parse().unwrap_or(0);
    }
    let (maj, min, pat) = (nums[0], nums[1], nums[2]);
    let file_type = file_type.rc_value();
    // \r\n in the .rc string keeps rc.exe/windres happy; the version string shown
    // in Properties is the human-readable cargo version (incl. any -pre suffix).
    format!(
        "1 VERSIONINFO\n\
         FILEVERSION {maj},{min},{pat},0\n\
         PRODUCTVERSION {maj},{min},{pat},0\n\
         FILEOS 0x40004\n\
         FILETYPE {file_type}\n\
         BEGIN\n\
         \x20 BLOCK \"StringFileInfo\"\n\
         \x20 BEGIN\n\
         \x20\x20\x20 BLOCK \"040904b0\"\n\
         \x20\x20\x20 BEGIN\n\
         \x20\x20\x20\x20\x20 VALUE \"CompanyName\", \"LunarWerx\"\n\
         \x20\x20\x20\x20\x20 VALUE \"FileDescription\", \"{file_desc}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"FileVersion\", \"{version}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"InternalName\", \"SageThumbs2K\"\n\
         \x20\x20\x20\x20\x20 VALUE \"LegalCopyright\", \"(C) 2026 LunarWerx\"\n\
         \x20\x20\x20\x20\x20 VALUE \"OriginalFilename\", \"{orig_name}\"\n\
         \x20\x20\x20\x20\x20 VALUE \"ProductName\", \"SageThumbs 2K\"\n\
         \x20\x20\x20\x20\x20 VALUE \"ProductVersion\", \"{version}\"\n\
         \x20\x20\x20 END\n\
         \x20 END\n\
         \x20 BLOCK \"VarFileInfo\"\n\
         \x20 BEGIN\n\
         \x20\x20\x20 VALUE \"Translation\", 0x409, 1200\n\
         \x20 END\n\
         END\n",
    )
}

/// Compile an architecture-neutral Windows `.res` with the SDK resource compiler.
/// GNU windres installations on x64 emit x64 COFF objects even for ARM targets;
/// `link.exe` can instead consume this `.res` while producing the final ARM64 PE.
pub fn compile_with_windows_sdk_rc(input: &str, output: &str) -> bool {
    windows_sdk_rc_candidates().into_iter().any(|rc| {
        let status = std::process::Command::new(rc)
            .args(["/nologo", &format!("/fo{output}"), input])
            .status();
        matches!(status, Ok(s) if s.success())
    })
}

/// Every place an SDK `rc.exe` might be found, most-specific first: a bare `rc.exe`
/// (already on PATH), then every `Windows Kits\10\bin\<version>\<host>\rc.exe` under
/// `%ProgramFiles(x86)%`, newest version first.
pub fn windows_sdk_rc_candidates() -> Vec<PathBuf> {
    let mut candidates = vec![PathBuf::from("rc.exe")];
    let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") else {
        return candidates;
    };
    let sdk_bin = PathBuf::from(program_files_x86).join("Windows Kits/10/bin");
    let Ok(entries) = std::fs::read_dir(sdk_bin) else {
        return candidates;
    };
    let host = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let mut versions: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join(host).join("rc.exe"))
        .filter(|path| path.is_file())
        .collect();
    versions.sort_by(|a, b| b.cmp(a));
    candidates.extend(versions);
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Locks the version-quadruple formatting (MAJOR,MINOR,PATCH,0 in both FILEVERSION and
    /// PRODUCTVERSION) plus the FILETYPE substitution for the App case. No env access, so
    /// this can't race with anything else in the test binary.
    #[test]
    fn versioninfo_rc_formats_the_version_quadruple() {
        let rc = versioninfo_rc("Some App", "some.exe", "0.0.0", FileType::App);
        assert!(rc.contains("FILEVERSION 0,0,0,0"), "{rc}");
        assert!(rc.contains("PRODUCTVERSION 0,0,0,0"), "{rc}");
        assert!(rc.contains("FILETYPE 0x1"), "{rc}");
        assert!(
            rc.contains("VALUE \"FileDescription\", \"Some App\""),
            "{rc}"
        );
        assert!(
            rc.contains("VALUE \"OriginalFilename\", \"some.exe\""),
            "{rc}"
        );
    }

    /// The DLL FILETYPE (VFT_DLL) is the one field crates/dll and crates/dlghook needed
    /// that src/build.rs's EXE path did not.
    #[test]
    fn versioninfo_rc_dll_file_type() {
        let rc = versioninfo_rc("Some Dll", "some.dll", "0.0.0", FileType::Dll);
        assert!(rc.contains("FILETYPE 0x2"), "{rc}");
    }

    /// A real MAJOR.MINOR.PATCH-with-prerelease string parses to the right numeric quad and
    /// keeps the full string (incl. suffix) in the human-readable FileVersion/ProductVersion.
    #[test]
    fn versioninfo_rc_parses_a_real_version_string() {
        let rc = versioninfo_rc("App", "app.exe", "2.5.0-beta", FileType::App);
        assert!(rc.contains("FILEVERSION 2,5,0,0"), "{rc}");
        assert!(rc.contains("PRODUCTVERSION 2,5,0,0"), "{rc}");
        assert!(rc.contains("VALUE \"FileVersion\", \"2.5.0-beta\""), "{rc}");
    }
}
