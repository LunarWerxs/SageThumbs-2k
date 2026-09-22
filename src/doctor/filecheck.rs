//! The per-file probe for `st2k doctor <path>`: cloud-placeholder and sync-root notes,
//! Explorer's own log/shell round trip, the video-codec and volume checks, and the
//! `probe_file` entry point that ties them together end to end for one file.

use super::*;

/// Probe ONE specific file end-to-end: is its extension one we hook, is that format
/// enabled, and — the part the global checks can't tell you — does THIS file actually
/// DECODE? The global report proves registration is healthy; it stays silent on "we're
/// registered fine but can't render the one file you care about", which is exactly the
/// shape of the modern-`.xcf` reports (GIMP 2.10+/3.0 writes an XCF version the bundled
/// ImageMagick's coder can't read). Read-only: opens + decodes the file, writes nothing.
/// OneDrive (and any Files-On-Demand provider) leaves a *placeholder* on disk: the metadata is
/// local, the bytes are not, and the first read pulls the whole file down over the network.
///
/// That matters here because of HOW MUCH we have to read. Formats with a baked-in preview are
/// cheap even on a placeholder: `stream_source` reads a bounded prefix or seeks straight to a
/// cover, so only a slice is ever recalled. The formats with NO such shortcut fall through to
/// the whole-file read, and on a cloud-only file that means downloading it in full inside
/// Explorer's thumbnail host, which is slow enough to be indistinguishable from broken and
/// leaves a cached failure behind. `.xcf` is the sharp edge (a report, 2026-08-05): GIMP writes
/// no embedded thumbnail, so there is nothing to read but the entire image.
///
/// Reported, never "fixed" silently: refusing to hydrate would take thumbnails away from people
/// whose files ARE downloaded and working today. `std::fs::metadata` reads attributes without
/// triggering recall, so this check itself never pulls anything down.
fn cloud_placeholder_note(r: &mut Report, p: &Path, ext: &str) {
    use std::os::windows::fs::MetadataExt;

    let Ok(meta) = std::fs::metadata(p) else {
        return;
    };
    let attrs = meta.file_attributes();
    // Same OFFLINE / RECALL_ON_OPEN / RECALL_ON_DATA_ACCESS trio `prebuild.rs` skips on —
    // one definition, so this diagnostic and that guard can't drift apart again (see
    // `crate::prebuild::OFFLINE_ATTRS`'s own doc for how they already had once).
    if attrs & crate::prebuild::OFFLINE_ATTRS == 0 {
        return; // fully local: nothing to say
    }

    // Deliberately NOT sniffing the header to say whether this format could be served from a
    // prefix: reading even the first bytes of a placeholder is what triggers the recall this
    // check exists to warn about. The advice is the same either way.
    let len = meta.len();
    let size = if len >= 1024 * 1024 {
        format!("{} MB", len / (1024 * 1024))
    } else {
        format!("{} KB", len.div_ceil(1024))
    };
    // Says what is true in general without claiming anything about THIS file's internals,
    // which would need the header read this check exists to avoid.
    r.fail_with_fix(
        "Cloud file (OneDrive)",
        &format!(
            "the bytes are not on this PC yet ({size}). Fetching them happens inside Explorer's \
             thumbnail host, slow enough to look like nothing is happening, and a format with \
             no embedded preview (.xcf is one) needs the WHOLE file, not a slice — this one \
             is .{ext}"
        ),
        "Right-click the file or its folder -> 'Always keep on this device'. Once the bytes are \
         local the thumbnail appears normally. If it stays blank after downloading, Explorer \
         cached the earlier failure: clear thumbcache_*.db (see the IconsOnly fix above).",
    );
}

/// Ask the SHELL for this file's thumbnail, the same way Explorer does, and report what
/// comes back.
///
/// This is the check every other check in this report is a proxy for. Everything above
/// proves *our half* works: registration is healthy, the DLL loads, the decoder renders
/// these bytes. None of it can see the one thing that actually decides what you look at —
/// whether Explorer, on this path, in this folder, calls us at all and keeps the result.
///
/// It matters because those two answers really do come apart. Issue #16 is the shape:
/// registration perfect, decode of the exact file perfect, thumbnail still missing — for a
/// file inside a OneDrive sync root, where the shell can route thumbnails through the sync
/// provider instead of the per-extension handler. Without this line the report says "no
/// blocking problem found" and the user is told, wrongly, that it must be their cache.
///
/// `SIIGBF_THUMBNAILONLY` is what makes the answer meaningful: it tells the shell to FAIL
/// rather than quietly substitute the file's icon, so "no thumbnail" is reported as no
/// thumbnail instead of arriving as a 256px picture of a document.
///
/// Read-only in the sense that matters (it writes nothing of the user's), with one honest
/// caveat: extracting a thumbnail lets Windows populate its own thumbnail cache for this
/// item — exactly what browsing to the folder would have done.
fn shell_roundtrip(r: &mut Report, path: &str) {
    use windows::core::HSTRING;
    use windows::Win32::Foundation::SIZE;
    use windows::Win32::Graphics::Gdi::{DeleteObject, GetObjectW, BITMAP};
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{
        IShellItemImageFactory, SHCreateItemFromParsingName, SIIGBF_THUMBNAILONLY,
    };

    // The shell wants an absolute path — handed a relative one (`st2k doctor file.mkv` from
    // the file's own folder), SHCreateItemFromParsingName fails with FILE_NOT_FOUND and this
    // check would report a spurious "shell returned NO thumbnail". `prebuild::parsing_path`
    // canonicalizes and undoes the extended-length prefix (its own doc has the UNC details);
    // shared with `prebuild.rs`'s `one()`, which needs the identical normalization for its
    // own `SHCreateItemFromParsingName` call — this used to be a hand-copied duplicate.
    let abs = crate::prebuild::parsing_path(path);
    let path = abs.as_str();

    // The shell objects need an apartment. Uninitialise only if WE initialised, so this
    // never tears down an apartment a caller (the MCP server, a future GUI host) owns.
    let inited = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
    let result: Result<(i32, i32), windows::core::Error> = (|| unsafe {
        let item: IShellItemImageFactory = SHCreateItemFromParsingName(&HSTRING::from(path), None)?;
        let hbmp = item.GetImage(SIZE { cx: 256, cy: 256 }, SIIGBF_THUMBNAILONLY)?;
        let mut bm = BITMAP::default();
        let got = GetObjectW(
            hbmp.into(),
            core::mem::size_of::<BITMAP>() as i32,
            Some(core::ptr::addr_of_mut!(bm).cast()),
        );
        let _ = DeleteObject(hbmp.into());
        if got == 0 {
            return Err(windows::core::Error::from_thread());
        }
        Ok((bm.bmWidth, bm.bmHeight.abs()))
    })();
    if inited {
        unsafe { CoUninitialize() };
    }

    match result {
        Ok((w, h)) => r.line(
            S::Ok,
            "Explorer's own thumbnail",
            &format!("the shell returned a {w}x{h} thumbnail for this path"),
        ),
        Err(e) => r.fail_with_fix(
            "Explorer's own thumbnail",
            &format!(
                "the shell returned NO thumbnail for this path ({:#010x}) — even though our \
                 decoder can render this file",
                e.code().0
            ),
            "our half is working, so something between us and Explorer is dropping it. In \
             order: rebuild the thumbnail cache (Settings > Advanced), then check the note \
             about this file's folder below — a cloud-synced folder can serve thumbnails \
             from the sync provider instead of from us. Copying the file to a plain local \
             folder and re-running this command tells the two apart in one step.",
        ),
    }
}

/// Is this file inside a cloud sync root (OneDrive and friends), and does that provider
/// register its own thumbnail source?
///
/// A sync engine built on the Cloud Files API may declare a `ThumbnailProvider` under its
/// `SyncRootManager` entry, which applies to EVERYTHING under that root rather than to one
/// file type — so it can pre-empt a per-extension handler like ours for every file in the
/// folder. That is the leading explanation for "works in a normal folder, generic icon in
/// OneDrive", and it is invisible from the file itself, so name it here rather than leaving
/// the user to guess. Purely a registry read; nothing is hydrated and nothing is written.
/// Whether lowercase `file` is `root` or inside it. A bare prefix match is not enough:
/// `c:\users\me\onedrive-old\x.jpg` starts with the sync root `c:\users\me\onedrive` but is not
/// inside it, so the character after the root must be a separator.
fn is_under_root(file: &str, root: &str) -> bool {
    let base = root.trim_end_matches('\\');
    !base.is_empty()
        && file.starts_with(base)
        && (file.len() == base.len() || file.as_bytes().get(base.len()) == Some(&b'\\'))
}

fn cloud_sync_root_note(r: &mut Report, p: &Path) {
    const SYNC_ROOTS: &str = r"SOFTWARE\Microsoft\Windows\CurrentVersion\Explorer\SyncRootManager";
    let Ok(file) = p.canonicalize() else {
        return;
    };
    let file = file
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_lowercase();

    let Ok(roots) = LOCAL_MACHINE.open(SYNC_ROOTS) else {
        return;
    };
    let Ok(names) = roots.keys() else {
        return;
    };
    for name in names {
        let Ok(root) = roots.open(&name) else {
            continue;
        };
        // Each provider lists its on-disk roots under UserSyncRoots\<user SID>.
        let Ok(user_roots) = root.open("UserSyncRoots") else {
            continue;
        };
        let Ok(values) = user_roots.values() else {
            continue;
        };
        let hit = values.into_iter().any(|(_, v)| {
            is_under_root(
                &file,
                &String::try_from(v).unwrap_or_default().to_lowercase(),
            )
        });
        if !hit {
            continue;
        }
        let has_provider =
            root.open("ThumbnailProvider").is_ok() || root.get_string("ThumbnailProvider").is_ok();
        // The provider id is `<Provider>!<SID>!<account>`; the first field is the readable bit.
        let provider = name.split('!').next().unwrap_or(&name).to_string();
        if has_provider {
            r.fail_with_fix(
                "Cloud-synced folder",
                &format!(
                    "this file is inside a {provider} sync root, and {provider} registers its \
                     OWN thumbnail source for everything under it — which can take precedence \
                     over ours for every file in the folder"
                ),
                "not something we can override from here. To confirm it is the cause, copy \
                 the file to a folder outside the sync root and look again: if the thumbnail \
                 appears there, this is why it does not appear here.",
            );
        } else {
            r.line(
                S::Warn,
                "Cloud-synced folder",
                &format!(
                    "this file is inside a {provider} sync root. {provider} does not register \
                     its own thumbnail source, so ours should be used — but a sync root is \
                     still the first thing to rule out by copying the file elsewhere"
                ),
            );
        }
        return;
    }
}

/// Whether Explorer ever reached our provider for THIS file, read off the diagnostics log.
/// The always-on failure line and the verbose per-call line both carry `ext=` and `size=`
/// (`thumbprovider::stream_identity`), and the extension plus the exact byte count is as
/// good a key as a path. Issue #37's third round was decided by reading that tail by hand;
/// a user cannot, so the report does it.
fn explorer_asked_us_note(r: &mut Report, p: &Path, ext: &str) {
    const LABEL: &str = "Explorer asked us?";
    let Ok(meta) = p.metadata() else {
        return;
    };
    let key = format!("ext={ext} size={}", meta.len());
    let Some(log) = crate::safety::log_file().filter(|l| l.exists()) else {
        r.line(S::Info, LABEL, "no diagnostics log on this machine yet");
        return;
    };
    let lines = tail_matching_lines(&log, LOG_TAIL_SCAN_BYTES, &[&key], 200);
    let failed: Vec<&String> = lines
        .iter()
        .filter(|l| l.contains("GetThumbnail: failed"))
        .collect();
    let asked = lines
        .iter()
        .filter(|l| l.contains("GetThumbnail: cx="))
        .count();
    if let Some(last) = failed.last() {
        let hr = last
            .split("hr=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .unwrap_or("?");
        r.line(
            S::Warn,
            LABEL,
            &format!(
                "yes — our provider failed {} time(s) for a file of this size (last hr={hr}); \
                 the log tail below has the lines",
                failed.len()
            ),
        );
    } else if asked > 0 {
        r.line(
            S::Ok,
            LABEL,
            &format!(
                "yes — {asked} call(s) for a file of this size in the verbose log, none failed"
            ),
        );
    } else {
        r.line(
            S::Info,
            LABEL,
            "no record for a file of this size in the recent log: either Explorer never asked \
             us (a namespace entry, a sync root or another handler answered first), or we \
             succeeded with Verbose logging off. Turn on Verbose logging (Settings > Advanced), \
             browse the folder, and run the doctor again to tell which",
        );
    }
}

/// For a video file: name the codec inside it and say whether THIS Windows can decode it.
///
/// Frames come from the OS Media Foundation codecs, and Windows does not ship them all —
/// HEVC and AV1 are Microsoft Store add-ons, not inbox — so "registration healthy, file
/// healthy, still no thumbnail" is routinely a codec gap rather than a bug in anything.
/// Without this line that failure is invisible: the decode check below just says FAILED,
/// and the old hint blamed ImageMagick, which never touches video. (Born of an uninstall
/// feedback that said, in full, "mkv thumbnail not showing" — this is the report that
/// would have answered it.)
fn video_codec_note(r: &mut Report, path: &str) {
    // Without Media Foundation there are no video thumbnails at all, whatever the codec.
    // check_engine already prints the global warning; this is the per-file FAIL with a fix.
    if !crate::video::media_foundation_available() {
        r.fail_with_fix(
            "Media Foundation",
            "NOT present on this Windows (\"N\"/\"KN\" editions omit it) — video thumbnails \
             decode through its codecs",
            "install the 'Media Feature Pack' (Settings > Apps > Optional features), then \
             sign out and back in",
        );
        return;
    }
    let Ok(file) = std::fs::File::open(path) else {
        return; // the Read-file check below reports this with its own message
    };
    let mut file = std::io::BufReader::new(file);
    let Some(info) = crate::vcodec::identify(&mut file) else {
        r.line(
            S::Info,
            "Video codec",
            "not identifiable from the container header (only Matroska/WebM, MP4/MOV, FLV \
             and MPEG program/elementary streams carry one we parse) — the decode check \
             below is the real test",
        );
        return;
    };
    let label = format!("{} ({})", info.name, info.raw);
    // A codec Windows DOES decode, in a profile its decoder does NOT implement (issue #35:
    // H.264 4:4:4 / 4:2:2 / 10-bit). The cascade refuses these before Media Foundation is
    // asked, on purpose - on Windows 10 the decoder wedged rather than declined - so the
    // report has to name that refusal, or "a decoder is installed" below reads as a bug in us.
    if let Some(block) = &info.mf_profile_block {
        r.fail_with_fix(
            "Video codec",
            &format!(
                "{label} - {block}; no frame can be decoded, so the thumbnail is skipped on \
                 purpose rather than risk hanging Explorer's thumbnail host"
            ),
            "no decoder can be installed for this profile; re-encode as 8-bit 4:2:0 H.264 \
             (ffmpeg: -c:v libx264 -pix_fmt yuv420p), or attach cover art, which we show \
             when no frame can be decoded",
        );
        return;
    }
    // Codecs we decode OURSELVES (FLV's VP6 / Sorenson Spark, MPEG-1/2 in a program or
    // elementary stream; out of process via st2k): whether Windows has a decoder (the Store
    // MPEG-2 extension) or never will (VP6), none is needed — say so BEFORE the MF probe,
    // whose honest answer ("no decoder installed") would come with a fix prescription that
    // does not apply.
    if info.self_decoded {
        r.line(
            S::Ok,
            "Video codec",
            &format!(
                "{label} — decoded by SageThumbs 2K's own built-in decoder (no Windows \
                 decoder is needed for this file)"
            ),
        );
        return;
    }
    match info.subtype.map(crate::vcodec::decoder_installed) {
        Some(Some(true)) => r.line(
            S::Ok,
            "Video codec",
            &format!("{label} — a Windows decoder is installed"),
        ),
        Some(Some(false)) => r.fail_with_fix(
            "Video codec",
            &format!(
                "{label} — NO Windows decoder for this codec is installed, so no frame \
                 can be decoded (this is the usual cause of a missing video thumbnail)"
            ),
            // Careful wording: this branch fires with MF PRESENT but an inbox decoder
            // missing — a Server / stripped-down edition, where "install the Media
            // Feature Pack" is a setting that does not exist. Name both possibilities
            // instead of sending the user hunting for a control their edition lacks.
            info.install_hint.unwrap_or(
                "this decoder normally ships with consumer Windows; a Server or \
                 stripped-down edition may simply not include it (on an \"N\"/\"KN\" \
                 edition, the Media Feature Pack under Settings > Apps > Optional \
                 features restores it)",
            ),
        ),
        // MF vanished between the gate above and the probe — report it, don't guess.
        Some(None) => r.line(
            S::Warn,
            "Video codec",
            &format!("{label} — could not query Media Foundation for a decoder"),
        ),
        None if info.known => r.fail_with_fix(
            "Video codec",
            &format!("{label} — Windows has no decoder for this codec"),
            "none exists to install; re-encode the file (H.264 plays everywhere), or rely \
             on attached cover art, which we show when no frame can be decoded",
        ),
        None => r.line(
            S::Warn,
            "Video codec",
            &format!("{label} — an id we don't recognize, so we can't check for a decoder"),
        ),
    }
    // An embedded poster (a Matroska attachment or an MP4 `covr` item) means a thumbnail
    // exists even with no codec at all, which is the whole answer for an HEVC library on a
    // machine without the Store extension. Say so, and say which rule is currently in force.
    if crate::vcodec::cover_art(&mut file).is_some() {
        let detail = if crate::settings::prefer_cover_art() {
            "present, and Settings prefers it, so this is the thumbnail you get"
        } else {
            "present - used when no frame can be decoded. Settings > General > 'Use a \
             video's cover art instead of a frame' makes it the first choice"
        };
        r.line(S::Ok, "Embedded cover art", detail);
    }
}

/// What the file's DRIVE is: fixed, removable, remote, optical or RAM disk; its file system;
/// and, for a drive letter, the device behind it — which is how a `subst` drive, a mapped
/// virtual disk or a per-session mount shows itself. Explorer extracts thumbnails in its own
/// helper process while this report's own probe loads the decoder in-process, so a drive that
/// exists in one context and not the other (a substituted letter is per logon session) is
/// the one case where "the doctor works, Explorer does not" is about the drive and not the
/// file (issue #37: every failing path was on one drive, every passing one on another).
fn volume_note(r: &mut Report, path: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetVolumeInformationW, QueryDosDeviceW};
    let bytes = path.as_bytes();
    if bytes.len() < 3 || !bytes[0].is_ascii_alphabetic() || bytes[1] != b':' {
        return; // UNC and relative paths: nothing per-drive to say
    }
    let letter = (bytes[0] as char).to_ascii_uppercase();
    let root: Vec<u16> = format!("{letter}:\\")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let kind = drive_kind(&root);
    let mut fs_name = [0u16; 64];
    let fs = unsafe {
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            None,
            None,
            None,
            None,
            Some(&mut fs_name),
        )
    }
    .ok()
    .map(|_| {
        let end = fs_name
            .iter()
            .position(|&c| c == 0)
            .unwrap_or(fs_name.len());
        String::from_utf16_lossy(&fs_name[..end])
    })
    .filter(|s| !s.is_empty())
    .unwrap_or_else(|| "file system unreadable".to_string());
    // The device behind the letter. A subst/virtual mapping answers `\??\<path>`; a real
    // volume answers `\Device\HarddiskVolumeN`; a mapped share `\Device\LanmanRedirector...`.
    let dev: Vec<u16> = format!("{letter}:").encode_utf16().chain(Some(0)).collect();
    let mut target = [0u16; 512];
    let n = unsafe { QueryDosDeviceW(PCWSTR(dev.as_ptr()), Some(&mut target)) } as usize;
    let device = if n > 0 {
        let end = target[..n].iter().position(|&c| c == 0).unwrap_or(n);
        String::from_utf16_lossy(&target[..end])
    } else {
        String::new()
    };
    let (status, detail) = if let Some(mapped) = device.strip_prefix("\\??\\") {
        (
            S::Warn,
            format!(
                "{letter}: is a substituted drive ({kind}, {fs}) mapped to {mapped} — a \
                 subst letter exists only in the logon session that made it, so Explorer's \
                 thumbnail helper may not see this path at all; use the real path instead"
            ),
        )
    } else {
        (
            S::Info,
            format!(
                "{letter}: is a {kind} drive, {fs}{}",
                if device.is_empty() {
                    String::new()
                } else {
                    format!(" ({device})")
                }
            ),
        )
    };
    r.line(status, "Volume", &detail);
}

/// Maps the raw GetDriveTypeW code for the drive `root` names to a human-readable kind.
fn drive_kind(root: &[u16]) -> &'static str {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::GetDriveTypeW;
    use windows::Win32::System::WindowsProgramming::{
        DRIVE_CDROM, DRIVE_FIXED, DRIVE_NO_ROOT_DIR, DRIVE_RAMDISK, DRIVE_REMOTE, DRIVE_REMOVABLE,
        DRIVE_UNKNOWN,
    };
    match unsafe { GetDriveTypeW(PCWSTR(root.as_ptr())) } {
        DRIVE_FIXED => "fixed",
        DRIVE_REMOVABLE => "removable",
        DRIVE_REMOTE => "network",
        DRIVE_CDROM => "optical",
        DRIVE_RAMDISK => "RAM disk",
        DRIVE_NO_ROOT_DIR => "no such drive",
        DRIVE_UNKNOWN => "unknown type",
        _ => "unknown type",
    }
}

pub(super) fn probe_file(
    r: &mut Report,
    path: &str,
    snap: &crate::settings::FormatEnabledSnapshot,
) {
    r.head("This file");
    let p = Path::new(path);
    r.line(S::Info, "Path", path);
    // Said HERE and not only in the policy section, because this is the report a confused
    // user actually reads, and the two facts only mean something together: the file is on a
    // network drive AND this machine tells Explorer not to thumbnail those. Everything else
    // below will pass, including the shell's own thumbnail call, which does not honour the
    // policy (issue #36).
    if is_network_path(path) {
        if network_thumbnails_disabled() {
            r.fail_with_fix(
                "Network location",
                "this file is on a network drive, and a policy on this machine turns thumbnails \
                 off for network folders — that is why it shows an icon while local files do not",
                "clear DisableThumbnailsOnNetworkFolders (see the Thumbnail policies section \
                 above), sign out and back in, then rebuild the thumbnail cache",
            );
        } else {
            r.line(
                S::Info,
                "Network location",
                "this file is on a network drive — thumbnails there are allowed on this \
                 machine, but they are fetched over the network and can be slow to appear",
            );
        }
    }
    volume_note(r, path);
    if !p.is_file() {
        r.fail_with_fix(
            "File",
            "does not exist / not a file",
            "check the path (quote it if it has spaces)",
        );
        return;
    }

    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if ext.is_empty() {
        r.line(
            S::Warn,
            "Extension",
            "none — Explorer keys thumbnails off the extension",
        );
        return;
    }
    // Is this extension one SageThumbs hooks at all? If not, THAT is the whole answer —
    // Explorer never asks us, no matter how healthy registration is.
    if !crate::formats::is_known(&ext) {
        r.fail_with_fix(
            &format!(".{ext}"),
            "NOT a format SageThumbs handles — Explorer will never ask us for it",
            "this file type isn't supported; open an issue to request it",
        );
        return;
    }
    r.line(S::Ok, &format!(".{ext}"), "a supported format");
    cloud_placeholder_note(r, p, &ext);
    cloud_sync_root_note(r, p);
    this_pc_namespace_note(r, p);
    explorer_asked_us_note(r, p, &ext);
    if !snap.enabled(&ext) {
        r.fail_with_fix(
            "Enabled in settings",
            "this format is unchecked in Settings > File types",
            "tick it in Settings > File types (or 'Select all')",
        );
    }
    let is_video = matches!(
        crate::formats::category(&ext),
        crate::formats::Category::Video
    );
    if is_video {
        video_codec_note(r, path);
    }

    // The decisive step: actually run the thumbnail decoder on THIS file's bytes, the
    // same preview-fidelity path Explorer's provider uses.
    match crate::decode::read_preview_capped(path) {
        Err(e) => r.fail_with_fix(
            "Read file",
            &format!("could not read the bytes: {e}"),
            "check the file isn't locked, truncated, or over the size limit",
        ),
        Ok(bytes) => report_decode(r, path, &bytes, is_video),
    }
}

/// Decodes the file's bytes and reports the decode outcome: a thumbnail that can be
/// produced, a video with no frame, or a failure with the matching hint.
fn report_decode(r: &mut Report, path: &str, bytes: &[u8], is_video: bool) {
    match crate::decode::decode_preview(bytes) {
        Ok(img) => {
            r.line(
                S::Ok,
                "Decode this file",
                &format!(
                    "OK ({}x{}) — a thumbnail CAN be produced",
                    img.width(),
                    img.height()
                ),
            );
            // Our half is proven good, so now ask the shell the same question and see
            // whether the two answers agree. When they don't, that disagreement IS the
            // diagnosis, and it is the only line in this report that can produce it.
            shell_roundtrip(r, path);
            // Reaching here means the decoder is fine and the file is fine, yet the user
            // is running `doctor` on it — so what is left is almost always the shell, and
            // this is the one the report cannot see. Explorer remembers a view PER FOLDER,
            // and Details / List / Small icons never draw thumbnails at all, by design; a
            // folder Windows auto-classified as "Documents" opens in Details. Only said on
            // success, where it is the likely remaining answer rather than noise.
            r.line(
                S::Info,
                "  if it still looks wrong",
                "check this file's FOLDER view: Details, List and Small icons never show \
                 thumbnails. Set Medium icons or larger (View menu, or Ctrl+Shift+2..4).",
            );
            // The live request above is not read-only from Explorer's point of view: a
            // fresh answer replaces whatever the thumbnail cache remembered for this path
            // at that size, including a miss cached when the file was still being copied
            // in (issue #36: the same PSD drew a thumbnail on the Desktop and an icon in
            // an Explorer window, one size per view). Say so, or the user reads "it works
            // now" as proof nothing was wrong, and the other sizes and files still hold
            // their stale entries.
            r.line(
                S::Info,
                "  note",
                "this check also refreshed Explorer's cached thumbnail for this file at \
                 that one size. Other sizes and other files can still hold a stale \
                 'no thumbnail' entry (the sign: a thumbnail on the Desktop but an icon \
                 in an Explorer window of the same folder). Settings > Advanced > \
                 'Rebuild thumbnail cache' clears them all.",
            );
        }
        Err(_) if is_video => {
            // Video never touches ImageMagick — the frame comes from the OS Media
            // Foundation codecs, so point at the codec finding instead of the
            // (irrelevant, and previously misleading) ImageMagick hint.
            r.fail_with_fix(
                "Decode this file",
                "FAILED — no frame could be decoded from this video",
                "see the 'Video codec' line above: a missing OS decoder is the usual \
                 cause. If a decoder IS installed, an unusual profile (10-bit, Dolby \
                 Vision) or a truncated file are the next suspects",
            );
        }
        Err(_) => {
            // Registered + enabled, but the pixels won't come out. Point at the
            // likely reason: the long-tail formats decode only through the bundled
            // ImageMagick, whose coders lag newer file-format versions.
            let magick = crate::decode::magick_available();
            let hint = if magick {
                "ImageMagick is present but its coder could not decode this file \
                 (often a newer version of the format than the coder supports)"
            } else {
                "this format decodes only via ImageMagick, which is NOT installed here \
                 (use the full installer, or install ImageMagick)"
            };
            r.fail_with_fix(
                "Decode this file",
                "FAILED — no thumbnail possible for this file",
                hint,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file whose bytes are not local (OneDrive placeholder) must be called out, and a
    /// normal local file must NOT be — a false "your file is in the cloud" on every ordinary
    /// probe would be worse than saying nothing.
    #[test]
    fn cloud_placeholder_is_reported_only_when_offline() {
        let dir = std::env::temp_dir().join(format!("st2k-doctor-cloud-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("probe.xcf");
        std::fs::write(&file, b"gimp xcf file\0").unwrap();

        let mut local = Report::new();
        cloud_placeholder_note(&mut local, &file, "xcf");
        assert!(
            local.out.is_empty(),
            "a fully local file must produce no cloud note, got: {}",
            local.out
        );

        // FILE_ATTRIBUTE_OFFLINE is exactly what a Files-On-Demand placeholder carries, and
        // it is settable here, so this exercises the real attribute check rather than a mock.
        set_offline(&file);
        let mut cloud = Report::new();
        cloud_placeholder_note(&mut cloud, &file, "xcf");
        assert!(
            cloud.out.contains("not on this PC yet"),
            "offline file should be reported: {}",
            cloud.out
        );
        // `fail_with_fix` prints the symptom inline and files the fix under Verdict, so the
        // fix lives in `problems` rather than in `out`.
        assert!(
            cloud
                .problems
                .iter()
                .any(|p| p.contains("Always keep on this device")),
            "the report must carry the actual fix: {:?}",
            cloud.problems
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Set FILE_ATTRIBUTE_OFFLINE, preserving whatever else is set.
    fn set_offline(path: &Path) {
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::fs::MetadataExt;
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let attrs = std::fs::metadata(path).unwrap().file_attributes() | 0x0000_1000;
        let ok = unsafe {
            windows::Win32::Storage::FileSystem::SetFileAttributesW(
                windows::core::PCWSTR(wide.as_ptr()),
                windows::Win32::Storage::FileSystem::FILE_FLAGS_AND_ATTRIBUTES(attrs),
            )
        };
        ok.expect("SetFileAttributesW(OFFLINE) should succeed on a temp file");
    }
}
