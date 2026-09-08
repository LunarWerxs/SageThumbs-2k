//! Context-menu verb actions (M6).
//!
//! Each action operates on a list of selected file paths (extracted from the
//! shell's IShellItemArray in command.rs). Conversion uses the `image` crate's
//! encoders and writes the result alongside the original.
//!
//! This module is a thin facade: the implementation is split across four
//! submodules and re-exported here so every existing caller keeps reaching the
//! items as `verbs::<Name>` unchanged.
//!
//! - [`menu`] — the `MenuItem` / `MENU` tree, `VerbAction` + its parameter enums,
//!   and the flattening helpers (`leaves`, `quick_items`).
//! - [`encode`] — decode → resize/flatten → encode primitives, the `Target` /
//!   `Resize` / `ConvertOpts` descriptors, and the per-file convert/transform/
//!   resize/email entry points.
//! - [`fileops`] — generic move/copy/sort helpers and the folder-mover verbs
//!   (files-to-folder, dimensions/tags-to-folders) + combine-to-CBZ.
//! - [`actions`] — `run_action` dispatch, clipboard, wallpaper, batch-rename,
//!   set-folder-icon, image-info, and the companion-app launchers.

mod actions;
mod encode;
mod fileops;
mod menu;
mod outcome;

// ---- Public surface (matches each item's ORIGINAL visibility) -----------
// `#[allow(unused_imports)]`: several of these re-exports are consumed only by the
// sibling `SageThumbs2K` (app) / `st2k` BINARY crates (and the test module), which the
// lib-only build can't see — so they read as "unused" here despite being load-bearing
// public API. (Items the lib itself uses — `is_image`, `run_action`, `MENU`,
// `leaves`, … — don't warn; the attribute just covers the bin-only ones.)

// Menu-tree model + flattening helpers.
#[allow(unused_imports)]
pub use menu::{
    audio_top_level, condensed_top_level, count_leaves, default_menu_tokens, id_for, leaves,
    ordered_top_level, quick_items, slot_for, top_level_audio_ok, CmdSlot, EmailSize, LeafId,
    MenuItem, QuickItem, RenamePattern, Transform, VerbAction, WallpaperMode, MENU, MENU_SEP_TOKEN,
    QUICK_KEYS,
};
// The leaf COUNT alone, for the QueryContextMenu id budget: cheaper than `leaves().len()`,
// which allocated the whole ~46-entry Vec on every right-click just to read its length.
pub(crate) use menu::leaf_count;

// Encode / convert / resize primitives and descriptors.
#[allow(unused_imports)]
pub use encode::{
    compress_to_size, convert_file, convert_file_opts, convert_file_opts_named,
    convert_image_to_pdf_in, convert_to, convert_to_magick_in, convert_to_magick_in_named,
    convert_to_reporting, resize_file, shrink_for_email, transform_file, ConvertOpts, Corner,
    Resize, Target, Watermark,
};
pub(crate) use encode::{flatten_onto_white, read_full_fidelity_capped};

// Folder/sort verbs + the CBZ archiver.
#[allow(unused_imports)]
pub use fileops::{combine_to_cbz, files_to_folder, tags_to_folders};

// The per-input result model the PDF/CBZ composers return (2026-09-05 audit, F31), and the
// per-file one every bulk run reports through (F11).
pub(crate) use outcome::{partition, refusal};
#[allow(unused_imports)]
pub use outcome::{BatchReport, Combined, FileOutcome, FileStatus, OmitCause, Omitted, OnOmit};

// Dispatch + the non-encode actions.
#[allow(unused_imports)]
pub use actions::{
    copy_rgba_to_clipboard, copy_to_clipboard, is_audio, is_image, run_action, run_action_detached,
    ActionReport,
};
// The free-pattern rename engine's bin-crate surface: the companion app's "Rename
// with pattern…" dialog (`src/bin/app/rename_dlg.rs`) calls these directly, the same
// way it calls `files_to_folder` above.
#[allow(unused_imports)]
pub use actions::{rename_by_pattern, rename_pattern_preview};

// Crate-internal helpers surfaced ONLY for the in-crate `tests` module below
// (module-private in the monolith). `#[cfg(test)]` so they don't warn as unused
// in a normal (non-test) lib build — they're reached only via `super::*` in tests.
pub(crate) use actions::{prepare_wallpaper_in, set_folder_icon};
#[cfg(test)]
pub(crate) use actions::{rename_one, set_wallpaper, tag_base};
// `write_atomic` is reachable in normal builds too: `topdf` writes through it.
#[cfg(test)]
pub(crate) use encode::{apply_resize, convert_to_magick};
#[allow(unused_imports)]
pub(crate) use encode::{with_tmp_suffix, write_atomic};
#[cfg(test)]
pub(crate) use fileops::{combined_path, expand_template, sanitize_component, sort_by_dimensions};

// Imports the original monolithic file kept at module scope for the in-crate
// `tests` (test-only, so gated to avoid unused-import warnings in normal builds).
#[cfg(test)]
use core::ffi::c_void;
#[cfg(test)]
use image::ImageFormat;
#[cfg(test)]
use std::iter::once;
#[cfg(test)]
use std::os::windows::ffi::OsStrExt;
#[cfg(test)]
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPIF_SENDCHANGE, SPIF_UPDATEINIFILE, SPI_SETDESKWALLPAPER,
};

#[cfg(test)]
mod tests;
