//! Layer 2 of the region editor's accessibility work: a UI Automation provider, so a screen
//! reader can find, name and operate the annotation chrome.
//!
//! Layer 1 gave a keyboard-only user a route to every control ([`FocusTarget`] and the key
//! handling in `input.rs`). That fixed the hands, not the eyes: the editor is a single
//! owner-drawn popup with no child windows at all, so to Narrator, NVDA or any automation
//! enumerator the whole thing is one blank rectangle. Nothing in it has a name, a role, a
//! bounding box or an invoke verb. This file is that missing tree.
//!
//! Three rules shape everything below, and each of them was arrived at by rejecting the
//! easier thing:
//!
//! 1. **Elements are addressed by layer 1's own indices.** A provider that numbered the
//!    buttons itself would be a second truth, free to drift from the list the mouse and the
//!    keyboard walk. [`FocusTarget`] IS the element id here, so "the third toolbar item" means
//!    exactly the same thing to a click, to Tab and to a screen reader, by construction.
//! 2. **Actions go through the existing seams.** `Invoke` sets `s.focus` and calls layer 1's
//!    `invoke_focus`, which is the same `actions::handle_button` / `apply_swatch` /
//!    `apply_text_item` path a click takes. A reimplementation "just for automation" is how an
//!    accessible control ends up doing something subtly different from the visible one.
//! 3. **No provider method ever touches `Shot` on its own thread.** UIA calls arrive on
//!    uiautomationcore's worker threads, not on the overlay's message loop, so a direct
//!    `shot_ptr` deref there would race every mouse move and every keystroke, and an `Invoke`
//!    would call `DestroyWindow` and `PostQuitMessage` from the wrong thread entirely. Every
//!    read is marshalled onto the UI thread with a sent message, and every WRITE is a POSTED
//!    message, see [`on_ui_thread`] and [`WM_UIA_INVOKE`].
//!
//! `ProviderOptions_UseComThreading` was considered for rule 3 and rejected: it would make UIA
//! marshal calls into our apartment for us, but only if the overlay thread is a COM apartment,
//! which it is not guaranteed to be (the capture can run before anything else in the app has
//! initialised COM). An explicit message hop depends on nothing but the message loop that is
//! already there.
//!
//! `panic = "abort"` applies here with unusual force: these are vtable methods called from
//! outside our process, and a panic in one aborts the whole application while a fullscreen
//! topmost window is up. Nothing in this file unwraps, indexes without a bounds check, or
//! assumes a lookup succeeded.

use super::*;
mod naming;
use naming::*;
mod providers;
use crate::uia::{hwnd_of, no_element};
use providers::*;

use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, Ordering};
use std::cell::Cell;
use std::sync::Mutex;

use windows::core::{implement, ComObjectInterface, Error, IUnknown, Interface, Result, HRESULT};
use windows::Win32::System::Com::SAFEARRAY;
use windows::Win32::System::Ole::{SafeArrayCreateVector, SafeArrayDestroy, SafeArrayPutElement};
use windows::Win32::System::Variant::{VARIANT, VT_I4, VT_UNKNOWN};
use windows::Win32::UI::Accessibility::{
    IInvokeProvider, IInvokeProvider_Impl, IRawElementProviderFragment,
    IRawElementProviderFragmentRoot, IRawElementProviderFragmentRoot_Impl,
    IRawElementProviderFragment_Impl, IRawElementProviderSimple, IRawElementProviderSimple_Impl,
    ISelectionItemProvider, ISelectionItemProvider_Impl, ISelectionProvider,
    ISelectionProvider_Impl, NavigateDirection, NavigateDirection_FirstChild,
    NavigateDirection_LastChild, NavigateDirection_NextSibling, NavigateDirection_Parent,
    NavigateDirection_PreviousSibling, ProviderOptions, ProviderOptions_ServerSideProvider,
    StructureChangeType_ChildrenInvalidated, UIA_AutomationFocusChangedEventId,
    UIA_AutomationIdPropertyId, UIA_ButtonControlTypeId, UIA_ControlTypePropertyId,
    UIA_HasKeyboardFocusPropertyId, UIA_InvokePatternId, UIA_IsContentElementPropertyId,
    UIA_IsControlElementPropertyId, UIA_IsEnabledPropertyId, UIA_IsKeyboardFocusablePropertyId,
    UIA_NamePropertyId, UIA_RadioButtonControlTypeId, UIA_SelectionItemPatternId,
    UIA_SelectionPatternId, UiaAppendRuntimeId, UiaClientsAreListening, UiaDisconnectProvider,
    UiaHostProviderFromHwnd, UiaRaiseAutomationEvent, UiaRaiseStructureChangedEvent, UiaRect,
    UiaReturnRawElementProvider, UIA_CONTROLTYPE_ID, UIA_E_ELEMENTNOTAVAILABLE,
    UIA_E_INVALIDOPERATION, UIA_PATTERN_ID, UIA_PROPERTY_ID,
};

// ---------------------------------------------------------------------------------------
// The message-loop side: private messages, nesting depth, and the marshalling seam.
// ---------------------------------------------------------------------------------------

/// Run one closure on the overlay's UI thread: the shared marshaller's message (see
/// [`on_ui_thread`] and `crate::uia`).
pub(super) use crate::uia::WM_UIA_JOB;
/// Invoke one element, addressed by `(wparam = group tag, lparam = index)`.
///
/// POSTED, never sent. `IInvokeProvider::Invoke` is contractually asynchronous, and that is
/// exactly what makes it safe here: invoking Close or Copy destroys the window, which in turn
/// disconnects the providers, and doing that while an assistive technology's thread is blocked
/// inside a call into one of them is the deadlock the UIA documentation warns about. Posting
/// means the work always runs at the top of the message loop with no provider call in flight.
pub(super) const WM_UIA_INVOKE: u32 = WM_APP + 8;
/// Move keyboard focus to one element, addressed the same way as [`WM_UIA_INVOKE`]. Also
/// posted, for the same reason.
pub(super) const WM_UIA_FOCUS: u32 = WM_APP + 9;

/// The overlay whose providers are live, as a raw `HWND` value, or 0 for "none".
///
/// Read on assistive-technology threads and written on the UI thread, hence the atomic. It is
/// cleared FIRST on `WM_DESTROY`, so a call already on its way from another thread turns into
/// "element not available" instead of reaching a window that is being torn down.
static LIVE_OVERLAY: AtomicIsize = AtomicIsize::new(0);

thread_local! {
    /// How many `Shot`-borrowing message handlers are on this thread's stack.
    ///
    /// One is the normal case: the message loop dispatched a message and its handler is
    /// running. Two or more means a NESTED message pump, which on this window means a modal
    /// dialog (`ChooseColorW`, `ChooseFontW`, the Save dialog) opened by a handler that still
    /// holds `&mut Shot`. Servicing a UIA message there would hand out a second `&mut` to the
    /// same state, so the three UIA arms refuse while the depth is above one.
    ///
    /// The guard is entered by `shot_dispatch`, PAST its two borrow-free early returns, and
    /// not by `shot_wndproc`. That placement is load-bearing rather than incidental: UIA
    /// answers `UiaReturnRawElementProvider` and `UiaRaiseAutomationEvent` by calling straight
    /// back into the provider on the calling thread, and both of those happen at points where
    /// nothing holds `Shot`. Counting them as nesting would make a screen reader see empty
    /// names and rectangles for exactly the elements it had just been told about.
    static DISPATCH_DEPTH: Cell<u32> = const { Cell::new(0) };
}

/// Bumps [`DISPATCH_DEPTH`] for the life of one message handler.
pub(super) struct DispatchGuard;

impl DispatchGuard {
    pub(super) fn enter() -> Self {
        DISPATCH_DEPTH.with(|c| c.set(c.get().saturating_add(1)));
        DispatchGuard
    }
}

impl Drop for DispatchGuard {
    fn drop(&mut self) {
        DISPATCH_DEPTH.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Are we inside a nested message pump (see [`DISPATCH_DEPTH`])?
fn reentrant() -> bool {
    DISPATCH_DEPTH.with(Cell::get) > 1
}

/// Run `f` against the overlay's `Shot` **on the overlay's own thread**, and hand back what it
/// returned. `None` means the answer is not available: wrong or dead window, no state attached
/// yet, a nested modal pump, or the UI thread did not answer in time.
///
/// This is the whole of rule 3 in the module header. Every read a provider method performs goes
/// through here, so `Shot` is only ever touched from the message loop that owns it, and the
/// state is read at CALL time rather than cached in the provider object, which is the other
/// half of the same problem: a cached rect or name goes stale the moment a flyout opens, and a
/// cached pointer dangles the moment the capture closes.
fn on_ui_thread<T, F>(hwnd_raw: isize, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(HWND, &mut Shot) -> T + Send + 'static,
{
    if hwnd_raw == 0 || LIVE_OVERLAY.load(Ordering::Acquire) != hwnd_raw {
        return None;
    }
    // The shared marshaller does the cross-thread hop (heap payload, receiver-owned, bounded
    // wait); this wrapper adds the overlay's own two rules: only the live overlay answers, and
    // the closure gets the overlay's `Shot`, which [`run_job`] reaches only past the
    // re-entrancy guard.
    crate::uia::on_ui_thread(hwnd_of(hwnd_raw), move |hwnd| {
        // SAFETY: this runs only from `run_job` below, on the thread that owns the window, past
        // the re-entrancy guard, so nothing else holds a reference to this `Shot`.
        let state = unsafe { shot_ptr(hwnd) };
        (!state.is_null()).then(|| f(hwnd, unsafe { &mut *state }))
    })
    .flatten()
}

/// `WM_UIA_JOB`: run the marshalled closure. See [`on_ui_thread`].
pub(super) unsafe fn run_job(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    if reentrant() {
        // Declined, but still reclaimed: the payload is receiver-owned either way.
        crate::uia::discard_job(lparam);
        return LRESULT(0);
    }
    crate::uia::run_job(hwnd, lparam)
}

/// `WM_UIA_INVOKE`: run one element's action through layer 1's own invoke path.
pub(super) unsafe fn run_invoke(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if reentrant() {
        return LRESULT(0);
    }
    let Some(id) = decode_target(wparam, lparam) else {
        return LRESULT(0);
    };
    let p = shot_ptr(hwnd);
    if p.is_null() {
        return LRESULT(0);
    }
    unsafe { invoke_element(hwnd, &mut *p, id) };
    LRESULT(0)
}

/// `WM_UIA_FOCUS`: move layer 1's keyboard focus to one element.
pub(super) unsafe fn run_set_focus(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if reentrant() {
        return LRESULT(0);
    }
    let Some(id) = decode_target(wparam, lparam) else {
        return LRESULT(0);
    };
    let p = shot_ptr(hwnd);
    if p.is_null() {
        return LRESULT(0);
    }
    let s = unsafe { &mut *p };
    // Only a target that is on screen RIGHT NOW may take focus. An assistive technology can
    // hold an element from before a flyout closed, and layer 1's `repair_focus` is a
    // correction, not a licence to set focus to something that is no longer painted.
    if !unsafe { children(s) }.contains(&id) {
        return LRESULT(0);
    }
    s.focus = Some(id);
    let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    LRESULT(0)
}

/// Invoke `id` exactly as a click or a Space press on it would.
///
/// Focus is moved onto the element first so that `invoke_focus` (layer 1) acts on it: that
/// function is the shared seam, and it also carries the follow-through a keyboard user gets,
/// such as focus moving into a flyout that just opened.
unsafe fn invoke_element(hwnd: HWND, s: &mut Shot, id: FocusTarget) {
    if !unsafe { children(s) }.contains(&id) {
        return;
    }
    let Some(sel) = s.sel else { return };
    let dpi = unsafe { shot_dpi_for_sel(s, sel) };
    let buttons = unsafe { toolbar_layout_cached(s, sel, dpi) };
    s.focus = Some(id);
    let repaint = unsafe { invoke_focus(hwnd, s, &buttons, dpi) };
    // `invoke_focus` can have destroyed the window (Close, Copy and OCR all do), in which case
    // `s` is a freed box and `hwnd` is stale. Ask Windows rather than either of them.
    if unsafe { IsWindow(Some(hwnd)) }.as_bool() && repaint {
        let _ = unsafe { InvalidateRect(Some(hwnd), None, false) };
    }
}

/// `(group tag, index)` for a message payload. Tags are 1-based so that 0 can never be a valid
/// decode of a zeroed message.
fn encode_target(id: FocusTarget) -> (WPARAM, LPARAM) {
    let (tag, index) = match id {
        FocusTarget::Toolbar(i) => (1usize, i),
        FocusTarget::ColorFlyout(i) => (2usize, i),
        FocusTarget::TextFlyout(i) => (3usize, i),
    };
    (WPARAM(tag), LPARAM(index as isize))
}

fn decode_target(wparam: WPARAM, lparam: LPARAM) -> Option<FocusTarget> {
    let i = usize::try_from(lparam.0).ok()?;
    match wparam.0 {
        1 => Some(FocusTarget::Toolbar(i)),
        2 => Some(FocusTarget::ColorFlyout(i)),
        3 => Some(FocusTarget::TextFlyout(i)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------------------
// Provider lifetime.
// ---------------------------------------------------------------------------------------

/// The registry key of the fragment root. Element keys are `(tag << 32) | index`, and tags
/// start at 1, so 0 cannot collide with one.
const ROOT_KEY: u64 = 0;

/// Every provider object handed out for the current overlay, as raw AddRef'd interface
/// pointers keyed by element.
///
/// Raw addresses rather than a `Vec<IRawElementProviderFragment>` because windows-rs interface
/// wrappers are deliberately neither `Send` nor `Sync`, and this table is written from
/// assistive-technology threads and drained on the UI thread. The registry holds exactly one
/// reference per entry, taken back and released by [`disconnect_all`].
///
/// It doubles as a cache, which is not merely an optimisation: UIA identifies an element by
/// the pair (runtime id, object identity), so handing out a fresh object for every `Navigate`
/// would make the same button look like a new element on every traversal.
static HANDED_OUT: Mutex<Vec<(u64, usize)>> = Mutex::new(Vec::new());

fn key_of(id: FocusTarget) -> u64 {
    let (tag, index) = match id {
        FocusTarget::Toolbar(i) => (1u64, i),
        FocusTarget::ColorFlyout(i) => (2u64, i),
        FocusTarget::TextFlyout(i) => (3u64, i),
    };
    (tag << 32) | (index as u64 & 0xffff_ffff)
}

/// The provider object for `key` under the overlay `hwnd_raw`, created by `make` the first time
/// it is asked for.
///
/// Liveness is re-checked HERE, under the registry lock, not only by the caller's [`live`]: a
/// thread that passed `live` and was then descheduled while `WM_DESTROY` cleared the handle and
/// drained the table would otherwise take the lock afterwards and push an object nobody will
/// ever retire. `on_destroy` clears `LIVE_OVERLAY` before it takes this lock, so any push that
/// passes this check is drained by that same `disconnect_all`.
fn cached(
    hwnd_raw: isize,
    key: u64,
    make: impl FnOnce() -> IRawElementProviderFragment,
) -> Option<IRawElementProviderFragment> {
    let mut live = HANDED_OUT.lock().ok()?;
    if LIVE_OVERLAY.load(Ordering::Acquire) != hwnd_raw {
        return None;
    }
    if let Some((_, raw)) = live.iter().find(|(k, _)| *k == key) {
        let raw = *raw as *mut c_void;
        // SAFETY: the registry owns one reference to this pointer until `disconnect_all`
        // takes it back, so it is valid for as long as the entry is in the table.
        let borrowed = unsafe { IRawElementProviderFragment::from_raw_borrowed(&raw) }?;
        return Some(borrowed.clone());
    }
    let made = make();
    live.push((key, made.clone().into_raw() as usize));
    Some(made)
}

/// Is `hwnd` still the overlay whose tree we are allowed to build objects for? The cheap early
/// answer; [`cached`] repeats the check under the registry lock, which is what actually keeps
/// a fresh entry from slipping in behind `WM_DESTROY`'s disconnect.
fn live(hwnd: HWND) -> Option<isize> {
    let raw = hwnd.0 as isize;
    (raw != 0 && LIVE_OVERLAY.load(Ordering::Acquire) == raw).then_some(raw)
}

/// The fragment root for `hwnd`.
fn root_provider(hwnd: HWND) -> Option<IRawElementProviderFragment> {
    let raw = live(hwnd)?;
    cached(raw, ROOT_KEY, || ShotRoot { hwnd: raw }.into())
}

/// The element provider for `id` under `hwnd`.
fn element_provider(hwnd: HWND, id: FocusTarget) -> Option<IRawElementProviderFragment> {
    let raw = live(hwnd)?;
    cached(raw, key_of(id), || ShotElement { hwnd: raw, id }.into())
}

/// Tell UIA that every provider this overlay handed out is finished with.
///
/// **This is not optional, and it is not merely tidy-up.** UIA core caches provider pointers
/// on the client's behalf and there is no rule that says a client releases them promptly; a
/// screen reader can quite legitimately still be holding one when the user presses Esc. Every
/// one of those objects answers by reaching for `Shot` through this HWND, and `WM_DESTROY`
/// frees that `Shot` and its four GDI objects. Without this call the surviving objects are a
/// use-after-free waiting for the next question, in a process built with `panic = "abort"`,
/// from a thread we do not own. `UiaDisconnectProvider` makes UIA drop its cached pointers and
/// answer any further client call with `UIA_E_ELEMENTNOTAVAILABLE`, which is precisely the
/// thing a client is required to cope with.
///
/// The registry is drained into a local BEFORE the disconnect loop so the lock is not held
/// across a call that re-enters UIA core.
fn disconnect_all() {
    let taken = match HANDED_OUT.lock() {
        Ok(mut live) => core::mem::take(&mut *live),
        Err(_) => return,
    };
    for (_, raw) in taken {
        // SAFETY: takes back the one reference the registry was holding for this entry.
        let frag = unsafe { IRawElementProviderFragment::from_raw(raw as *mut c_void) };
        if let Ok(simple) = frag.cast::<IRawElementProviderSimple>() {
            let _ = unsafe { UiaDisconnectProvider(&simple) };
        }
        drop(frag);
    }
}

/// `WM_GETOBJECT` with `UiaRootObjectId`: hand UIA the fragment root.
///
/// Answered ahead of the wndproc's "no state attached yet" guard, because the provider is
/// addressed by HWND alone and every one of its reads copes with the state not being there.
pub(super) unsafe fn on_get_object(hwnd: HWND, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    LIVE_OVERLAY.store(hwnd.0 as isize, Ordering::Release);
    match root_provider(hwnd).and_then(|f| f.cast::<IRawElementProviderSimple>().ok()) {
        Some(p) => unsafe { UiaReturnRawElementProvider(hwnd, wparam, lparam, &p) },
        None => unsafe { DefWindowProcW(hwnd, WM_GETOBJECT, wparam, lparam) },
    }
}

/// `WM_DESTROY`: retire every provider before the `Shot` behind them is freed.
pub(super) unsafe fn on_destroy(hwnd: HWND) {
    let raw = hwnd.0 as isize;
    // Clear the live handle FIRST: from this moment a provider call on another thread returns
    // "not available" without so much as a message hop, which also keeps the disconnect below
    // from having to wait on one.
    let _ = LIVE_OVERLAY.compare_exchange(raw, 0, Ordering::AcqRel, Ordering::Acquire);
    disconnect_all();
}

// ---------------------------------------------------------------------------------------
// Events.
// ---------------------------------------------------------------------------------------

/// The part of `Shot` a screen reader has to be TOLD about rather than asked for.
#[derive(Clone, Copy, PartialEq)]
pub(super) struct Snapshot {
    focus: Option<FocusTarget>,
    color_flyout: bool,
    text_flyout: bool,
    font_dropdown: bool,
}

unsafe fn snapshot(hwnd: HWND) -> Option<Snapshot> {
    let p = shot_ptr(hwnd);
    if p.is_null() {
        return None;
    }
    let s = unsafe { &*p };
    Some(Snapshot {
        focus: s.focus,
        color_flyout: s.color_flyout,
        text_flyout: s.text_flyout,
        font_dropdown: s.font_dropdown,
    })
}

/// Before dispatching `msg`, note what a screen reader would care about, but only for the
/// messages that can actually change it and only while something is listening.
///
/// `UiaClientsAreListening` is the whole reason this costs nothing in the normal case: with no
/// assistive technology running it is a cheap "no" and not one further field is read. The
/// alternative, raising events unconditionally, puts a cross-process notification on the
/// critical path of every keystroke for the benefit of nobody.
pub(super) unsafe fn watch_before(hwnd: HWND, msg: u32) -> Option<Snapshot> {
    if !matches!(
        msg,
        WM_KEYDOWN | WM_LBUTTONDOWN | WM_LBUTTONUP | WM_UIA_INVOKE | WM_UIA_FOCUS
    ) {
        return None;
    }
    // Nothing is raised until UIA has actually asked this window for its tree. An event about
    // an element no client has ever seen is not merely wasted, it would have this side build
    // and register provider objects for a tree nobody is reading.
    if LIVE_OVERLAY.load(Ordering::Acquire) != hwnd.0 as isize {
        return None;
    }
    if !unsafe { UiaClientsAreListening() }.as_bool() {
        return None;
    }
    unsafe { snapshot(hwnd) }
}

/// After dispatching, raise whatever actually changed.
pub(super) unsafe fn watch_after(hwnd: HWND, before: Option<Snapshot>) {
    let Some(before) = before else { return };
    // The dispatch may have destroyed the window (Close, Copy, OCR, Esc), which also freed the
    // `Shot` the snapshot came from.
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return;
    }
    let Some(now) = (unsafe { snapshot(hwnd) }) else {
        return;
    };
    if now.focus != before.focus {
        unsafe { raise_focus_changed(hwnd, now.focus) };
    }
    let panels_before = (
        before.color_flyout,
        before.text_flyout,
        before.font_dropdown,
    );
    let panels_now = (now.color_flyout, now.text_flyout, now.font_dropdown);
    if panels_now != panels_before {
        unsafe { raise_structure_changed(hwnd) };
    }
}

unsafe fn raise_focus_changed(hwnd: HWND, focus: Option<FocusTarget>) {
    let provider = match focus {
        Some(id) => element_provider(hwnd, id),
        // Focus peeled off the chrome entirely (Esc): the window itself is the focused thing.
        None => root_provider(hwnd),
    };
    if let Some(simple) = provider.and_then(|f| f.cast::<IRawElementProviderSimple>().ok()) {
        let _ = unsafe { UiaRaiseAutomationEvent(&simple, UIA_AutomationFocusChangedEventId) };
    }
}

/// A flyout opened or closed, so the root's child list is a different list now.
///
/// `UiaRaiseStructureChangedEvent` rather than `UiaRaiseAutomationEvent` with the
/// structure-changed id: the structure event carries a change TYPE and a runtime id, and the
/// generic entry point has nowhere to put either, so a client would be told "something moved"
/// with no way to find out what. `ChildrenInvalidated` with a null runtime id is the documented
/// way to say "re-read my children", which is exactly true here.
unsafe fn raise_structure_changed(hwnd: HWND) {
    let Some(simple) = root_provider(hwnd).and_then(|f| f.cast::<IRawElementProviderSimple>().ok())
    else {
        return;
    };
    let _ = unsafe {
        UiaRaiseStructureChangedEvent(
            &simple,
            StructureChangeType_ChildrenInvalidated,
            core::ptr::null_mut(),
            0,
        )
    };
}

// ---------------------------------------------------------------------------------------
// Reading the editor: names, roles, rects. Everything here runs on the UI thread.
// ---------------------------------------------------------------------------------------

/// Everything a provider call can be asked about one element, gathered in a single hop.
struct ElementFacts {
    name: String,
    automation_id: String,
    control_type: UIA_CONTROLTYPE_ID,
    enabled: bool,
    focused: bool,
    /// `Some` only for the mutually exclusive tools, which are the one selection set here.
    selected: Option<bool>,
    /// Screen coordinates, already translated out of overlay-client space.
    rect: UiaRect,
}

/// Every element of the chrome, in reading order, addressed by layer 1's indices.
///
/// The toolbar first (skipping the separators, exactly as `toolbar::hit` and
/// `toolbar::step_focus` do), then whichever flyout is open. This is the ONE enumeration; the
/// child navigation, the focus lookup and the hit test all read it, so they cannot disagree
/// about what exists.
unsafe fn children(s: &mut Shot) -> Vec<FocusTarget> {
    let Some(sel) = s.sel else {
        return Vec::new();
    };
    // The OCR launch mode finishes on the drag and never shows a toolbar, so it has no chrome
    // to describe rather than an empty one.
    if s.ocr_mode {
        return Vec::new();
    }
    let dpi = unsafe { shot_dpi_for_sel(s, sel) };
    let buttons = unsafe { toolbar_layout_cached(s, sel, dpi) };
    let mut out = toolbar_elements(&buttons);
    if let Some(items) = color_flyout_items(s, &buttons, dpi) {
        out.extend((0..items.len()).map(FocusTarget::ColorFlyout));
    }
    if let Some(items) = text_flyout_items(s, &buttons, dpi) {
        out.extend((0..items.len()).map(FocusTarget::TextFlyout));
    }
    out
}

/// The focusable toolbar indices. Separators are painted dividers, never elements.
fn toolbar_elements(buttons: &[(Button, RECT)]) -> Vec<FocusTarget> {
    buttons
        .iter()
        .enumerate()
        .filter(|(_, (b, _))| !matches!(b, Button::Sep))
        .map(|(i, _)| FocusTarget::Toolbar(i))
        .collect()
}

/// Describe one element, or `None` when it is not currently on screen.
unsafe fn describe(s: &mut Shot, id: FocusTarget) -> Option<ElementFacts> {
    let sel = s.sel?;
    if s.ocr_mode {
        return None;
    }
    let dpi = unsafe { shot_dpi_for_sel(s, sel) };
    let buttons = unsafe { toolbar_layout_cached(s, sel, dpi) };
    let (vx, vy) = (s.vx, s.vy);
    let focused = s.focus == Some(id);
    let automation_id = automation_id(id);
    match id {
        FocusTarget::Toolbar(i) => {
            let (btn, r) = *buttons.get(i)?;
            if matches!(btn, Button::Sep) {
                return None;
            }
            Some(ElementFacts {
                name: button_name(btn),
                automation_id,
                control_type: button_control_type(btn),
                enabled: button_enabled(s, btn),
                focused,
                selected: match btn {
                    Button::Tool(t) => Some(s.tool == t),
                    _ => None,
                },
                rect: to_uia_rect(r, vx, vy),
            })
        }
        FocusTarget::ColorFlyout(i) => {
            let items = color_flyout_items(s, &buttons, dpi)?;
            let (swatch, r) = *items.get(i)?;
            Some(ElementFacts {
                name: swatch_name(swatch),
                automation_id,
                control_type: UIA_ButtonControlTypeId,
                enabled: true,
                focused,
                selected: None,
                rect: to_uia_rect(r, vx, vy),
            })
        }
        FocusTarget::TextFlyout(i) => {
            let items = text_flyout_items(s, &buttons, dpi)?;
            let (item, r) = *items.get(i)?;
            Some(ElementFacts {
                name: text_item_name(item, &tools::face_name(&s.text_font)),
                automation_id,
                control_type: UIA_ButtonControlTypeId,
                enabled: true,
                focused,
                selected: None,
                rect: to_uia_rect(r, vx, vy),
            })
        }
    }
}

/// Which element the point `(screen_x, screen_y)` is over.
///
/// Ordered the same way `on_lbuttondown_selected` orders its click routing, flyouts before the
/// bar, because a flyout is painted OVER the button that opened it: answering with the button
/// underneath would put a screen reader's cursor somewhere the mouse could never click.
unsafe fn hit_test(s: &mut Shot, screen_x: i32, screen_y: i32) -> Option<FocusTarget> {
    let sel = s.sel?;
    if s.ocr_mode {
        return None;
    }
    let p = POINT {
        x: screen_x.saturating_sub(s.vx),
        y: screen_y.saturating_sub(s.vy),
    };
    let dpi = unsafe { shot_dpi_for_sel(s, sel) };
    let buttons = unsafe { toolbar_layout_cached(s, sel, dpi) };
    if let Some(items) = color_flyout_items(s, &buttons, dpi) {
        if let Some(i) = items.iter().position(|(_, r)| pt_in(*r, p)) {
            return Some(FocusTarget::ColorFlyout(i));
        }
    }
    if let Some(items) = text_flyout_items(s, &buttons, dpi) {
        if let Some(i) = items.iter().position(|(_, r)| pt_in(*r, p)) {
            return Some(FocusTarget::TextFlyout(i));
        }
    }
    let btn = toolbar::hit(&buttons, p.x, p.y)?;
    button_index(&buttons, btn).map(FocusTarget::Toolbar)
}

/// Overlay-client geometry to the physical screen rectangle UIA reports.
///
/// The editor's rects are all client-relative because the overlay paints its backing bitmap at
/// `(0, 0)` whatever the virtual desktop's origin is; `vx`/`vy` are that origin. Getting this
/// wrong on a multi-monitor desktop is invisible in the app and completely breaks a screen
/// reader's highlight rectangle, which is drawn in screen space.
fn to_uia_rect(client: RECT, vx: i32, vy: i32) -> UiaRect {
    let r = client_rect_to_screen(client, vx, vy);
    UiaRect {
        left: f64::from(r.left),
        top: f64::from(r.top),
        width: f64::from(r.right.saturating_sub(r.left)),
        height: f64::from(r.bottom.saturating_sub(r.top)),
    }
}

// ---------------------------------------------------------------------------------------
// The COM objects.
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests;
