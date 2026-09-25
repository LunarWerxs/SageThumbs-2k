//! The COM objects a UI Automation client talks to: the overlay root (fragment root, selection container) and one element per toolbar button, flyout row and swatch. Every read goes through the hub's on_ui_thread.

use super::*;

pub(super) fn i4_array(values: &[i32]) -> Result<*mut SAFEARRAY> {
    let len = u32::try_from(values.len()).unwrap_or(0);
    let psa = unsafe { SafeArrayCreateVector(VT_I4, 0, len) };
    if psa.is_null() {
        return Err(Error::from(E_FAIL));
    }
    for (i, v) in values.iter().enumerate() {
        let idx = i32::try_from(i).unwrap_or(i32::MAX);
        let put = unsafe { SafeArrayPutElement(psa, &idx, (v as *const i32).cast()) };
        if put.is_err() {
            let _ = unsafe { SafeArrayDestroy(psa) };
            return Err(Error::from(E_FAIL));
        }
    }
    Ok(psa)
}

/// The fragment root: the overlay window itself, and the container of the one selection set
/// (the active tool).
///
/// It holds the HWND and nothing else. A `&mut Shot` cached in here would be exactly the
/// dangling pointer `UiaDisconnectProvider` exists to prevent, and even before the window
/// closes it would be stale: the toolbar is laid out afresh from the selection rect and the
/// DPI, so a cached rect is wrong the moment the region moves.
#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IRawElementProviderFragmentRoot,
    ISelectionProvider
)]
pub(super) struct ShotRoot {
    pub(super) hwnd: isize,
}

impl ShotRoot_Impl {
    pub(super) fn children(&self) -> Vec<FocusTarget> {
        on_ui_thread(self.hwnd, |_, s| unsafe { children(s) }).unwrap_or_default()
    }

    pub(super) fn child(&self, id: FocusTarget) -> Result<IRawElementProviderFragment> {
        element_provider(hwnd_of(self.hwnd), id).map_or_else(no_element, Ok)
    }
}

impl IRawElementProviderSimple_Impl for ShotRoot_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, patternid: UIA_PATTERN_ID) -> Result<IUnknown> {
        const P_SELECTION: i32 = UIA_SelectionPatternId.0;
        if patternid.0 == P_SELECTION {
            let sel = ComObjectInterface::<ISelectionProvider>::as_interface_ref(self).to_owned();
            return sel.cast::<IUnknown>();
        }
        no_element()
    }

    fn GetPropertyValue(&self, propertyid: UIA_PROPERTY_ID) -> Result<VARIANT> {
        const P_NAME: i32 = UIA_NamePropertyId.0;
        const P_AUTOMATION_ID: i32 = UIA_AutomationIdPropertyId.0;
        const P_IS_CONTROL: i32 = UIA_IsControlElementPropertyId.0;
        const P_IS_CONTENT: i32 = UIA_IsContentElementPropertyId.0;
        // Control type is deliberately NOT answered: the host provider below reports the real
        // window, and claiming to be a toolbar would be a lie about a fullscreen surface that
        // is mostly canvas.
        Ok(match propertyid.0 {
            P_NAME => VARIANT::from("Screenshot region editor"),
            P_AUTOMATION_ID => VARIANT::from("screenshot.editor"),
            P_IS_CONTROL | P_IS_CONTENT => VARIANT::from(true),
            _ => VARIANT::default(),
        })
    }

    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        unsafe { UiaHostProviderFromHwnd(hwnd_of(self.hwnd)) }
    }
}

impl IRawElementProviderFragment_Impl for ShotRoot_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        // Matched on the raw discriminant, because the windows-rs constants are not
        // upper-case and using them directly as patterns trips `non_upper_case_globals`.
        const FIRST_CHILD: i32 = NavigateDirection_FirstChild.0;
        const LAST_CHILD: i32 = NavigateDirection_LastChild.0;
        // The root's parent is the desktop, which UIA derives from the host window, and a root
        // has no siblings of its own, so only the two child directions cost a message hop.
        if !matches!(direction.0, FIRST_CHILD | LAST_CHILD) {
            return no_element();
        }
        let kids = self.children();
        let id = if direction.0 == FIRST_CHILD {
            kids.first().copied()
        } else {
            kids.last().copied()
        };
        id.map_or_else(no_element, |id| self.child(id))
    }

    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        // NULL means "I have no id of my own": UIA uses the host window's, which is correct
        // for a fragment root that IS a window.
        Ok(core::ptr::null_mut())
    }

    fn BoundingRectangle(&self) -> Result<UiaRect> {
        // An empty rect tells UIA to use the host window's rect, which is exactly the overlay.
        Ok(UiaRect::default())
    }

    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(core::ptr::null_mut())
    }

    fn SetFocus(&self) -> Result<()> {
        // The overlay takes the foreground for itself when it opens (`activate_overlay`), and
        // it has no child windows to move focus between, so there is nothing to do here that
        // would not be a lie about what happened.
        Ok(())
    }

    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        Ok(
            ComObjectInterface::<IRawElementProviderFragmentRoot>::as_interface_ref(self)
                .to_owned(),
        )
    }
}

impl IRawElementProviderFragmentRoot_Impl for ShotRoot_Impl {
    fn ElementProviderFromPoint(&self, x: f64, y: f64) -> Result<IRawElementProviderFragment> {
        let px = x as i32;
        let py = y as i32;
        let hit = on_ui_thread(self.hwnd, move |_, s| unsafe { hit_test(s, px, py) }).flatten();
        // Off the chrome (i.e. over the canvas) is NOT an element: answering with the root
        // would make every pixel of the frozen screenshot claim to be a control.
        hit.map_or_else(no_element, |id| self.child(id))
    }

    fn GetFocus(&self) -> Result<IRawElementProviderFragment> {
        let focus = on_ui_thread(self.hwnd, |_, s| s.focus).flatten();
        focus.map_or_else(no_element, |id| self.child(id))
    }
}

impl ISelectionProvider_Impl for ShotRoot_Impl {
    fn GetSelection(&self) -> Result<*mut SAFEARRAY> {
        let selected = on_ui_thread(self.hwnd, |_, s| unsafe { selected_tool(s) }).flatten();
        let psa = unsafe { SafeArrayCreateVector(VT_UNKNOWN, 0, u32::from(selected.is_some())) };
        if psa.is_null() {
            return Err(Error::from(E_FAIL));
        }
        if let Some(id) = selected {
            let put = element_provider(hwnd_of(self.hwnd), id)
                .and_then(|f| f.cast::<IRawElementProviderSimple>().ok())
                .map(|simple| {
                    let idx = 0i32;
                    // SafeArrayPutElement AddRefs an interface element, so `simple` stays ours
                    // to drop on the way out of this closure.
                    unsafe { SafeArrayPutElement(psa, &idx, simple.as_raw()) }
                });
            if !matches!(put, Some(Ok(()))) {
                let _ = unsafe { SafeArrayDestroy(psa) };
                return Err(Error::from(E_FAIL));
            }
        }
        Ok(psa)
    }

    fn CanSelectMultiple(&self) -> Result<windows::core::BOOL> {
        Ok(false.into())
    }

    fn IsSelectionRequired(&self) -> Result<windows::core::BOOL> {
        // A tool is always active; there is no "no tool" state to fall back to.
        Ok(true.into())
    }
}

/// The toolbar item whose tool is the active one, if the bar is up.
pub(super) unsafe fn selected_tool(s: &mut Shot) -> Option<FocusTarget> {
    let sel = s.sel?;
    if s.ocr_mode {
        return None;
    }
    let dpi = unsafe { shot_dpi_for_sel(s, sel) };
    let buttons = unsafe { toolbar_layout_cached(s, sel, dpi) };
    button_index(&buttons, Button::Tool(s.tool)).map(FocusTarget::Toolbar)
}

/// One control of the chrome: a toolbar button, a palette swatch, or a text-flyout row.
///
/// Holds the window and layer 1's index for the item, and nothing else, for the reasons given
/// on [`ShotRoot`]. The index is not a private numbering: it is the same `FocusTarget` the
/// keyboard model uses, so `Invoke` here and Space there reach the same code.
#[implement(
    IRawElementProviderSimple,
    IRawElementProviderFragment,
    IInvokeProvider,
    ISelectionItemProvider
)]
pub(super) struct ShotElement {
    pub(super) hwnd: isize,
    pub(super) id: FocusTarget,
}

impl ShotElement_Impl {
    pub(super) fn facts(&self) -> Option<ElementFacts> {
        let id = self.id;
        on_ui_thread(self.hwnd, move |_, s| unsafe { describe(s, id) }).flatten()
    }

    /// Ask the UI thread to do something to this element. Posted, never sent: see
    /// [`WM_UIA_INVOKE`].
    pub(super) fn post(&self, msg: u32) -> Result<()> {
        if LIVE_OVERLAY.load(Ordering::Acquire) != self.hwnd {
            return Err(Error::from(HRESULT(UIA_E_ELEMENTNOTAVAILABLE as i32)));
        }
        let (wparam, lparam) = encode_target(self.id);
        let posted = unsafe { PostMessageW(Some(hwnd_of(self.hwnd)), msg, wparam, lparam) }.is_ok();
        if posted {
            Ok(())
        } else {
            Err(Error::from(HRESULT(UIA_E_ELEMENTNOTAVAILABLE as i32)))
        }
    }

    pub(super) fn sibling(&self, forward: bool) -> Result<IRawElementProviderFragment> {
        let kids = on_ui_thread(self.hwnd, |_, s| unsafe { children(s) }).unwrap_or_default();
        let Some(at) = kids.iter().position(|k| *k == self.id) else {
            return no_element();
        };
        let next = if forward {
            at.checked_add(1).and_then(|n| kids.get(n))
        } else {
            at.checked_sub(1).and_then(|n| kids.get(n))
        };
        match next {
            Some(id) => element_provider(hwnd_of(self.hwnd), *id).map_or_else(no_element, Ok),
            None => no_element(),
        }
    }
}

impl IRawElementProviderSimple_Impl for ShotElement_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, patternid: UIA_PATTERN_ID) -> Result<IUnknown> {
        const P_INVOKE: i32 = UIA_InvokePatternId.0;
        const P_SELECTION_ITEM: i32 = UIA_SelectionItemPatternId.0;
        if patternid.0 == P_INVOKE {
            let inv = ComObjectInterface::<IInvokeProvider>::as_interface_ref(self).to_owned();
            return inv.cast::<IUnknown>();
        }
        // Only the mutually exclusive tools are a selection; an action button that also
        // advertised SelectionItem would have a screen reader announce "not selected" after
        // every press of Undo.
        if patternid.0 == P_SELECTION_ITEM && self.facts().is_some_and(|f| f.selected.is_some()) {
            let si =
                ComObjectInterface::<ISelectionItemProvider>::as_interface_ref(self).to_owned();
            return si.cast::<IUnknown>();
        }
        no_element()
    }

    fn GetPropertyValue(&self, propertyid: UIA_PROPERTY_ID) -> Result<VARIANT> {
        const P_NAME: i32 = UIA_NamePropertyId.0;
        const P_CONTROL_TYPE: i32 = UIA_ControlTypePropertyId.0;
        const P_IS_ENABLED: i32 = UIA_IsEnabledPropertyId.0;
        const P_IS_KEYBOARD_FOCUSABLE: i32 = UIA_IsKeyboardFocusablePropertyId.0;
        const P_HAS_KEYBOARD_FOCUS: i32 = UIA_HasKeyboardFocusPropertyId.0;
        const P_AUTOMATION_ID: i32 = UIA_AutomationIdPropertyId.0;
        const P_IS_CONTROL: i32 = UIA_IsControlElementPropertyId.0;
        const P_IS_CONTENT: i32 = UIA_IsContentElementPropertyId.0;
        let Some(f) = self.facts() else {
            // VT_EMPTY is "I do not answer this", which is what a vanished element should say
            // rather than an error or a stale value.
            return Ok(VARIANT::default());
        };
        Ok(match propertyid.0 {
            P_NAME => VARIANT::from(f.name.as_str()),
            P_CONTROL_TYPE => VARIANT::from(f.control_type.0),
            P_IS_ENABLED => VARIANT::from(f.enabled),
            P_IS_KEYBOARD_FOCUSABLE => VARIANT::from(true),
            P_HAS_KEYBOARD_FOCUS => VARIANT::from(f.focused),
            P_AUTOMATION_ID => VARIANT::from(f.automation_id.as_str()),
            P_IS_CONTROL | P_IS_CONTENT => VARIANT::from(true),
            _ => VARIANT::default(),
        })
    }

    fn HostRawElementProvider(&self) -> Result<IRawElementProviderSimple> {
        // Only the fragment ROOT is hosted in a window; its children are drawn, not windowed.
        no_element()
    }
}

impl IRawElementProviderFragment_Impl for ShotElement_Impl {
    fn Navigate(&self, direction: NavigateDirection) -> Result<IRawElementProviderFragment> {
        const PARENT: i32 = NavigateDirection_Parent.0;
        const NEXT: i32 = NavigateDirection_NextSibling.0;
        const PREVIOUS: i32 = NavigateDirection_PreviousSibling.0;
        match direction.0 {
            PARENT => root_provider(hwnd_of(self.hwnd)).map_or_else(no_element, Ok),
            NEXT => self.sibling(true),
            PREVIOUS => self.sibling(false),
            // The chrome is one flat list: nothing here contains anything else.
            _ => no_element(),
        }
    }

    fn GetRuntimeId(&self) -> Result<*mut SAFEARRAY> {
        i4_array(&runtime_id_parts(self.id))
    }

    fn BoundingRectangle(&self) -> Result<UiaRect> {
        // An element that is no longer painted reports an empty rect rather than its last
        // known one, so a highlight never sits over a control that has gone.
        Ok(self.facts().map(|f| f.rect).unwrap_or_default())
    }

    fn GetEmbeddedFragmentRoots(&self) -> Result<*mut SAFEARRAY> {
        Ok(core::ptr::null_mut())
    }

    fn SetFocus(&self) -> Result<()> {
        self.post(WM_UIA_FOCUS)
    }

    fn FragmentRoot(&self) -> Result<IRawElementProviderFragmentRoot> {
        root_provider(hwnd_of(self.hwnd))
            .and_then(|f| f.cast::<IRawElementProviderFragmentRoot>().ok())
            .map_or_else(no_element, Ok)
    }
}

impl IInvokeProvider_Impl for ShotElement_Impl {
    fn Invoke(&self) -> Result<()> {
        self.post(WM_UIA_INVOKE)
    }
}

impl ISelectionItemProvider_Impl for ShotElement_Impl {
    fn Select(&self) -> Result<()> {
        // Selecting a tool IS pressing its button, so it goes down the same path rather than
        // reaching into `s.tool`, which would skip everything `handle_button` does around it
        // (committing an in-progress text box, closing the flyouts, dropping a grabbed shape).
        self.post(WM_UIA_INVOKE)
    }

    fn AddToSelection(&self) -> Result<()> {
        // Single-selection container: adding to the selection is not a thing you can do.
        Err(Error::from(HRESULT(UIA_E_INVALIDOPERATION as i32)))
    }

    fn RemoveFromSelection(&self) -> Result<()> {
        Err(Error::from(HRESULT(UIA_E_INVALIDOPERATION as i32)))
    }

    fn IsSelected(&self) -> Result<windows::core::BOOL> {
        Ok(self
            .facts()
            .and_then(|f| f.selected)
            .unwrap_or(false)
            .into())
    }

    fn SelectionContainer(&self) -> Result<IRawElementProviderSimple> {
        // The fragment root. The tree is flat (there is no separate toolbar element between
        // the window and its buttons), so the root IS the container of the tool set.
        root_provider(hwnd_of(self.hwnd))
            .and_then(|f| f.cast::<IRawElementProviderSimple>().ok())
            .map_or_else(no_element, Ok)
    }
}
