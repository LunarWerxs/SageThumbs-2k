//! Keeping the cloud sync folders' thumbnail slots in step with the `CloudThumbs` setting from the
//! app side: the Settings switch, the sign-in task (`--cloud-relink`) and the ordinary launches.
//! The chain itself lives in `sagethumbs2k_core::register::cloud`.

use sagethumbs2k_core::register::cloud;

/// Apply the setting now: chain or hand back this user's sync folders, and create or drop the
/// sign-in re-link task. A portable copy gets no task: it leaves no autostart behind, the same
/// rule the hotkey helper follows (`screenshot::enable::autostart_allowed`).
pub(crate) fn sync(on: bool) {
    let _ = cloud::relink();
    if let Ok(exe) = std::env::current_exe() {
        cloud::sync_relink_task(&exe, on && !st2k_base::settings::portable());
    }
}

/// Re-apply the chain if a cloud client has put its own provider back since (OneDrive may when
/// it re-registers its folder). A few registry reads when nothing changed.
pub(crate) fn relink() {
    let _ = cloud::relink();
}
