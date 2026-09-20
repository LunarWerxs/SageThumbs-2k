//! Which installer asset to take, and the proof it is ours: SHA-256, the detached signature, and the PE-stamped version binding.

use super::*;

/// The public half of the ed25519 key that signs every release artifact
/// (`examples/update-sign.rs` holds the private half; `examples/update-keygen.rs` mints the
/// pair). Every release asset's bytes must carry a valid detached signature against this key
/// before the installer is ever launched - see [`verify_signature`] and [`download_and_install`].
///
/// PLACEHOLDER: all zeros until the integrator runs `examples/update-keygen.rs` and pastes its
/// printed array literal here. An all-zero key is not a parse error for `ed25519-dalek` - it
/// decodes to a (weak, useless) point on the curve - so nothing verifies against it, and every
/// self-update correctly refuses rather than silently accepting an unsigned release. The
/// `the_compiled_in_key_is_not_the_placeholder` test below fails until this is a real key; that
/// is the point of the test, not a bug in it.
pub const UPDATE_PUBLIC_KEY: [u8; 32] = [
    0x16, 0x9f, 0xce, 0x0a, 0xde, 0x4a, 0xec, 0xed, 0x2d, 0xcb, 0x36, 0xa3, 0x76, 0xc1, 0x27, 0x74,
    0x38, 0x44, 0x77, 0x91, 0x84, 0xf5, 0x10, 0x9b, 0xc3, 0x8c, 0x00, 0x60, 0x3f, 0xa8, 0x32, 0x5b,
];

/// One published installer asset: where to fetch it, its exact byte size, and (when GitHub
/// supplies it) the sha256 digest we verify the bytes against before running it elevated.
pub(super) struct InstallerAsset {
    pub(super) url: String,
    pub(super) size: u64,
    pub(super) sha256: String, // lowercase hex, no "sha256:" prefix
    /// The `browser_download_url` of the sibling `<installer-name>.sig` asset, when the
    /// release published one. `None` means the release is unsigned - [`download_and_install`]
    /// refuses to launch in that case rather than skipping the check.
    pub(super) sig_url: Option<String>,
}

/// Pull the Windows installer asset out of GitHub's latest-release JSON — the exact versioned
/// setup executable — returning its tag + download URL + size + sha256, or None on
/// any failure (offline, no release, no matching asset).
pub(super) fn latest_installer_asset() -> Option<(String, InstallerAsset)> {
    let bytes = http_fetch(RELEASES_API, true)?;
    installer_asset_from_json(&serde_json::from_slice(&bytes).ok()?)
}

/// Pure parse of GitHub's latest-release JSON → (tag, installer asset). Split from the fetch
/// so it can be unit-tested against a real release body with no network.
pub(super) fn installer_asset_from_json(
    json: &serde_json::Value,
) -> Option<(String, InstallerAsset)> {
    installer_asset_from_json_for_arch(json, native_installer_arch())
}

/// Choose the installer for the native Windows architecture, not merely this process.
/// That distinction matters on ARM64: an older x64 SageThumbs build can run under
/// emulation, but native Explorer needs the ARM64 shell extension after the update.
pub(super) fn native_installer_arch() -> &'static str {
    let mut info = SYSTEM_INFO::default();
    unsafe {
        GetNativeSystemInfo(&mut info);
        installer_arch_for_native(
            info.Anonymous.Anonymous.wProcessorArchitecture,
            std::env::consts::ARCH,
        )
    }
}

/// Pure half of [`native_installer_arch`] so the x64-on-ARM64 migration rule is
/// covered on any CI host.
pub(super) fn installer_arch_for_native(
    native_arch: PROCESSOR_ARCHITECTURE,
    process_arch: &'static str,
) -> &'static str {
    match native_arch {
        PROCESSOR_ARCHITECTURE_ARM64 => "aarch64",
        PROCESSOR_ARCHITECTURE_AMD64 => "x86_64",
        _ => process_arch,
    }
}

/// Architecture-aware half of [`installer_asset_from_json`]. Keeping the target explicit
/// makes the release-asset contract testable on either development architecture: x64 gets
/// the established setup name, while ARM64 must never download that x64 installer.
pub(super) fn installer_asset_from_json_for_arch(
    json: &serde_json::Value,
    arch: &str,
) -> Option<(String, InstallerAsset)> {
    let raw_tag = json.get("tag_name")?.as_str()?;
    let (major, minor, patch) = parse_ver(raw_tag)?;
    let tag = format!("{major}.{minor}.{patch}");
    let expected_name = match arch {
        "x86_64" => format!("SageThumbs2K-Setup-{tag}.exe"),
        "aarch64" => format!("SageThumbs2K-Setup-{tag}-arm64.exe"),
        _ => return None, // no published self-update installer for this architecture
    };
    let asset = json.get("assets")?.as_array()?.iter().find(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n.eq_ignore_ascii_case(&expected_name))
    })?;
    let url = asset.get("browser_download_url")?.as_str()?.to_string();
    let (host, path) = crate::http::split_https(&url)?;
    if host != "github.com" || !path.starts_with("/LunarWerxs/SageThumbs-2k/releases/download/") {
        return None;
    }
    let size = asset
        .get("size")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let sha256 = asset
        .get("digest")
        .and_then(|d| d.as_str())
        .and_then(|d| d.strip_prefix("sha256:"))
        .map(str::to_ascii_lowercase)
        .filter(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()))?;

    // The detached signature ships as a SEPARATE release asset named "<installer-name>.sig"
    // beside the installer, not a field on the installer's own JSON - `release.ps1` uploads it
    // that way so the existing digest-verification loop (step 5) covers it for free. Its
    // absence is not a lookup failure: an unsigned release is a real (refused) state, not a
    // malformed one, so this stays `Option` rather than folding into the `?` chain above.
    let sig_name = format!("{expected_name}.sig");
    let sig_url = json.get("assets")?.as_array()?.iter().find_map(|a| {
        a.get("name")
            .and_then(|n| n.as_str())
            .filter(|n| n.eq_ignore_ascii_case(&sig_name))
            .and_then(|_| a.get("browser_download_url"))
            .and_then(|u| u.as_str())
            .map(str::to_string)
    });

    Some((
        tag,
        InstallerAsset {
            url,
            size,
            sha256,
            sig_url,
        },
    ))
}

/// SHA-256 of `data` as lowercase hex, via Windows CNG (no extra crate). None on failure.
pub(super) fn sha256_hex(data: &[u8]) -> Option<String> {
    let digest = crate::license::sha256(data)?;
    Some(digest.iter().map(|b| format!("{b:02x}")).collect())
}

/// Parse 128 lowercase-or-uppercase hex characters into a raw 64-byte ed25519 signature.
/// `None` on anything else - wrong length, non-ASCII, non-hex - worked byte-wise so a
/// downloaded `.sig` file can never panic this on a bad char boundary.
pub(super) fn parse_sig_hex(sig_hex: &str) -> Option<[u8; 64]> {
    let bytes = sig_hex.as_bytes();
    if bytes.len() != 128 || !bytes.is_ascii() {
        return None;
    }
    let mut out = [0u8; 64];
    for i in 0..64 {
        let hi = (bytes[i * 2] as char).to_digit(16)?;
        let lo = (bytes[i * 2 + 1] as char).to_digit(16)?;
        out[i] = ((hi << 4) | lo) as u8;
    }
    Some(out)
}

/// Verify a detached ed25519 signature over `bytes`. `sig_hex` is the `.sig` asset's raw
/// content - 128 hex characters, no framing. Any parse failure (bad length, non-hex, a `key`
/// that doesn't decode to a curve point) is simply `false`, same as a bad signature: there is
/// no distinguishable "malformed" outcome for the caller to accidentally treat as anything
/// other than "not verified".
pub(super) fn verify_signature(key: &[u8; 32], bytes: &[u8], sig_hex: &str) -> bool {
    let Some(sig_bytes) = parse_sig_hex(sig_hex) else {
        return false;
    };
    let Ok(verifying_key) = VerifyingKey::from_bytes(key) else {
        return false;
    };
    verifying_key
        .verify(bytes, &Signature::from_bytes(&sig_bytes))
        .is_ok()
}

/// The file version stamped into a PE's `VS_VERSIONINFO` resource, as (major, minor, patch),
/// read with the Windows version API. `None` when the file carries no version resource or
/// the API refuses it. Reads the file on disk: the caller passes the locked, re-verified
/// installer, so the bytes read here are the signed bytes.
pub(super) fn pe_stamped_version(path: &Path) -> Option<(u32, u32, u32)> {
    use windows::core::{w, HSTRING};
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW, VS_FIXEDFILEINFO,
    };
    let wide = HSTRING::from(path.as_os_str());
    // SAFETY: plain wide-string in, a buffer we size from the API's own answer, and a pointer
    // into that buffer that VerQueryValueW promises stays valid while the buffer does.
    unsafe {
        let mut handle = 0u32;
        let size = GetFileVersionInfoSizeW(&wide, Some(&mut handle));
        if size == 0 {
            return None;
        }
        let mut buf = vec![0u8; size as usize];
        GetFileVersionInfoW(&wide, Some(0), size, buf.as_mut_ptr().cast()).ok()?;
        let mut info: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        if !VerQueryValueW(buf.as_ptr().cast(), w!("\\"), &mut info, &mut len).as_bool()
            || info.is_null()
            || (len as usize) < std::mem::size_of::<VS_FIXEDFILEINFO>()
        {
            return None;
        }
        let fixed = std::ptr::read_unaligned(info as *const VS_FIXEDFILEINFO);
        if fixed.dwSignature != 0xFEEF_04BD {
            return None;
        }
        Some((
            fixed.dwFileVersionMS >> 16,
            fixed.dwFileVersionMS & 0xFFFF,
            fixed.dwFileVersionLS >> 16,
        ))
    }
}

/// Bind the signed installer to the update it claims to be: the version stamped inside the
/// file must equal `advertised` (the feed's tag) and be newer than `running`. `Err` carries
/// the user-facing refusal. Pure in `stamped`; [`pe_stamped_version`] supplies it.
pub(super) fn version_binding(
    stamped: Option<(u32, u32, u32)>,
    advertised: &str,
    running: &str,
) -> Result<(), String> {
    let Some(stamped) = stamped else {
        return Err("The downloaded update carries no version stamp, so it was not run.".into());
    };
    let Some(advertised) = parse_ver(advertised) else {
        return Err("The update's advertised version could not be read, so it was not run.".into());
    };
    let fmt = |(a, b, c): (u32, u32, u32)| format!("{a}.{b}.{c}");
    if stamped != advertised {
        return Err(format!(
            "The downloaded update is version {} but was offered as {}, so it was not run.",
            fmt(stamped),
            fmt(advertised)
        ));
    }
    if let Some(running) = parse_ver(running) {
        if stamped <= running {
            return Err(format!(
                "The downloaded update is version {}, not newer than the installed {}, so it \
                 was not run.",
                fmt(stamped),
                fmt(running)
            ));
        }
    }
    Ok(())
}

/// [`version_binding`] for the installer saved at `path`.
pub(super) fn stamped_version_is_the_advertised_upgrade(
    path: &Path,
    advertised: &str,
    running: &str,
) -> Result<(), String> {
    version_binding(pe_stamped_version(path), advertised, running)
}

/// Validate downloaded installer bytes before we ever run them elevated: a real PE, the
/// exact advertised size, and (when GitHub supplied a digest) a matching sha256. False =
/// refuse — we'd rather fall back to the manual page than run an unverified installer. We
/// write the bytes ourselves (no Mark-of-the-Web), so the silent launch won't trip SmartScreen.
pub(super) fn verify_installer_bytes(bytes: &[u8], asset: &InstallerAsset) -> bool {
    if bytes.len() < 2 || &bytes[..2] != b"MZ" {
        return false; // not a Windows executable
    }
    if asset.size != 0 && bytes.len() as u64 != asset.size {
        return false; // truncated / wrong length
    }
    if sha256_hex(bytes).as_deref() != Some(asset.sha256.as_str()) {
        return false; // integrity check failed
    }
    true
}

/// Atomically create the downloaded installer, then hold a READ-ONLY handle that permits
/// readers but denies other writers and deleters. Holding that handle through
/// `ShellExecuteW("runas")` closes the pathname replacement window between the final hash
/// check and the elevated process opening the image — and the final verification is read
/// back THROUGH that handle, so the bytes we bless are the bytes it is protecting.
///
/// THE LOCK MUST NOT CARRY WRITE ACCESS. Windows maps an executable image by opening the
/// file with `FILE_SHARE_READ | FILE_SHARE_DELETE`, and that share mode cannot coexist with
/// an existing writer — so a read+WRITE lock makes the launch itself fail with
/// `ERROR_SHARING_VIOLATION`, which `ShellExecuteW` reports as `SE_ERR_SHARE` (26). That is
/// precisely what shipped in 1.3.3: one-click self-update failed on EVERY machine, every
/// time, and the failure text blamed the user for not being an administrator. A read-only
/// lock denies writers and deleters exactly as well (both tested below) while leaving the
/// image mappable.
pub(super) fn write_locked_installer(
    tag: &str,
    bytes: &[u8],
    asset: &InstallerAsset,
) -> Result<(PathBuf, std::fs::File), &'static str> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    for attempt in 0..16u8 {
        let path = std::env::temp_dir().join(format!(
            "SageThumbs2K-Setup-{tag}-{}-{nonce}-{attempt}.exe",
            std::process::id()
        ));
        // Share NOTHING while the bytes are going down: nobody may even read a half-written
        // setup, let alone race the write.
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .share_mode(0)
            .open(&path);
        let mut file = match opened {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err("couldn't save the installer"),
        };
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file); // the write handle is gone before anything tries to run the image
        if written.is_err() {
            let _ = std::fs::remove_file(&path);
            return Err("couldn't save the installer");
        }
        // Re-open read-only and verify through THIS handle — the one held across the launch.
        let Ok(mut file) = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ.0)
            .open(&path)
        else {
            let _ = std::fs::remove_file(&path);
            return Err("couldn't lock the saved installer");
        };
        let mut on_disk = Vec::with_capacity(bytes.len());
        if file.read_to_end(&mut on_disk).is_err() || !verify_installer_bytes(&on_disk, asset) {
            drop(file);
            let _ = std::fs::remove_file(&path);
            return Err("the saved installer failed re-verification");
        }
        return Ok((path, file));
    }
    Err("couldn't reserve a temporary installer path")
}
