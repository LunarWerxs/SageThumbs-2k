//! Classic per-extension registration for the thumbnail provider.
//!
//! This is the Phase-1 path: a plain in-proc COM server registered via
//! `regsvr32`. Thumbnail providers do NOT need package identity (only the
//! Phase-2 context menu does), and the shell runs us out-of-process in its
//! isolated host automatically.
//!
//! KNOWN LIMITATION (Phase 1): Windows resolves a thumbnail handler in priority
//! order — per-user UserChoice ProgID, then the extension's default ProgID's
//! `shellex`, then `SystemFileAssociations`, then the bare-extension key. We
//! register the last two (non-invasively). For formats whose default ProgID
//! already carries a thumbnail handler (e.g. .jpg/.png via the Photos app),
//! that handler still wins. That's acceptable: SageThumbs' value is the formats
//! Windows can't thumbnail at all, where the bare/association key wins. The
//! Phase-2 sparse-package `fileTypeAssociation/ThumbnailHandler` path is the
//! clean, identity-based fix that sidesteps this precedence entirely.

use windows::core::Result;
use windows::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
use windows_registry::{CLASSES_ROOT, LOCAL_MACHINE};

use crate::guids::{CLSID_CONTEXT_MENU_STR, CLSID_THUMBNAIL_PROVIDER_STR};
use crate::settings;

const NAME: &str = "SageThumbs 2K Thumbnail Provider";
const CM_NAME: &str = "SageThumbs 2K Context Menu";
/// The IThumbnailProvider shell-extension handler category GUID.
const THUMB_HANDLER: &str = "{E357FCCD-A995-4576-B01F-234630154E96}";
const APPROVED: &str =
    r"SOFTWARE\Microsoft\Windows\CurrentVersion\Shell Extensions\Approved";

pub fn register(dll_path: &str) -> Result<()> {
    // "Approved Shell Extensions" is mandatory on locked-down systems.
    let approved = LOCAL_MACHINE.create(APPROVED)?;

    // The thumbnail provider's COM server.
    register_inproc_server(CLSID_THUMBNAIL_PROVIDER_STR, NAME, dll_path, &approved)?;

    // Hook each enabled extension; explicitly unhook disabled ones so a
    // re-register reflects the Options format list (matches the legacy
    // RegisterExtensions-on-OK behavior). Best-effort per extension: a single
    // failing key (transient lock, locked-down subtree) must NOT abort the whole
    // register and skip the context-menu setup + shell-notify below.
    for (ext, _) in crate::formats::FORMATS {
        if settings::format_enabled(ext) {
            let _ = hook_ext(ext);
        } else {
            unhook_ext(ext);
        }
    }

    // The classic IContextMenu handler's COM server (for classic-menu machines:
    // StartAllBack, ExplorerPatcher, or the {86ca1aa0…} tweak). Registered under
    // "*" (all files) and filtered to images inside QueryContextMenu.
    register_inproc_server(CLSID_CONTEXT_MENU_STR, CM_NAME, dll_path, &approved)?;
    CLASSES_ROOT
        .create("*\\shellex\\ContextMenuHandlers\\SageThumbs2K")?
        .set_string("", CLSID_CONTEXT_MENU_STR)?;

    notify_shell();
    Ok(())
}

/// Register one in-proc COM server: `CLSID\{guid}` (friendly name) +
/// `InprocServer32` (dll path, Apartment threading) + the Approved entry.
/// Both of our coclasses configure identically through here.
fn register_inproc_server(
    clsid_str: &str,
    name: &str,
    dll_path: &str,
    approved: &windows_registry::Key,
) -> Result<()> {
    let base = format!("CLSID\\{clsid_str}");
    CLASSES_ROOT.create(&base)?.set_string("", name)?;
    let inproc = CLASSES_ROOT.create(format!("{base}\\InprocServer32"))?;
    inproc.set_string("", dll_path)?;
    inproc.set_string("ThreadingModel", "Apartment")?;
    approved.set_string(clsid_str, name)?;
    Ok(())
}

/// The two `shellex` thumbnail-handler key paths for one extension: the
/// bare-extension key (lowest-priority lookup) and the association-independent
/// `SystemFileAssociations` key (consulted first, without clobbering any app's
/// ProgID-level handler). One source of truth for the key layout.
fn thumb_keys(ext: &str) -> [String; 2] {
    [
        format!(".{ext}\\shellex\\{THUMB_HANDLER}"),
        format!("SystemFileAssociations\\.{ext}\\shellex\\{THUMB_HANDLER}"),
    ]
}

/// Point one extension's thumbnail `shellex` keys at our CLSID.
fn hook_ext(ext: &str) -> Result<()> {
    for path in thumb_keys(ext) {
        CLASSES_ROOT.create(path)?.set_string("", CLSID_THUMBNAIL_PROVIDER_STR)?;
    }
    Ok(())
}

/// Remove one extension's thumbnail `shellex` keys — but only the ones that
/// actually point at OUR CLSID, so we never clobber a handler another product
/// (or Windows) registered in that slot.
fn unhook_ext(ext: &str) {
    for path in thumb_keys(ext) {
        remove_if_ours(&path);
    }
}

/// Delete a thumbnail-handler `shellex` key only if its default value is our
/// CLSID. A foreign handler in that slot is left untouched.
fn remove_if_ours(path: &str) {
    if let Ok(key) = CLASSES_ROOT.open(path) {
        if key.get_string("").ok().as_deref() == Some(CLSID_THUMBNAIL_PROVIDER_STR) {
            let _ = CLASSES_ROOT.remove_tree(path);
        }
    }
}

pub fn unregister() -> Result<()> {
    for (ext, _) in crate::formats::FORMATS {
        unhook_ext(ext);
    }
    let _ = CLASSES_ROOT.remove_tree(format!("CLSID\\{CLSID_THUMBNAIL_PROVIDER_STR}"));
    let _ = CLASSES_ROOT.remove_tree("*\\shellex\\ContextMenuHandlers\\SageThumbs2K");
    let _ = CLASSES_ROOT.remove_tree(format!("CLSID\\{CLSID_CONTEXT_MENU_STR}"));
    if let Ok(approved) = LOCAL_MACHINE.open(APPROVED) {
        let _ = approved.remove_value(CLSID_THUMBNAIL_PROVIDER_STR);
        let _ = approved.remove_value(CLSID_CONTEXT_MENU_STR);
    }
    notify_shell();
    Ok(())
}

fn notify_shell() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}
