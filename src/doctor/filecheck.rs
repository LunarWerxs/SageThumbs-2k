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
/// local, the bytes are not, and reading them pulls the file down over the network. Our
/// cloud-folder provider (`crate::cloudthumb`) therefore never reads an online-only file: it
/// hands it to the cloud app's own provider, so our thumbnail for it appears only once the
/// bytes are here. Said out loud, because "why is this one blank" has exactly that answer.
/// `std::fs::metadata` reads attributes without triggering recall, so this check itself never
/// pulls anything down.
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
        "Cloud file (online-only)",
        &format!(
            "the bytes are not on this PC yet ({size}). SageThumbs never downloads a file just to \
             draw its thumbnail, so until it is here the cloud app's own thumbnail (if it has one \
             for .{ext}) is what Explorer shows"
        ),
        "Right-click the file or its folder -> 'Always keep on this device'. Once the bytes are \
         local, SageThumbs draws it like any other file.",
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
    let abs = st2k_base::fsutil::parsing_path(path);
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
             order: read the 'Thumbnail handler' and 'Cloud-synced folder' lines above (another \
             program's handler, or a cloud folder we are not linked into), then rebuild the \
             thumbnail cache (Settings > Advanced). Copying the file to a plain local folder and \
             re-running this command tells the cases apart in one step.",
        ),
    }
}

/// Is this file inside a cloud sync folder (OneDrive, Synology Drive, ...), and is our
/// cloud-folder provider linked into that folder's one thumbnail slot? Explorer asks ONLY that
/// slot for a file there (`register::cloud` has the measurement), so this decides whether our
/// thumbnail can appear at all. Purely a registry read; nothing is hydrated and nothing is written.
fn cloud_sync_root_note(r: &mut Report, p: &Path) {
    use crate::register::cloud;
    let Ok(file) = p.canonicalize() else {
        return;
    };
    let file = file
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_string();
    let Some(root) = cloud::sync_roots()
        .into_iter()
        .find(|root| root.folders.iter().any(|f| cloud::path_is_under(&file, f)))
    else {
        return;
    };
    let provider = root.provider_name().to_string();
    if root.is_chained() {
        r.line(
            S::Ok,
            "Cloud-synced folder",
            &format!(
                "inside a {provider} sync folder, and SageThumbs is linked into its thumbnail \
                 slot: downloaded files get our thumbnails, online-only ones get {provider}'s"
            ),
        );
    } else if root.aumid.is_some() && root.provider.is_none() {
        r.line(
            S::Warn,
            "Cloud-synced folder",
            &format!(
                "inside a {provider} sync folder. {provider} is a packaged app that draws its own \
                 thumbnails there, and SageThumbs leaves it alone, so this file shows whatever \
                 {provider} draws for it"
            ),
        );
    } else {
        r.fail_with_fix(
            "Cloud-synced folder",
            &format!(
                "inside a {provider} sync folder, where Explorer asks ONLY {provider} for \
                 thumbnails, and SageThumbs is not linked in there"
            ),
            "Settings -> General -> turn on 'Thumbnails in OneDrive & cloud folders' (opening \
             Settings also re-links a folder the cloud app took back).",
        );
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
    let Some(log) = st2k_base::safety::log_file().filter(|l| l.exists()) else {
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
    if !st2k_codecs::video::media_foundation_available() {
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
    let Some(info) = st2k_codecs::vcodec::identify(&mut file) else {
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
    match info.subtype.map(st2k_codecs::vcodec::decoder_installed) {
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
    if st2k_codecs::vcodec::cover_art(&mut file).is_some() {
        let detail = if st2k_base::settings::prefer_cover_art() {
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
    snap: &st2k_base::settings::FormatEnabledSnapshot,
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
    if !st2k_base::formats::is_known(&ext) {
        r.fail_with_fix(
            &format!(".{ext}"),
            "NOT a format SageThumbs handles — Explorer will never ask us for it",
            "this file type isn't supported; open an issue to request it",
        );
        return;
    }
    r.line(S::Ok, &format!(".{ext}"), "a supported format");
    thumbnail_handler_note(r, &ext);
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
        st2k_base::formats::category(&ext),
        st2k_base::formats::Category::Video
    );
    if is_video {
        video_codec_note(r, path);
    }

    // The decisive step: actually run the thumbnail decoder on THIS file's bytes, the
    // same preview-fidelity path Explorer's provider uses.
    match st2k_codecs::decode::read_preview_capped(path) {
        Err(e) => report_unbuffered(r, path, &e),
        Ok(bytes) => report_decode(r, path, &bytes, is_video),
    }
}

/// A file the buffered read refused. Past the in-memory ceiling Explorer does not give up: it
/// runs the streamed rescues, and `decode_oversized_path` is that same cascade, so the report
/// runs it too rather than calling a 421 MB scanned book unreadable (issue #59's report did).
/// Under the ceiling it bows out at once and the read error stands.
fn report_unbuffered(r: &mut Report, path: &str, e: &std::io::Error) {
    match st2k_codecs::decode::decode_oversized_path(path, 256) {
        Some(img) => {
            r.line(
                S::Ok,
                "Decode this file",
                &format!(
                    "OK ({}x{}) off the stream: too big to hold in memory, read the way \
                     Explorer reads it",
                    img.width(),
                    img.height()
                ),
            );
            // Then ask the shell, exactly as the buffered success path does.
            shell_roundtrip(r, path);
        }
        None => r.fail_with_fix(
            "Read file",
            &format!("could not read the bytes: {e}"),
            "check the file isn't locked or truncated; past the size limit no streamed rescue \
             read it either, and with Debug logging on the log's last lines name the step \
             that failed",
        ),
    }
}

/// Decodes the file's bytes and reports the decode outcome: a thumbnail that can be
/// produced, a video with no frame, or a failure with the matching hint.
fn report_decode(r: &mut Report, path: &str, bytes: &[u8], is_video: bool) {
    match st2k_codecs::decode::decode_preview(bytes) {
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
            let magick = st2k_codecs::decode::magick_available();
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

/// Which thumbnail handler Windows resolves for `.ext` through the normal association lookup,
/// and whether this PC can even load it. Issue #47 is the shape this catches: on an ARM64 PC
/// the shell answered 0x800700C1 (ERROR_BAD_EXE_FORMAT) for an .epub and never asked us,
/// because some other program had registered an x64-only DLL as the .epub thumbnail handler.
fn thumbnail_handler_note(r: &mut Report, ext: &str) {
    const LABEL: &str = "Thumbnail handler";
    let Some(clsid) = (unsafe { crate::cloudthumb::type_thumbnail_handler(ext) }) else {
        r.line(
            S::Warn,
            LABEL,
            &format!("Windows resolves NO thumbnail handler for .{ext}"),
        );
        return;
    };
    if clsid == st2k_base::guids::CLSID_THUMBNAIL_PROVIDER {
        r.line(
            S::Ok,
            LABEL,
            "SageThumbs 2K (Windows resolves ours for this type)",
        );
        return;
    }
    let braced = format!("{clsid:?}");
    let dll = CLASSES_ROOT
        .open(format!("CLSID\\{{{braced}}}\\InprocServer32"))
        .and_then(|k| k.get_string(""))
        .ok();
    let arch = dll
        .as_deref()
        .and_then(|d| std::fs::read(expand_env(d)).ok())
        .and_then(|b| pe_machine(&b));
    let who = format!(
        "{{{braced}}} {}",
        dll.as_deref().unwrap_or("(no InprocServer32)")
    );
    match arch {
        Some(m) if !machine_loads_here(m) => r.fail_with_fix(
            LABEL,
            &format!(
                "another program's handler wins for .{ext}, and its DLL is built for {} while this \
                 PC runs {}: Explorer cannot load it (0x800700C1) and never gets to ours — {who}",
                machine_name(m),
                std::env::consts::ARCH
            ),
            "update or uninstall the program that installed that DLL (its path names it), then \
             Settings -> Advanced -> 'Repair file associations'.",
        ),
        // Not necessarily wrong (Windows' own handler winning for .jpg is by design, see
        // `register.rs`), so a warning that names it rather than a failure.
        _ => r.line(
            S::Warn,
            LABEL,
            &format!("another program's handler is used for .{ext}, not ours: {who}"),
        ),
    }
}

/// `%SystemRoot%`-style variables in a registry path, expanded (an InprocServer32 value may be
/// REG_EXPAND_SZ).
fn expand_env(path: &str) -> String {
    let mut out = String::new();
    let mut rest = path;
    while let Some(a) = rest.find('%') {
        let Some(b) = rest[a + 1..].find('%') else {
            break;
        };
        out.push_str(&rest[..a]);
        let name = &rest[a + 1..a + 1 + b];
        out.push_str(&std::env::var(name).unwrap_or_else(|_| format!("%{name}%")));
        rest = &rest[a + 2 + b..];
    }
    out.push_str(rest);
    out
}

/// The PE `Machine` field of an executable image, or `None` when `bytes` is not one.
pub(super) fn pe_machine(bytes: &[u8]) -> Option<u16> {
    let e_lfanew = u32::from_le_bytes(bytes.get(0x3C..0x40)?.try_into().ok()?) as usize;
    if bytes.get(..2)? != b"MZ" || bytes.get(e_lfanew..e_lfanew + 4)? != b"PE\0\0" {
        return None;
    }
    Some(u16::from_le_bytes(
        bytes.get(e_lfanew + 4..e_lfanew + 6)?.try_into().ok()?,
    ))
}

const MACHINE_X86: u16 = 0x014C;
const MACHINE_X64: u16 = 0x8664;
const MACHINE_ARM64: u16 = 0xAA64;

/// Can a DLL built for `machine` load into a process like this one? (An ARM64X DLL reports
/// ARM64 and loads into both; an x64 or x86 DLL cannot load into a native ARM64 process.)
fn machine_loads_here(machine: u16) -> bool {
    match std::env::consts::ARCH {
        "aarch64" => machine == MACHINE_ARM64,
        "x86_64" => machine == MACHINE_X64,
        "x86" => machine == MACHINE_X86,
        _ => true,
    }
}

fn machine_name(machine: u16) -> &'static str {
    match machine {
        MACHINE_X86 => "32-bit x86",
        MACHINE_X64 => "x64",
        MACHINE_ARM64 => "ARM64",
        _ => "another architecture",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The architecture verdict for issue #47 rests on reading a DLL's PE machine correctly:
    /// this very test binary must read as loadable here, and a non-image as nothing.
    #[test]
    fn the_running_image_reads_as_loadable_and_a_non_image_as_nothing() {
        let me = std::fs::read(std::env::current_exe().unwrap()).unwrap();
        let machine = pe_machine(&me).expect("a PE image");
        assert!(machine_loads_here(machine), "{machine:#06x}");
        assert_eq!(pe_machine(b"MZ not really an image"), None);
    }

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
