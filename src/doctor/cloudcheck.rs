//! Cloud sync folders: is our cloud-folder provider in each one's thumbnail slot, the only
//! handler Explorer asks for a file there (see `register::cloud` for why).

use super::*;
use crate::register::cloud::{self, SyncRoot};

pub(super) fn check_cloud_folders(r: &mut Report) {
    r.head("Cloud sync folders (OneDrive, Synology Drive, ...)");
    let roots = cloud::sync_roots();
    if roots.is_empty() {
        r.line(S::Info, "Sync folders", "none registered on this PC");
        return;
    }
    let on = st2k_base::settings::cloud_thumbs();
    r.line(
        S::Info,
        "Thumbnails in cloud folders",
        if on {
            "on"
        } else {
            "off (Settings -> General turns it on)"
        },
    );
    if on && !cloud::provider_registered() {
        r.fail_with_fix(
            "Cloud-folder provider",
            "not registered, so no cloud folder can show our thumbnails",
            "Settings -> Advanced -> 'Repair file associations' registers it again.",
        );
    }
    let mut unlinked = 0usize;
    for root in &roots {
        let (status, detail) = root_line(root, on);
        if on && status == S::Warn {
            unlinked += 1;
        }
        r.line(status, root.provider_name(), &detail);
    }
    if unlinked > 0 {
        r.fail_with_fix(
            "Cloud folders not linked",
            &format!(
                "{unlinked} sync folder(s) still hand every thumbnail to the cloud app alone, so \
                 our formats show a stock icon there"
            ),
            "open SageThumbs Settings once (that re-links them), or sign out and back in.",
        );
    }
}

/// One sync root's line: whose provider is in its slot, and what that means for our formats.
fn root_line(root: &SyncRoot, on: bool) -> (S, String) {
    let folders = if root.folders.is_empty() {
        "(no folder listed)".to_string()
    } else {
        root.folders.join(", ")
    };
    if root.is_chained() {
        let after = root
            .chained
            .as_deref()
            .filter(|c| !c.trim().is_empty())
            .unwrap_or("nothing, the folder had no provider");
        return (
            S::Ok,
            format!(
                "{folders}\n         SageThumbs first, then the folder's own provider: {after}"
            ),
        );
    }
    if root.aumid.is_some() && root.provider.is_none() {
        return (
            S::Info,
            format!("{folders}\n         a packaged app that draws its own thumbnails; left alone"),
        );
    }
    let holder = root.provider.as_deref().unwrap_or("none");
    if on {
        (
            S::Warn,
            format!("{folders}\n         not linked: its thumbnail slot holds {holder}"),
        )
    } else {
        (
            S::Info,
            format!("{folders}\n         not linked (the setting is off)"),
        )
    }
}
