//! The COM surface: IShellExtInit and IContextMenu / 2 / 3.
//!
//! Split out of `contextmenu.rs` 2026-07-31 (pure move).

use super::*;

/// Command ids consumed by `QueryContextMenu`, given the leaf budget and whether the preview
/// slot was actually FILLED (not merely reserved). Pure so the invariant is unit-testable
/// without a real `IContextMenu2_Impl` call.
///
/// Gating on `preview_inserted` rather than "a preview id was reserved" matters: a reserved id
/// whose `insert_preview` call then failed (undecodable / timed-out tile) must not be counted,
/// or the next handler in the chain gets its id range shifted by one for an item that was never
/// actually added to the menu.
fn consumed_ids(budget: u32, preview_inserted: bool) -> u32 {
    if preview_inserted {
        budget + 1
    } else {
        budget
    }
}

impl IShellExtInit_Impl for ContextMenu_Impl {
    fn Initialize(
        &self,
        _pidlfolder: *const ITEMIDLIST,
        pdtobj: Ref<'_, IDataObject>,
        _hkeyprogid: HKEY,
    ) -> Result<()> {
        safety::guard(|| {
            let obj = pdtobj.ok()?;
            let paths = unsafe { hdrop_paths(obj)? };
            let preview_mode = settings::menu_preview();
            let eligible = settings::menu_enabled()
                && preview_mode != 0
                && paths.len() == 1
                && verbs::is_image(&paths[0])
                && preview_size_ok(&paths[0]);
            self.preview_eligible.set(eligible);
            *self.preview_job.borrow_mut() = if eligible {
                start_menu_thumb(&paths[0])
            } else {
                None
            };
            *self.paths.borrow_mut() = paths;
            Ok(())
        })
    }
}

impl IContextMenu_Impl for ContextMenu_Impl {
    fn QueryContextMenu(
        &self,
        hmenu: HMENU,
        indexmenu: u32,
        idcmdfirst: u32,
        idcmdlast: u32,
        uflags: u32,
    ) -> HRESULT {
        safety::guard_hr(|| {
            if uflags & CMF_DEFAULTONLY != 0 {
                return S_OK; // no default action to add
            }
            if !settings::menu_enabled() {
                return S_OK; // menu disabled in Options
            }
            let paths = self.paths.borrow();
            // Computed once and reused for the single-image preview gate below (for a
            // 1-file selection this `.any` IS `is_image(paths[0])`), so we don't probe
            // the same path's extension twice per right-click.
            let any_image = paths.iter().any(|p| verbs::is_image(p));
            // "Show on all file types": on an UNSUPPORTED selection, fall through to a
            // CONDENSED menu (file-agnostic utilities only) when the user opted in;
            // otherwise add nothing, as before.
            let condensed = !any_image;
            if condensed && !settings::menu_all_file_types() {
                return S_OK; // nothing for non-image selections
            }
            // An AUDIO-only selection (supported, so not condensed, but every file is a
            // music file) gets the audio view: the image-only verbs (Convert/Resize/
            // Rotate/Wallpaper/…) no-op or produce garbage on sound, so we drop them and
            // show only the audio-relevant set (Files to folder · Rename ▸ · Sort ▸ +
            // Settings). `all()` is false for an empty selection, but `condensed` already
            // is true then, so `!condensed` guards that.
            let audio_only = !condensed && paths.iter().all(|p| verbs::is_audio(p));
            // Honor the shell's allotted command-id range [idcmdfirst, idcmdlast]
            // (inclusive). Each leaf consumes one id; popup parents do not.
            // Overflowing the range can collide with a neighboring handler's ids
            // and misdispatch a click — so clamp to what we're allowed.
            let avail = (idcmdlast as usize)
                .saturating_sub(idcmdfirst as usize)
                .saturating_add(1);
            let leaves_n = verbs::leaves().len();
            let budget = leaves_n.min(avail) as u32;
            if budget == 0 {
                return S_OK;
            }

            // Menu preview: single image selection, enabled in Options, and the
            // id range has room for one extra command (offset = leaves_n, so the
            // InvokeCommand mapping stays stable even if leaves were clamped).
            self.preview_cmd.set(None);
            *self.preview.borrow_mut() = None;
            let mode = settings::menu_preview();
            // For a 1-file selection, `any_image` already == is_image(paths[0]).
            let single = paths.len() == 1 && any_image;
            // Reserve the bitmap preview slot when one is wanted and the file passed
            // the cheap initialization-time metadata gate. Decoding has already
            // started on a bounded worker for both placements during Initialize.
            if mode != 0 && single && avail > leaves_n && self.preview_eligible.get() {
                // The preview occupies the slot just past the last leaf;
                // id_for(Preview) encapsulates that "== leaves.len()" convention.
                self.preview_cmd
                    .set(Some(verbs::id_for(verbs::CmdSlot::Preview, idcmdfirst)));
            }

            unsafe {
                // One snapshot of the menu-item visibility subkey for this whole
                // build (the quick-verb loop + every build_menu_into node share it),
                // so a right-click does ONE key-open instead of one per item.
                let vis = settings::menu_visibility();

                // Our items grow downward from `indexmenu`: [preview?] [quick groups?]
                // [the "SageThumbs 2K" submenu] — all cohesive, in one place. (We ship
                // ONLY this classic handler now, not the packaged modern command, so the
                // menu can't double-list "SageThumbs 2K" — see AppxManifest.xml / register.rs.)
                let mut pos = indexmenu;
                // Whether `insert_preview` actually added a menu item, NOT merely whether a
                // command id was reserved: a reserved id whose insert then failed (undecodable
                // / timed-out tile) must not be counted in `consumed` below, or the next
                // handler in the chain gets its id range shifted by one for nothing.
                let mut preview_inserted = false;

                // 1) Preview directly on the main menu (mode 2), topmost. A bitmap item
                //    on a stock host, owner-drawn on a menu-skinned one — see
                //    `insert_preview` and the module header for why the host decides.
                if let Some(cmd) = self.preview_cmd.get() {
                    if mode == 2 && self.insert_preview(hmenu, pos, cmd) {
                        pos += 1;
                        preview_inserted = true;
                    }
                }

                // 2) Quick-verb groups directly on the main menu (Options toggle),
                //    below the preview. Each is built starting at its GLOBAL leaf
                //    index, so it reuses the submenu's command ids — a click on
                //    either copy invokes the same action and we claim no extra ids.
                // The quick verbs are Convert/Resize/Rotate — all image-only, so they're
                // suppressed for an audio-only selection too (same reason the audio view
                // drops them from the submenu below).
                //
                // Shown whenever the toggle is on, INCLUDING when the signed sparse package
                // is installed. An earlier build also gated on `!modern_menu_active()`, on
                // the theory that Windows bridges the packaged IExplorerCommand quick verbs
                // DOWN into this legacy "Show more options" menu, so our copies would
                // double-list. That premise is false: packaged context-menu verbs appear
                // ONLY in the modern COMPACT flyout, never in the classic menu — so the
                // suppression just made the quick verbs vanish for every user whose default
                // IS the classic menu (a very common Win11 setup), the "I enabled Show quick
                // actions and see nothing" report. Packaged (compact) and classic (this menu)
                // are separate surfaces; no single menu ever shows both, so nothing doubles.
                if settings::menu_quick_verbs() && !condensed && !audio_only {
                    for item in verbs::quick_items() {
                        // Honor per-item visibility: a hidden top-level item drops
                        // its quick-verb copy from the main menu too.
                        let qtitle = match &item {
                            verbs::QuickItem::Group(t, _, _) => *t,
                            verbs::QuickItem::Leaf(t, _) => *t,
                        };
                        if !vis.shown(qtitle) {
                            continue;
                        }
                        match item {
                            verbs::QuickItem::Group(title, children, start) => {
                                let Ok(qsub) = CreatePopupMenu() else {
                                    continue;
                                };
                                let mut n = start;
                                build_menu_into(qsub, children, idcmdfirst, &mut n, budget, &vis);
                                let _ = InsertMenuW(
                                    hmenu,
                                    pos,
                                    MF_BYPOSITION | MF_POPUP | MF_STRING,
                                    qsub.0 as usize,
                                    &HSTRING::from(crate::i18n::t(title)),
                                );
                                pos += 1;
                            }
                            verbs::QuickItem::Leaf(title, idx) => {
                                // A top-level leaf reusing its submenu command id:
                                // same global leaf index → same id_for() id → same action.
                                if idx < budget {
                                    let cmd = verbs::id_for(
                                        verbs::CmdSlot::Leaf(verbs::LeafId(idx)),
                                        idcmdfirst,
                                    );
                                    let _ = InsertMenuW(
                                        hmenu,
                                        pos,
                                        MF_BYPOSITION | MF_STRING,
                                        cmd as usize,
                                        &HSTRING::from(crate::i18n::t(title)),
                                    );
                                    pos += 1;
                                }
                            }
                        }
                    }
                    // No divider here: the quick verbs flow straight into the
                    // "SageThumbs 2K" entry so the whole group reads as one block.
                    // The separator goes BELOW that entry instead (see section 3).
                }

                // 3) The full "SageThumbs 2K" submenu, directly below the preview +
                // quick verbs (preview at its top in mode 1). This is the brand entry
                // with every verb + Settings — kept cohesive with the preview above it,
                // never "off on its own." We ship ONLY this classic handler (no packaged
                // modern command), so "SageThumbs 2K" is listed exactly once.
                if let Ok(hsub) = CreatePopupMenu() {
                    if let Some(cmd) = self.preview_cmd.get() {
                        if mode == 1 && self.insert_preview(hsub, 0, cmd) {
                            preview_inserted = true;
                            // Real Explorer does not reliably forward
                            // WM_INITMENUPOPUP for this child popup, so the row must
                            // exist before the submenu is handed to the parent.
                            let _ = InsertMenuW(
                                hsub,
                                1,
                                MF_BYPOSITION | MF_SEPARATOR,
                                0,
                                PCWSTR::null(),
                            );
                        }
                    }
                    // Build the top-level items in the user's saved order (drag-to-
                    // reorder in Settings). Each item keeps its ORIGINAL leaf-start
                    // index, so command ids stay stable — the dispatch side reads the
                    // default leaves()/slot_for, so only the insertion order changes.
                    // Full custom-ordered tree for a supported image selection; the
                    // audio-only set for a music selection; the condensed file-agnostic
                    // set for an unsupported one (show-on-all-file-types).
                    let top = if condensed {
                        verbs::condensed_top_level()
                    } else if audio_only {
                        verbs::audio_top_level()
                    } else {
                        verbs::ordered_top_level()
                    };
                    for (item, start_leaf) in top {
                        let mut leaf = start_leaf;
                        build_menu_into(
                            hsub,
                            std::slice::from_ref(item),
                            idcmdfirst,
                            &mut leaf,
                            budget,
                            &vis,
                        );
                    }
                    let _ = InsertMenuW(
                        hmenu,
                        pos,
                        MF_BYPOSITION | MF_POPUP | MF_STRING,
                        hsub.0 as usize,
                        &HSTRING::from("SageThumbs 2K"),
                    );
                    // Brand icon in front of "SageThumbs 2K" (hbmpItem, alpha-blended).
                    let logo = menu_logo();
                    if !logo.is_invalid() {
                        let mii = MENUITEMINFOW {
                            cbSize: core::mem::size_of::<MENUITEMINFOW>() as u32,
                            fMask: MIIM_BITMAP,
                            hbmpItem: logo,
                            ..Default::default()
                        };
                        let _ = SetMenuItemInfoW(hmenu, pos, true, &mii);
                    }
                    pos += 1;

                    // The single divider for our whole block goes BELOW the
                    // "SageThumbs 2K" entry, so the preview + quick verbs + this
                    // entry read as one cohesive "SageThumbs" group, fenced off from
                    // the rest of the menu (owner request).
                    let _ =
                        InsertMenuW(hmenu, pos, MF_BYPOSITION | MF_SEPARATOR, 0, PCWSTR::null());
                }
                // Command ids consumed: the preview slot (offset = leaf count) when a
                // preview was ACTUALLY added, else the leaves the submenu used (0 when
                // skipped). Claiming the leaf range is harmless when only the preview is
                // present.
                //
                // Report `budget`, NOT `leaves_n`: `budget = leaves_n.min(avail)` is what
                // `build_menu_into` was actually allowed to append, and `QueryContextMenu` must
                // never return an offset past the range the shell granted — overshooting pushes
                // the NEXT extension in the chain's `idCmdFirst` past `idCmdLast`. The `+ 1` is
                // safe because a preview id is only handed out when `avail > leaves_n`, which
                // also forces `budget == leaves_n`.
                //
                // Gated on `preview_inserted`, not `preview_cmd.get().is_some()`: see
                // `consumed_ids`'s doc comment for why that distinction is the whole fix.
                let consumed = consumed_ids(budget, preview_inserted);
                HRESULT(consumed as i32)
            }
        })
    }

    fn InvokeCommand(&self, pici: *const CMINVOKECOMMANDINFO) -> Result<()> {
        safety::guard(|| {
            let pici = unsafe { pici.as_ref().ok_or_else(|| Error::from(E_FAIL))? };
            let lp = pici.lpVerb.0 as usize;
            if (lp >> 16) != 0 {
                return Err(Error::from(E_FAIL)); // string verb, not the offset form
            }
            let offset = (lp & 0xFFFF) as u32;
            let leaves = verbs::leaves();
            // Map the raw offset back to a typed slot through the central slot_for(),
            // so the "preview == leaves.len()" convention isn't re-derived here.
            let action = match verbs::slot_for(offset, leaves.len() as u32) {
                Some(verbs::CmdSlot::Preview) if self.preview_cmd.get().is_some() => {
                    // The preview thumbnail itself: open the image.
                    if let Some(p) = self.paths.borrow().first() {
                        open_with_default(p);
                    }
                    return Ok(());
                }
                Some(verbs::CmdSlot::Leaf(verbs::LeafId(i))) => {
                    leaves.get(i as usize).ok_or_else(|| Error::from(E_FAIL))?.1
                }
                // Preview slot but no preview added, or out of our range entirely.
                _ => return Err(Error::from(E_FAIL)),
            };
            let paths = self.paths.borrow().clone();
            // Run the (possibly multi-file, multi-second) batch on a DETACHED worker so
            // this Invoke returns immediately instead of freezing explorer.exe's UI
            // thread; the worker surfaces errors + reveals new-folder output itself. The
            // shell window is the natural parent for any error dialog.
            verbs::run_action_detached(action, paths, Some(pici.hwnd.0 as isize));
            Ok(())
        })
    }

    fn GetCommandString(
        &self,
        _idcmd: usize,
        _utype: u32,
        _reserved: *const u32,
        _pszname: PSTR,
        _cchmax: u32,
    ) -> Result<()> {
        Err(Error::from(E_NOTIMPL))
    }
}

// Explorer forwards owner-draw measure/paint messages here. Bitmap items need no
// message handling; the submenu preview row is inserted during QueryContextMenu.
impl IContextMenu2_Impl for ContextMenu_Impl {
    fn HandleMenuMsg(&self, umsg: u32, wparam: WPARAM, lparam: LPARAM) -> Result<()> {
        safety::guard(|| {
            unsafe { self.menu_msg(umsg, wparam, lparam) };
            Ok(())
        })
    }
}

impl IContextMenu3_Impl for ContextMenu_Impl {
    fn HandleMenuMsg2(
        &self,
        umsg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
        plresult: *mut LRESULT,
    ) -> Result<()> {
        safety::guard(|| {
            let handled = unsafe { self.menu_msg(umsg, wparam, lparam) };
            if !plresult.is_null() {
                // WM_INITMENUPOPUP and owner-draw messages return TRUE when handled.
                unsafe { *plresult = LRESULT(handled as isize) };
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod consumed_ids_tests {
    use super::*;

    /// The bug this guards: `consumed` used to be driven by "was an id reserved", so a
    /// reserved-but-never-inserted preview still reported `budget + 1` and shifted the next
    /// chained handler's command-id range by one for an item that doesn't exist on the menu.
    #[test]
    fn a_reserved_but_uninserted_preview_must_not_be_counted() {
        assert_eq!(
            consumed_ids(5, false),
            5,
            "insert_preview failing must report only the leaves actually appended"
        );
    }

    #[test]
    fn an_actually_inserted_preview_adds_exactly_one() {
        assert_eq!(consumed_ids(5, true), 6);
    }

    #[test]
    fn a_zero_budget_with_no_preview_consumes_nothing() {
        assert_eq!(consumed_ids(0, false), 0);
    }
}
