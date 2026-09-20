//! The context-menu tree model: the `MenuItem` node kinds, the `MENU` tree, the
//! `VerbAction` leaf actions + their parameter enums, and the flattening helpers
//! (`leaves`, `quick_items`) the two menu surfaces dispatch through.

use image::ImageFormat;

use super::encode::{Resize, Target};

/// Desktop wallpaper placement.
#[derive(Clone, Copy)]
pub enum WallpaperMode {
    Stretch,
    Tile,
    Center,
    Fill,
    Fit,
    Span,
}

/// A lossy-but-non-destructive pixel transform (writes a new file, never the
/// original). Quarter-turns + flips, applied via the `image` crate.
#[derive(Clone, Copy)]
pub enum Transform {
    Right90,
    Left90,
    Rotate180,
    FlipH,
    FlipV,
}

/// A "shrink for email" preset: cap the longest edge to this many px, then
/// re-encode as a JPEG (small files that attach/send cleanly). Never upscales.
#[derive(Clone, Copy)]
pub enum EmailSize {
    Small,
    Medium,
    Large,
}

impl EmailSize {
    /// Longest-edge cap in pixels.
    pub(crate) fn max_edge(self) -> u32 {
        match self {
            EmailSize::Small => 640,
            EmailSize::Medium => 1024,
            EmailSize::Large => 1600,
        }
    }
}

/// A "compress to under N MB" preset for the one-click menu leaf (the CLI's `st2k
/// compress --max-size` takes an arbitrary byte target; the menu offers fixed
/// presets since a right-click verb has no text-entry field).
#[derive(Clone, Copy)]
pub enum CompressSize {
    Mb1,
    Mb5,
    Mb10,
}

impl CompressSize {
    /// Target size in bytes for [`crate::verbs::encode::compress_to_size`]. Decimal
    /// megabytes (1_000_000), matching the CLI's `parse_size` / the menu label.
    pub(crate) fn target_bytes(self) -> u64 {
        match self {
            CompressSize::Mb1 => 1_000_000,
            CompressSize::Mb5 => 5_000_000,
            CompressSize::Mb10 => 10_000_000,
        }
    }
}

/// How to name files for the batch-rename verb. The first two read EXIF (photos);
/// the last two read audio tags via `lofty` (music files).
#[derive(Clone, Copy)]
pub enum RenamePattern {
    /// `YYYY-MM-DD HH.MM.SS.ext` (EXIF capture time)
    DateTaken,
    /// `<camera> YYYY-MM-DD HH.MM.SS.ext` (EXIF)
    CameraDate,
    /// `<artist> - <title>.ext` (audio tags)
    ArtistTitle,
    /// `<NN> - <title>.ext` (audio tags; zero-padded track number)
    TrackTitle,
}

/// What a context-menu leaf verb does when invoked.
#[derive(Clone, Copy)]
pub enum VerbAction {
    Convert(Target),
    Transform(Transform),
    Clipboard,
    /// Base64-encode the file's raw bytes into a `data:<mime>;base64,…` URI and place
    /// it on the clipboard as text (the original file is untouched).
    CopyDataUri,
    /// Upload the selected image(s) to a keyless host — the companion app POSTs each
    /// and copies the returned link(s) to the clipboard; the originals are untouched.
    Upload,
    Wallpaper(WallpaperMode),
    /// Set as the Windows lock screen background (`Windows.System.UserProfile.LockScreen`).
    LockScreen,
    CombineToPdf,
    /// Combine the selected images into one CBZ (zip) comic archive.
    CombineToCbz,
    Ocr,
    ImageInfo,
    StripMetadata,
    ConvertDialog,
    OpenSettings,
    /// Resize to a new "(resized)" file (preset; never upscales).
    ResizeImg(Resize),
    /// Re-encode to a small "(email)" JPEG sibling at the given size preset.
    ShrinkForEmail(EmailSize),
    /// Shrink (JPEG quality + scale search) until the image fits under the given
    /// size preset — a "(compressed)" JPEG sibling. Never touches the original.
    CompressToSize(CompressSize),
    /// Batch-rename the selected images from their EXIF capture metadata.
    RenameByExif(RenamePattern),
    /// Open the "Rename with pattern…" dialog (companion app): a free-form template
    /// (`{name}`/`{ext}`/`{n}`/`{date}`/`{w}`/`{h}` + find/replace) with a live
    /// preview, for any file type — not just the four fixed metadata patterns above.
    RenameWithPattern,
    /// Make the selected image the icon of the folder that contains it.
    SetFolderIcon,
    /// Open the eyedropper window (in the companion app) to pick a color.
    Eyedropper,
    /// Create a folder and move the selected file(s) into it (1 file → named after
    /// it; many → a name-prompt dialog in the companion app).
    FilesToFolder,
    /// Move each selected image into a `WIDTHxHEIGHT` subfolder of its own folder.
    SortByDimensions,
    /// Move each selected image into a `YYYY-MM-DD` subfolder of its own folder, named
    /// from its EXIF capture date (the same date [`RenamePattern::DateTaken`] uses). A
    /// file with no capture date is skipped.
    SortByDateTaken,
    /// Sort selected audio files into folders by their tags (opens a dialog in
    /// the companion app: destination, template, copy/move).
    TagsToFolders,
    /// Save the video's frame as a standalone image ("(frame).png" sibling). Routed
    /// ALWAYS through the `st2k` helper (`thumbnail --size 0`) — video decode must
    /// never run in-process inside `explorer.exe`, so unlike every other routed verb
    /// this one has no in-process fallback.
    SaveVideoFrame,
}

/// One node of the context menu: a submenu (i18n-key title + children) or a leaf
/// verb (i18n-key title + action). The same tree drives the classic
/// `IContextMenu` (nested HMENUs) and the modern `IExplorerCommand` (nested
/// `EnumSubCommands`).
pub enum MenuItem {
    Group(&'static str, &'static [MenuItem]),
    Verb(&'static str, VerbAction),
    /// A visual divider between groups (classic menu only — the modern flyout has
    /// no separator concept, so it's skipped there). Consumes no command id.
    Separator,
}

impl MenuItem {
    pub fn title(&self) -> &'static str {
        match self {
            MenuItem::Group(t, _) | MenuItem::Verb(t, _) => t,
            MenuItem::Separator => "",
        }
    }
}

const fn convert(key: &'static str, format: ImageFormat, ext: &'static str) -> MenuItem {
    MenuItem::Verb(
        key,
        VerbAction::Convert(Target {
            format,
            ext,
            webp_quality: None,
        }),
    )
}

/// Quality for the quick "Convert into ▸ WebP" verb. WebP's whole point is small
/// files, so the one-click verb encodes LOSSY at this quality (libwebp) rather
/// than the pure-Rust lossless encoder (which can produce files larger than the
/// source). The Convert… dialog still offers lossless WebP via its settings.
/// 80 matches the dialog's default WebP quality.
const WEBP_LOSSY_QUALITY: u8 = 80;

/// The "SageThumbs 2K ▸" menu tree, in display order.
pub const MENU: &[MenuItem] = &[
    MenuItem::Group(
        "menu_convert_into",
        &[
            convert("menu_fmt_png", ImageFormat::Png, "png"),
            convert("menu_fmt_jpg", ImageFormat::Jpeg, "jpg"),
            // Two one-click WebP options: lossy (small files — what most people mean by
            // "convert to WebP") and lossless (perfect quality, larger). The dialog also
            // exposes lossless + a quality slider.
            MenuItem::Verb(
                "menu_fmt_webp",
                VerbAction::Convert(Target {
                    format: ImageFormat::WebP,
                    ext: "webp",
                    webp_quality: Some(WEBP_LOSSY_QUALITY),
                }),
            ),
            convert("menu_fmt_webp_lossless", ImageFormat::WebP, "webp"),
            // AVIF (AV1 still image) — the modern "smaller than WebP/JPEG" target. The
            // `image` crate can't encode it, so this routes through the bundled
            // ImageMagick (see `encode::ext_needs_magick`); same engine the Convert…
            // dialog uses for AVIF. On a compact (no-magick) install the encode fails
            // gracefully (the file is just reported as not converted).
            convert("menu_fmt_avif", ImageFormat::Avif, "avif"),
            convert("menu_fmt_bmp", ImageFormat::Bmp, "bmp"),
            convert("menu_fmt_gif", ImageFormat::Gif, "gif"),
            convert("menu_fmt_tiff", ImageFormat::Tiff, "tiff"),
            convert("menu_fmt_ico", ImageFormat::Ico, "ico"),
        ],
    ),
    MenuItem::Verb("menu_convert_dialog", VerbAction::ConvertDialog),
    MenuItem::Verb("menu_combine_pdf", VerbAction::CombineToPdf),
    MenuItem::Verb("menu_combine_cbz", VerbAction::CombineToCbz),
    // Video-only verb: the sole leaf video_top_level() surfaces besides the
    // file-agnostic ones. Lives at top level (not nested) like Combine ▸ PDF/CBZ, so
    // it's a single click on a video-only selection.
    MenuItem::Verb("menu_save_video_frame", VerbAction::SaveVideoFrame),
    MenuItem::Separator,
    MenuItem::Group(
        "menu_resize",
        &[
            MenuItem::Verb(
                "menu_resize_1080",
                VerbAction::ResizeImg(Resize::Fit(1920, 1080)),
            ),
            MenuItem::Verb(
                "menu_resize_720",
                VerbAction::ResizeImg(Resize::Fit(1280, 720)),
            ),
            MenuItem::Verb(
                "menu_resize_600",
                VerbAction::ResizeImg(Resize::Fit(800, 600)),
            ),
            MenuItem::Verb("menu_resize_50", VerbAction::ResizeImg(Resize::Percent(50))),
            MenuItem::Verb("menu_resize_25", VerbAction::ResizeImg(Resize::Percent(25))),
        ],
    ),
    MenuItem::Group(
        "menu_email",
        &[
            MenuItem::Verb(
                "menu_email_small",
                VerbAction::ShrinkForEmail(EmailSize::Small),
            ),
            MenuItem::Verb(
                "menu_email_medium",
                VerbAction::ShrinkForEmail(EmailSize::Medium),
            ),
            MenuItem::Verb(
                "menu_email_large",
                VerbAction::ShrinkForEmail(EmailSize::Large),
            ),
        ],
    ),
    MenuItem::Group(
        "menu_rotate",
        &[
            MenuItem::Verb(
                "menu_rotate_right",
                VerbAction::Transform(Transform::Right90),
            ),
            MenuItem::Verb("menu_rotate_left", VerbAction::Transform(Transform::Left90)),
            MenuItem::Verb(
                "menu_rotate_180",
                VerbAction::Transform(Transform::Rotate180),
            ),
            MenuItem::Verb("menu_flip_h", VerbAction::Transform(Transform::FlipH)),
            MenuItem::Verb("menu_flip_v", VerbAction::Transform(Transform::FlipV)),
        ],
    ),
    MenuItem::Separator,
    MenuItem::Group(
        "menu_rename",
        &[
            MenuItem::Verb(
                "menu_rename_date",
                VerbAction::RenameByExif(RenamePattern::DateTaken),
            ),
            MenuItem::Verb(
                "menu_rename_camera",
                VerbAction::RenameByExif(RenamePattern::CameraDate),
            ),
            MenuItem::Verb(
                "menu_rename_artist_title",
                VerbAction::RenameByExif(RenamePattern::ArtistTitle),
            ),
            MenuItem::Verb(
                "menu_rename_track_title",
                VerbAction::RenameByExif(RenamePattern::TrackTitle),
            ),
            MenuItem::Verb("menu_rename_pattern", VerbAction::RenameWithPattern),
        ],
    ),
    MenuItem::Verb("menu_files_to_folder", VerbAction::FilesToFolder),
    MenuItem::Group(
        "menu_sort",
        &[
            MenuItem::Verb("menu_sort_dimensions", VerbAction::SortByDimensions),
            MenuItem::Verb("menu_sort_date", VerbAction::SortByDateTaken),
            MenuItem::Verb("menu_sort_tags", VerbAction::TagsToFolders),
        ],
    ),
    MenuItem::Separator,
    // The old "Tools" submenu, flattened to TOP-LEVEL verbs so each can be individually
    // shown/hidden + reordered via the "Menu items" customization (no extra submenu to
    // wrangle). Leaf order is unchanged (OCR · info · color · strip), so command ids stay
    // stable; each is gated like any other top-level item.
    MenuItem::Verb("menu_copy_text", VerbAction::Ocr),
    MenuItem::Verb("menu_image_info", VerbAction::ImageInfo),
    MenuItem::Verb("menu_pick_color", VerbAction::Eyedropper),
    MenuItem::Verb("menu_strip_meta", VerbAction::StripMetadata),
    MenuItem::Verb("menu_copy", VerbAction::Clipboard),
    MenuItem::Verb("menu_copy_data_uri", VerbAction::CopyDataUri),
    MenuItem::Verb("menu_upload", VerbAction::Upload),
    MenuItem::Separator,
    MenuItem::Verb("menu_set_folder_icon", VerbAction::SetFolderIcon),
    MenuItem::Group(
        "menu_wallpaper",
        &[
            MenuItem::Verb(
                "menu_wallpaper_stretch",
                VerbAction::Wallpaper(WallpaperMode::Stretch),
            ),
            MenuItem::Verb(
                "menu_wallpaper_tile",
                VerbAction::Wallpaper(WallpaperMode::Tile),
            ),
            MenuItem::Verb(
                "menu_wallpaper_center",
                VerbAction::Wallpaper(WallpaperMode::Center),
            ),
            MenuItem::Verb(
                "menu_wallpaper_fill",
                VerbAction::Wallpaper(WallpaperMode::Fill),
            ),
            MenuItem::Verb(
                "menu_wallpaper_fit",
                VerbAction::Wallpaper(WallpaperMode::Fit),
            ),
            MenuItem::Verb(
                "menu_wallpaper_span",
                VerbAction::Wallpaper(WallpaperMode::Span),
            ),
        ],
    ),
    MenuItem::Verb("menu_lock_screen", VerbAction::LockScreen),
    MenuItem::Group(
        "menu_compress",
        &[
            MenuItem::Verb(
                "menu_compress_1mb",
                VerbAction::CompressToSize(CompressSize::Mb1),
            ),
            MenuItem::Verb(
                "menu_compress_5mb",
                VerbAction::CompressToSize(CompressSize::Mb5),
            ),
            MenuItem::Verb(
                "menu_compress_10mb",
                VerbAction::CompressToSize(CompressSize::Mb10),
            ),
        ],
    ),
    MenuItem::Separator,
    MenuItem::Verb("menu_settings", VerbAction::OpenSettings),
];

/// Depth-first list of every leaf verb (title + action), in menu order. The
/// classic surface assigns command ids in this order and dispatches by offset.
pub fn leaves() -> Vec<(&'static str, VerbAction)> {
    fn walk(items: &'static [MenuItem], out: &mut Vec<(&'static str, VerbAction)>) {
        for it in items {
            match it {
                MenuItem::Group(_, children) => walk(children, out),
                MenuItem::Verb(title, action) => out.push((title, *action)),
                MenuItem::Separator => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(MENU, &mut out);
    out
}

// ---- Typed classic command ids ------------------------------------------
//
// The classic `IContextMenu` surface identifies every clickable item by a u32
// command id the shell hands back in `InvokeCommand`. Those ids are *offsets*
// from the shell-allotted `idcmdfirst`, in depth-first leaf order — except for
// the owner-drawn preview item, which (by convention) lives at the slot just
// past the last leaf (`offset == leaves().len()`). That convention + the offset
// arithmetic used to be open-coded at every assign/dispatch site; it now lives
// here so a single pair of functions (`id_for` / `slot_for`) is the only place
// that knows the mapping.

/// A leaf verb's global index in [`leaves`] (depth-first menu order). This is the
/// offset, relative to `idcmdfirst`, that the classic surface assigns to the leaf
/// — and the same index a quick-verb copy reuses so both fire the same action.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct LeafId(pub u32);

/// A clickable classic-menu slot: either a leaf verb or the owner-drawn preview
/// item. Centralizes the "preview sits just past the last leaf" convention.
pub enum CmdSlot {
    Leaf(LeafId),
    Preview,
}

/// The absolute menu command id for `slot`, given the shell's `idcmdfirst`. A
/// leaf maps to `idcmdfirst + leaf.0`; the preview maps to the slot one past the
/// last leaf (`idcmdfirst + leaves().len()`). [`slot_for`] is its inverse.
pub fn id_for(slot: CmdSlot, idcmdfirst: u32) -> u32 {
    let offset = match slot {
        CmdSlot::Leaf(LeafId(i)) => i,
        // The preview's offset depends on how many leaves precede it. We use the
        // full menu's leaf count so the id is stable even if a clamped budget cut
        // some trailing leaves from the *drawn* menu (the dispatch side agrees).
        CmdSlot::Preview => leaf_count(),
    };
    idcmdfirst + offset
}

/// Inverse of [`id_for`]: map a raw command `offset` (already relative to
/// `idcmdfirst`) back to the slot it identifies, given the menu's `n_leaves`.
/// `offset < n_leaves` → that leaf; `offset == n_leaves` → the preview; anything
/// past that is not one of ours (`None`).
pub fn slot_for(offset: u32, n_leaves: u32) -> Option<CmdSlot> {
    if offset < n_leaves {
        Some(CmdSlot::Leaf(LeafId(offset)))
    } else if offset == n_leaves {
        Some(CmdSlot::Preview)
    } else {
        None
    }
}

/// Total leaf verbs in the whole `MENU` tree (the preview's offset). Cheap walk
/// shared by [`id_for`] so we don't allocate a `leaves()` Vec just to count. `pub(crate)`
/// so `contextmenu/com.rs`'s `QueryContextMenu` can read just the count on every
/// right-click instead of calling `leaves()` (which allocates and fills a ~46-entry
/// `Vec`) only to immediately discard it and keep `.len()`.
pub(crate) fn leaf_count() -> u32 {
    MENU.iter().map(count_leaves).sum()
}

/// Top-level MENU items surfaced directly on the MAIN context menu when the
/// "quick verbs" Option is on (the most-used actions, one click instead of two).
/// In MENU order this yields: Convert into ▸ · Convert… · Resize ▸ · Rotate ▸.
pub const QUICK_KEYS: &[&str] = &[
    "menu_convert_into",
    "menu_convert_dialog",
    "menu_resize",
    "menu_rotate",
];

/// Count the leaf verbs under a menu item (separators / the group node itself
/// don't count). Used to map each top-level item to its first global leaf index,
/// and (pub) by the classic surface to advance the leaf counter past a hidden
/// top-level item so command ids stay aligned with the full tree.
pub fn count_leaves(item: &MenuItem) -> u32 {
    match item {
        MenuItem::Group(_, children) => children.iter().map(count_leaves).sum(),
        MenuItem::Verb(..) => 1,
        MenuItem::Separator => 0,
    }
}

/// A quick-menu item: either a submenu group (title, children, start leaf index)
/// or a top-level leaf (title, leaf index). The index lets the classic surface
/// reuse the SAME command ids as the in-submenu copy, so a click on either fires
/// the same action and the handler claims no extra ids.
pub enum QuickItem {
    Group(&'static str, &'static [MenuItem], u32),
    Leaf(&'static str, u32),
}

/// The quick-menu items (groups + leaves) in MENU display order, each with its
/// starting/own global leaf index.
pub fn quick_items() -> Vec<QuickItem> {
    let mut out = Vec::new();
    let mut idx = 0u32;
    for it in MENU {
        if QUICK_KEYS.contains(&it.title()) {
            match it {
                MenuItem::Group(t, children) => out.push(QuickItem::Group(t, children, idx)),
                MenuItem::Verb(t, _) => out.push(QuickItem::Leaf(t, idx)),
                MenuItem::Separator => {}
            }
        }
        idx += count_leaves(it);
    }
    out
}

/// The top-level MENU items to DISPLAY, in the user's saved order
/// ([`crate::settings::menu_order`]), each paired with its ORIGINAL leaf-start index.
/// The original index keeps command ids STABLE regardless of display order — dispatch
/// reads the original [`leaves`]/[`slot_for`], so only the INSERTION order changes,
/// never the id→action mapping. With no saved order this is just the default MENU
/// items (separators included) in tree order. Reorderable = the top-level toggle items;
/// `menu_settings` stays last (after a divider), and any item missing from a stale
/// saved order is appended in default order.
/// The token persisted in `MenuOrder` for a user-placed separator (divider) row. Item
/// keys are all `menu_*`, so this can never collide with one.
pub const MENU_SEP_TOKEN: &str = "--";

/// The factory top-level order as persisted tokens — each reorderable item's key and
/// [`MENU_SEP_TOKEN`] for each divider, in tree order, EXCLUDING the always-last
/// `menu_settings` and its preceding divider (the menu re-adds that automatically).
/// Seeds the Settings reorder list and backs "Reset order" / "Defaults".
pub fn default_menu_tokens() -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    for it in MENU {
        match it {
            // Drop a leading divider + collapse consecutive ones.
            MenuItem::Separator
                if !out.is_empty() && out.last().copied() != Some(MENU_SEP_TOKEN) =>
            {
                out.push(MENU_SEP_TOKEN);
            }
            MenuItem::Group(t, _) | MenuItem::Verb(t, _) if *t != "menu_settings" => out.push(t),
            _ => {} // menu_settings is the always-last tail, never in the saved order
        }
    }
    while out.last().copied() == Some(MENU_SEP_TOKEN) {
        out.pop(); // trailing divider (the one before Settings) — re-added by the builder
    }
    out
}

pub fn ordered_top_level() -> Vec<(&'static MenuItem, u32)> {
    order_top_level_with(&crate::settings::menu_order())
}

/// Pure core of [`ordered_top_level`]: apply `saved` (top-level item keys +
/// [`MENU_SEP_TOKEN`] divider markers, e.g. from `settings::menu_order`) to the default
/// `MENU`. Each item keeps its ORIGINAL leaf-start index so command ids stay stable —
/// only the INSERTION order changes. Dividers render exactly where the user placed them
/// (a leading/consecutive/trailing divider is normalized away), then one divider + the
/// always-last `menu_settings`. Empty `saved` → the default tree verbatim. Split from
/// the registry read so it's unit-testable.
fn order_top_level_with(saved: &[String]) -> Vec<(&'static MenuItem, u32)> {
    let mut pairs: Vec<(&'static MenuItem, u32)> = Vec::with_capacity(MENU.len());
    let mut idx = 0u32;
    for it in MENU {
        pairs.push((it, idx));
        idx += count_leaves(it);
    }
    if saved.is_empty() {
        return pairs;
    }
    let item = |key: &str| {
        pairs
            .iter()
            .copied()
            .find(|(it, _)| !matches!(it, MenuItem::Separator) && it.title() == key)
    };
    let sep = pairs
        .iter()
        .copied()
        .find(|(it, _)| matches!(it, MenuItem::Separator));
    let reorderable = |t: &str| !t.is_empty() && t != "menu_settings";

    // Body in saved order (items de-duped, dividers as placed), then any item missing
    // from a stale saved order appended in default order.
    let mut body: Vec<(&'static MenuItem, u32)> = Vec::new();
    let mut seen: Vec<&'static str> = Vec::new();
    for tok in saved {
        if tok == MENU_SEP_TOKEN {
            body.extend(sep);
        } else if reorderable(tok) && !seen.contains(&tok.as_str()) {
            if let Some(p) = item(tok) {
                body.push(p);
                seen.push(p.0.title());
            }
        }
    }
    for &(it, s) in &pairs {
        let t = it.title();
        if reorderable(t) && !seen.contains(&t) {
            body.push((it, s));
            seen.push(t);
        }
    }

    let mut out = normalize_dividers(body);
    // Tail: one divider, then the always-last Settings entry.
    out.extend(sep);
    out.extend(item("menu_settings"));
    out
}

/// Drop a leading divider, collapse consecutive ones, drop a trailing one (the always-on
/// divider before Settings stands in for any trailing divider).
fn normalize_dividers(body: Vec<(&'static MenuItem, u32)>) -> Vec<(&'static MenuItem, u32)> {
    let mut out: Vec<(&'static MenuItem, u32)> = Vec::with_capacity(body.len() + 2);
    for p in body {
        if matches!(p.0, MenuItem::Separator)
            && out
                .last()
                .is_none_or(|last| matches!(last.0, MenuItem::Separator))
        {
            continue;
        }
        out.push(p);
    }
    while matches!(out.last().map(|p| p.0), Some(MenuItem::Separator)) {
        out.pop();
    }
    out
}

/// The CONDENSED top-level items shown on an UNSUPPORTED selection when the "show on all
/// file types" Option is on: only the file-agnostic utilities (Files to folder · Sort
/// into folders · Rename · Pick color), then a divider + the always-last Settings. Each
/// carries its ORIGINAL leaf-start index so command ids match the default [`leaves`] and
/// dispatch is unchanged (a click maps to the same action as on the full menu).
pub fn condensed_top_level() -> Vec<(&'static MenuItem, u32)> {
    // Only verbs that actually DO something on a file we can't read: move-to-folder + the
    // system-wide colour picker. Sort-into-folders and the whole Rename group are dropped here
    // — MOST of Rename keys off image dimensions / EXIF / audio tags, so on a truly unsupported
    // file (e.g. a .docx) it'd silently no-op. "Rename with pattern…" alone would actually work
    // on any file, but it lives in the same group and this list is group-granular, not
    // leaf-granular, so it's excluded along with its siblings rather than split out.
    // (Audio files take `audio_top_level` instead, where Sort/Rename DO apply.)
    const KEYS: &[&str] = &["menu_files_to_folder", "menu_pick_color"];
    top_level_subset(KEYS)
}

/// The AUDIO-only top-level items shown when every selected file is audio (music
/// files): only the verbs that mean something for audio — Files to folder · Rename ▸
/// (its artist-title / track-title patterns) · Sort ▸ (by tags) — then a divider + the
/// always-last Settings. The image-only verbs (Convert/Resize/Rotate/Wallpaper/…) are
/// dropped because they no-op or produce garbage on a sound file. Mirrors
/// [`condensed_top_level`] exactly: each item carries its ORIGINAL leaf-start index so
/// command ids match the default [`leaves`] and dispatch is unchanged (a click maps to
/// the same action as on the full menu). KEYS are kept in sync with
/// [`top_level_audio_ok`] (which adds the always-shown Settings).
pub fn audio_top_level() -> Vec<(&'static MenuItem, u32)> {
    // Pick color is a system-wide screen picker (works regardless of the selected file), so it
    // belongs here too — it was previously offered on the condensed (unsupported) menu but not
    // the audio one, an inconsistency.
    const KEYS: &[&str] = &[
        "menu_files_to_folder",
        "menu_rename",
        "menu_sort",
        "menu_pick_color",
    ];
    top_level_subset(KEYS)
}

/// The VIDEO-only top-level items shown when every selected file is a video: the
/// frame-grab leaf plus the same file-agnostic utilities `condensed_top_level` offers
/// (Files to folder · Pick color), then a divider + the always-last Settings. The
/// image-only verbs (Convert/Resize/Rotate/Wallpaper/…) are dropped — none of them
/// read a video file — same shape as [`audio_top_level`]/[`condensed_top_level`]:
/// each item keeps its ORIGINAL leaf-start index so command ids match the default
/// [`leaves`] and dispatch is unchanged.
pub fn video_top_level() -> Vec<(&'static MenuItem, u32)> {
    const KEYS: &[&str] = &[
        "menu_save_video_frame",
        "menu_files_to_folder",
        "menu_pick_color",
    ];
    top_level_subset(KEYS)
}

/// Shared walk behind [`condensed_top_level`], [`audio_top_level`] and
/// [`video_top_level`]: collect every top-level `MENU` item whose `title()` is in
/// `keys`, plus the trailing separator+Settings, each carrying its ORIGINAL
/// leaf-start index so command ids match the default [`leaves`] and dispatch is
/// unchanged. The three callers differ only in which keys they pass — the walk
/// itself had drifted into byte-identical copies before this shared it.
fn top_level_subset(keys: &[&str]) -> Vec<(&'static MenuItem, u32)> {
    let mut items: Vec<(&'static MenuItem, u32)> = Vec::new();
    let mut sep: Option<(&'static MenuItem, u32)> = None;
    let mut settings: Option<(&'static MenuItem, u32)> = None;
    let mut idx = 0u32;
    for it in MENU {
        if matches!(it, MenuItem::Separator) {
            sep.get_or_insert((it, idx));
        } else if it.title() == "menu_settings" {
            settings = Some((it, idx));
        } else if keys.contains(&it.title()) {
            items.push((it, idx));
        }
        idx += count_leaves(it);
    }
    if let Some(s) = sep {
        items.push(s);
    }
    if let Some(st) = settings {
        items.push(st);
    }
    items
}

/// Is this TOP-LEVEL menu item meaningful for an AUDIO-only selection? True for the
/// audio-relevant verbs ([`audio_top_level`]'s KEYS) plus the always-shown Settings;
/// false for the image-only verbs. The modern Win11 flyout can't filter its top-level
/// list (its `EnumSubCommands` has no selection context — see `command.rs`), so it gates
/// each item's `GetState` on this instead, returning `ECS_HIDDEN` for an image-only
/// top-level verb when the selection is audio-only. Keep in sync with
/// [`audio_top_level`].
/// Which TOP-LEVEL titles survive a VIDEO-ONLY selection in the modern flyout — the mirror of
/// [`top_level_audio_ok`], and the same set [`video_top_level`] builds for the classic menu.
/// Both surfaces have to agree: a verb offered on one and hidden on the other is the exact
/// class of bug the 2026-07-21 quick-verb incident was (CLAUDE.md §6), where the two menus
/// disagreed about what a selection supported and a verb ended up appearing nowhere.
pub fn top_level_video_ok(title: &str) -> bool {
    matches!(
        title,
        "menu_save_video_frame" | "menu_files_to_folder" | "menu_pick_color" | "menu_settings"
    )
}

pub fn top_level_audio_ok(title: &str) -> bool {
    matches!(
        title,
        "menu_files_to_folder" | "menu_rename" | "menu_sort" | "menu_pick_color" | "menu_settings"
    )
}

#[cfg(test)]
mod tests;
