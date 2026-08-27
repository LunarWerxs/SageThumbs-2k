//! Embed VERSIONINFO into `st2k_dlghook.dll`.
//!
//! Not cosmetic for this one. It is the binary that gets loaded into OTHER applications, it
//! is unsigned, and a shipped DLL with no company, product or version is exactly the shape a
//! scanner scores as suspicious. It also has to carry the release version because
//! `check-release-rust-payload.ps1` asserts every shipped Rust binary's VERSIONINFO matches
//! the version in Cargo.toml, which is what stops a stale artifact riding along in a release.
//!
//! Self-contained on purpose: build scripts cannot share code across crates, so this mirrors
//! `crates/dll/build.rs`'s helper. No delay-load and no PDB redirect here, because this crate
//! links nothing optional and its basename collides with nothing.

fn main() {
    if std::env::var_os("CARGO_CFG_WINDOWS").is_none() {
        return;
    }
    let Ok(out) = std::env::var("OUT_DIR") else {
        return;
    };
    let rc = versioninfo_rc(
        "SageThumbs 2K Open/Save dialog selection reader",
        "st2k_dlghook.dll",
    );
    let input = format!("{out}/dlghook_version.rc");
    if std::fs::write(&input, rc).is_err() {
        panic!("couldn't write dlghook_version.rc; refusing a version-metadata-free hook DLL");
    }
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("aarch64") {
        let res = format!("{out}/dlghook_version.res");
        if compile_with_windows_sdk_rc(&input, &res) {
            println!("cargo:rustc-link-arg={res}");
            return;
        }
        panic!(
            "ARM64 resource compilation requires Windows SDK rc.exe; refusing a \
             version-metadata-free hook DLL"
        );
    }
    let obj = format!("{out}/dlghook_version.o");
    // Every candidate's REASON for failing is kept, and the panic prints all of them.
    //
    // This used to collapse "not on PATH", "spawned but exited non-zero" and "spawned and the
    // OS refused" into one sentence, "windres unavailable ... Install binutils". That sentence
    // is a diagnosis, not an observation, and when it was WRONG it sent a session an hour down
    // the wrong road: windres was installed, on PATH, and ran fine by hand. A probe that names
    // one cause for every failure mode is worse than one that just says what happened.
    let mut why: Vec<String> = Vec::new();
    for windres in ["windres", "x86_64-w64-mingw32-windres"] {
        match std::process::Command::new(windres)
            .args(["-I", &out, &input, "-O", "coff", "-o", &obj])
            .status()
        {
            Ok(s) if s.success() => {
                // cdylib-only crate, so `-arg` reaches the DLL (no bins to confuse).
                println!("cargo:rustc-link-arg={obj}");
                return;
            }
            Ok(s) => why.push(format!("{windres}: ran but exited {s}")),
            Err(e) => why.push(format!("{windres}: could not run it ({e})")),
        }
    }
    // Unlike the shell extension, this one REFUSES to ship without a version: an unsigned
    // DLL that injects into other processes and carries no identity is the worst of both.
    panic!(
        "cannot compile the VERSIONINFO for st2k_dlghook.dll, and refusing to ship it without \
         one.\n  {}\nIf the reason above is \"could not run it\", windres is not being FOUND \
         (install binutils/llvm-windres, or put it on PATH). If it RAN and failed, read its \
         own error above this message.",
        why.join("\n  ")
    );
}

fn compile_with_windows_sdk_rc(input: &str, output: &str) -> bool {
    windows_sdk_rc_candidates().into_iter().any(|rc| {
        let status = std::process::Command::new(rc)
            .args(["/nologo", &format!("/fo{output}"), input])
            .status();
        matches!(status, Ok(s) if s.success())
    })
}

fn windows_sdk_rc_candidates() -> Vec<std::path::PathBuf> {
    let mut candidates = vec![std::path::PathBuf::from("rc.exe")];
    let Some(program_files_x86) = std::env::var_os("ProgramFiles(x86)") else {
        return candidates;
    };
    let sdk_bin = std::path::PathBuf::from(program_files_x86).join("Windows Kits/10/bin");
    let Ok(entries) = std::fs::read_dir(sdk_bin) else {
        return candidates;
    };
    let host = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let mut versions: Vec<std::path::PathBuf> = entries
        .flatten()
        .map(|entry| entry.path().join(host).join("rc.exe"))
        .filter(|path| path.is_file())
        .collect();
    versions.sort_by(|a, b| b.cmp(a));
    candidates.extend(versions);
    candidates
}

/// A Windows `VERSIONINFO` `.rc` pinned to `CARGO_PKG_VERSION` (the shared workspace
/// version). Mirrors `crates/dll/build.rs`.
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
