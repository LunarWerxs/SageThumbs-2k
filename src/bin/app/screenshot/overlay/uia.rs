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

use core::ffi::c_void;
use core::sync::atomic::{AtomicIsize, Ordering};
use std::cell::Cell;
use std::sync::{Arc, Mutex};

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

/// Run one closure on the overlay's UI thread. `lparam` carries a pointer to the closure;
/// see [`on_ui_thread`] for why the pointer stays valid for the whole call.
pub(super) const WM_UIA_JOB: u32 = WM_APP + 7;
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

/// How long a provider call waits for the UI thread before giving up and reporting nothing.
///
/// A timeout rather than a plain `SendMessageW` because the overlay's thread can be sitting in
/// a modal Font or Colour dialog, and a screen reader that hangs until the user closes a
/// dialog it cannot describe is worse than one that briefly reports an element as unavailable.
const JOB_TIMEOUT_MS: u32 = 250;

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

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// One marshalled unit of work, type-erased so the window procedure can run it without knowing
/// what it returns.
///
/// The obvious implementation puts the closure on the CALLER's stack and passes a pointer to
/// it, and it is wrong in a way that would only ever show up under load: a timed-out
/// `SendMessageTimeoutW` does not cancel anything. Windows leaves the message in the target
/// thread's sent-message queue and delivers it the next time that thread pumps, which by then
/// is after the caller's frame has gone, so the receiver would call a closure that no longer
/// exists. The payload is therefore heap allocated, and the RECEIVER owns it: on a delivered
/// message it has already been dropped, and on a timed-out one the allocation is deliberately
/// abandoned rather than freed by a sender that cannot know whether the other thread is about
/// to touch it. Leaking a few dozen bytes on a stall we do not expect to see is the cheap side
/// of that trade; the other side is a use-after-free in a process that aborts on panic.
trait Job: Send + Sync {
    /// Runs on the overlay's own thread. `state` is null when no `Shot` is attached yet.
    fn run(&self, hwnd: HWND, state: *mut Shot);
}

/// A [`Job`] that carries a closure in and its answer back out. Both halves are behind their
/// own lock because a late delivery (see above) can touch this after the caller has walked
/// away with `None`.
struct Call<T> {
    work: Mutex<Option<Work<T>>>,
    out: Mutex<Option<T>>,
}

/// The boxed closure a [`Call`] carries across to the UI thread.
type Work<T> = Box<dyn FnOnce(HWND, &mut Shot) -> T + Send>;

impl<T: Send> Job for Call<T> {
    fn run(&self, hwnd: HWND, state: *mut Shot) {
        if state.is_null() {
            return;
        }
        let Ok(mut work) = self.work.lock() else {
            return;
        };
        let Some(f) = work.take() else {
            return; // already run: a duplicate delivery must not run the closure twice
        };
        drop(work);
        // SAFETY: this runs only from the `WM_UIA_JOB` arm, on the thread that owns the window,
        // past the re-entrancy guard, so nothing else holds a reference to this `Shot`.
        let value = f(hwnd, unsafe { &mut *state });
        if let Ok(mut out) = self.out.lock() {
            *out = Some(value);
        }
    }
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
    let hwnd = hwnd_of(hwnd_raw);
    if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
        return None;
    }
    let call = Arc::new(Call {
        work: Mutex::new(Some(Box::new(f) as Work<T>)),
        out: Mutex::new(None),
    });
    // The receiver's own reference, handed across as a thin pointer to the (fat) trait object.
    let handoff: *mut Arc<dyn Job> = Box::into_raw(Box::new(Arc::clone(&call) as Arc<dyn Job>));
    unsafe {
        SendMessageTimeoutW(
            hwnd,
            WM_UIA_JOB,
            WPARAM(0),
            LPARAM(handoff as isize),
            SMTO_ABORTIFHUNG | SMTO_ERRORONEXIT,
            JOB_TIMEOUT_MS,
            None,
        );
    }
    // Bound to a local rather than returned straight out of the tail expression: the guard
    // has to be dropped before `call` is, and locals drop in reverse declaration order.
    let mut answer = call.out.lock().ok()?;
    answer.take()
}

/// `WM_UIA_JOB`: run the marshalled closure. See [`on_ui_thread`].
pub(super) unsafe fn run_job(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let ptr = lparam.0 as *mut Arc<dyn Job>;
    if ptr.is_null() {
        return LRESULT(0);
    }
    // SAFETY: the sender handed this reference over and never touches the allocation again.
    // Taken back BEFORE the re-entrancy check so that a declined job still frees it.
    let job = unsafe { Box::from_raw(ptr) };
    if reentrant() {
        return LRESULT(0);
    }
    job.run(hwnd, unsafe { shot_ptr(hwnd) });
    LRESULT(1)
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

/// The provider object for `key`, created by `make` the first time it is asked for.
fn cached(
    key: u64,
    make: impl FnOnce() -> IRawElementProviderFragment,
) -> Option<IRawElementProviderFragment> {
    let mut live = HANDED_OUT.lock().ok()?;
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

/// Is `hwnd` still the overlay whose tree we are allowed to build objects for?
///
/// Checked before every creation so that a call which was waiting on the registry lock while
/// `WM_DESTROY` drained it cannot slip a fresh entry in behind the disconnect and leave one
/// object that nobody will ever retire.
fn live(hwnd: HWND) -> Option<isize> {
    let raw = hwnd.0 as isize;
    (raw != 0 && LIVE_OVERLAY.load(Ordering::Acquire) == raw).then_some(raw)
}

/// The fragment root for `hwnd`.
fn root_provider(hwnd: HWND) -> Option<IRawElementProviderFragment> {
    let raw = live(hwnd)?;
    cached(ROOT_KEY, || ShotRoot { hwnd: raw }.into())
}

/// The element provider for `id` under `hwnd`.
fn element_provider(hwnd: HWND, id: FocusTarget) -> Option<IRawElementProviderFragment> {
    let raw = live(hwnd)?;
    cached(key_of(id), || ShotElement { hwnd: raw, id }.into())
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

/// A stable, non-localised handle for an element, for test harnesses and for a client that
/// wants to remember "the Undo button" across a re-read of the tree.
fn automation_id(id: FocusTarget) -> String {
    match id {
        FocusTarget::Toolbar(i) => format!("toolbar.{i}"),
        FocusTarget::ColorFlyout(i) => format!("colour.{i}"),
        FocusTarget::TextFlyout(i) => format!("text.{i}"),
    }
}

/// `[UiaAppendRuntimeId, group tag, index]`, the shape UIA expects from a fragment whose root
/// is a window: the first element tells UIA to prefix the host window's own id, so the rest
/// only has to be unique WITHIN this overlay.
fn runtime_id_parts(id: FocusTarget) -> [i32; 3] {
    let (tag, index) = match id {
        FocusTarget::Toolbar(i) => (1i32, i),
        FocusTarget::ColorFlyout(i) => (2i32, i),
        FocusTarget::TextFlyout(i) => (3i32, i),
    };
    [
        UiaAppendRuntimeId as i32,
        tag,
        i32::try_from(index).unwrap_or(i32::MAX),
    ]
}

/// The tools are one mutually exclusive set, so they read as radio buttons; everything else on
/// the bar performs an action and reads as a button.
fn button_control_type(btn: Button) -> UIA_CONTROLTYPE_ID {
    match btn {
        Button::Tool(_) => UIA_RadioButtonControlTypeId,
        _ => UIA_ButtonControlTypeId,
    }
}

/// Undo and Redo genuinely do nothing with an empty stack (see `actions::handle_button`), and
/// saying so is the point of the property. The bar does not grey them out, so this is the only
/// place that difference is visible, which is a small honesty gain for a screen reader user
/// rather than a change to what the bar does.
fn button_enabled(s: &Shot, btn: Button) -> bool {
    match btn {
        Button::Undo => !s.shapes.is_empty(),
        Button::Redo => !s.redo.is_empty(),
        _ => true,
    }
}

/// The name a screen reader should speak for a toolbar item.
fn button_name(btn: Button) -> String {
    spoken_name(&toolbar::button_tip(btn)).to_string()
}

/// Reduce a toolbar tooltip to a spoken name.
///
/// The tips are written for a mouse user hovering a bare icon, so each one is three things
/// glued together: a name, a keyboard hint, and a sentence about how to use the tool. Read
/// aloud on every arrow press that is unbearable, so keep the head and drop the rest. The
/// shortcut is not being hidden from anyone, it is still on the tooltip and still in the
/// Settings help; it simply does not belong in the element's NAME, which is what a reader
/// repeats every single time focus lands.
fn spoken_name(tip: &str) -> &str {
    // U+2014 EM DASH, written as an escape rather than the character itself.
    let head = tip.split('\u{2014}').next().unwrap_or(tip).trim();
    strip_key_hint(head)
}

/// Drop a trailing parenthetical, but only when it is a keyboard hint.
///
/// "Copy text (OCR) (Ctrl+T)" has two parentheticals and only the last one is a hint, so this
/// tests the content rather than assuming the last group is always droppable.
fn strip_key_hint(s: &str) -> &str {
    let t = s.trim_end();
    let Some(rest) = t.strip_suffix(')') else {
        return t;
    };
    let Some(open) = rest.rfind('(') else {
        return t;
    };
    let inner = &rest[open + 1..];
    if looks_like_key_hint(inner) {
        rest[..open].trim_end()
    } else {
        t
    }
}

fn looks_like_key_hint(inner: &str) -> bool {
    if inner.chars().count() == 1 {
        return true; // the single-letter tool shortcuts: (R), (O), (A), ...
    }
    [
        "Ctrl", "Alt", "Shift", "Esc", "Enter", "Del", "Tab", "Space",
    ]
    .iter()
    .any(|w| inner.contains(w))
}

/// The nearest basic colour name for `c`.
///
/// A swatch named "#E62828" is technically complete and useless out loud: six digits spelled
/// one at a time, with nothing to tell the red one from the green one. The palette carries no
/// names of its own (it is six RGB triples), so the name is DERIVED, which also means a custom
/// colour the user picked gets a sensible name for free.
fn color_name(c: COLORREF) -> &'static str {
    const NAMED: [(&str, i32, i32, i32); 13] = [
        ("Black", 0, 0, 0),
        ("White", 255, 255, 255),
        ("Grey", 128, 128, 128),
        ("Red", 220, 30, 30),
        ("Orange", 245, 130, 30),
        ("Yellow", 240, 220, 40),
        ("Green", 40, 170, 60),
        ("Cyan", 40, 200, 220),
        ("Blue", 40, 100, 220),
        ("Purple", 130, 60, 200),
        ("Magenta", 220, 60, 180),
        ("Brown", 130, 80, 40),
        ("Pink", 245, 150, 180),
    ];
    let r = (c.0 & 0xff) as i32;
    let g = ((c.0 >> 8) & 0xff) as i32;
    let b = ((c.0 >> 16) & 0xff) as i32;
    let mut best = NAMED[0].0;
    let mut best_d = i32::MAX;
    for (name, nr, ng, nb) in NAMED {
        let d = (r - nr) * (r - nr) + (g - ng) * (g - ng) + (b - nb) * (b - nb);
        if d < best_d {
            best_d = d;
            best = name;
        }
    }
    best
}

fn swatch_name(swatch: Swatch) -> String {
    match swatch {
        Swatch::Color(c) => color_name(c).to_string(),
        Swatch::Custom(Some(c)) => format!("Custom colour, {}", color_name(c)),
        Swatch::Custom(None) => "Empty custom colour slot".to_string(),
        Swatch::Picker => "More colours".to_string(),
    }
}

/// `face` is the font currently in force, so the field announces what it is set TO rather than
/// just what it is.
///
/// Localized (audit F29, 2026-09-06): pre-fix these were a SECOND, independent set of hardcoded
/// English strings that happened to describe the same controls `textflyout`'s own paint code
/// already localizes - a screen reader user got English regardless of the active language even
/// after the visible captions were fixed. `Bold`/`Underline` now go through the exact same
/// locale keys `checkbox_label` paints with, so the two can never drift apart again; the
/// remaining three have no on-screen caption of their own ("−"/"+" are language-neutral, and the
/// font dropdown toggle has no separate label), so they get their own keys.
fn text_item_name(item: TextItem, face: &str) -> String {
    match item {
        TextItem::FontField => crate::win::t("shot_text_font_field").replace("{face}", face),
        TextItem::FontOption(i) => toolbar::PRESET_FONTS
            .get(i)
            .map_or_else(|| "Font".to_string(), |n| (*n).to_string()),
        TextItem::SizeDown => crate::win::t("shot_text_size_down").to_string(),
        TextItem::SizeUp => crate::win::t("shot_text_size_up").to_string(),
        TextItem::Bold => crate::win::t("shot_text_bold").to_string(),
        TextItem::Underline => crate::win::t("shot_text_underline").to_string(),
        TextItem::More => crate::win::t("shot_text_more_options").to_string(),
    }
}

// ---------------------------------------------------------------------------------------
// The COM objects.
// ---------------------------------------------------------------------------------------

/// "There is no element here, and that is not an error."
///
/// UIA's contract for `Navigate`, `GetPatternProvider` and `HostRawElementProvider` is S_OK
/// with a NULL out pointer, but the generated trait signature is `Result<Interface>` and every
/// windows-rs interface wrapper is a NON-NULL pointer internally. `Ok(null)` is therefore not
/// merely unidiomatic, it is an invalid value whose niche the `Ok`/`Err` layout is free to
/// reuse. `Error::empty()` carries the code S_OK, so the generated vtable shim returns S_OK and
/// leaves the caller's out pointer at the NULL it initialised. Returning a real failure
/// HRESULT was rejected: a client would read "you are the last sibling" as a broken provider.
fn no_element<T>() -> Result<T> {
    Err(Error::empty())
}

fn i4_array(values: &[i32]) -> Result<*mut SAFEARRAY> {
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
struct ShotRoot {
    hwnd: isize,
}

impl ShotRoot_Impl {
    fn children(&self) -> Vec<FocusTarget> {
        on_ui_thread(self.hwnd, |_, s| unsafe { children(s) }).unwrap_or_default()
    }

    fn child(&self, id: FocusTarget) -> Result<IRawElementProviderFragment> {
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
unsafe fn selected_tool(s: &mut Shot) -> Option<FocusTarget> {
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
struct ShotElement {
    hwnd: isize,
    id: FocusTarget,
}

impl ShotElement_Impl {
    fn facts(&self) -> Option<ElementFacts> {
        let id = self.id;
        on_ui_thread(self.hwnd, move |_, s| unsafe { describe(s, id) }).flatten()
    }

    /// Ask the UI thread to do something to this element. Posted, never sent: see
    /// [`WM_UIA_INVOKE`].
    fn post(&self, msg: u32) -> Result<()> {
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

    fn sibling(&self, forward: bool) -> Result<IRawElementProviderFragment> {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A toolbar laid out the way a real capture would lay it out, so the index-to-element
    /// mapping is tested against the same vector the mouse hit-tests.
    fn laid_out() -> Vec<(Button, RECT)> {
        toolbar::layout(
            RECT {
                left: 200,
                top: 200,
                right: 900,
                bottom: 700,
            },
            1920,
            1080,
            96,
        )
    }

    #[test]
    fn tooltips_reduce_to_a_spoken_name_without_the_key_hint() {
        assert_eq!(
            spoken_name("Rectangle (R) \u{2014} drag to draw"),
            "Rectangle"
        );
        assert_eq!(
            spoken_name("Colour (K) \u{2014} cycle the palette"),
            "Colour"
        );
        assert_eq!(spoken_name("Undo (Ctrl+Z)"), "Undo");
        assert_eq!(
            spoken_name("Copy to the clipboard (Ctrl+C / Enter)"),
            "Copy to the clipboard"
        );
        assert_eq!(spoken_name("Close (Esc)"), "Close");
        assert_eq!(
            spoken_name("Pick colour (E) \u{2014} click a pixel to copy its hex"),
            "Pick colour"
        );
    }

    /// The one tooltip with TWO parentheticals: only the trailing key hint may go, because
    /// "(OCR)" is part of what the button is called.
    #[test]
    fn a_parenthetical_that_is_not_a_key_hint_survives() {
        assert_eq!(
            spoken_name("Copy text (OCR) (Ctrl+T) \u{2014} read the words in the region"),
            "Copy text (OCR)"
        );
        assert!(!looks_like_key_hint("OCR"));
        assert!(looks_like_key_hint("R"));
        assert!(looks_like_key_hint("Ctrl+Shift+Z"));
    }

    /// Every button the mouse can click has to end up with something to say. An empty name is
    /// how an element becomes "button" and nothing else in a screen reader's list.
    #[test]
    fn every_focusable_toolbar_item_has_a_name_and_a_role() {
        for (btn, _) in laid_out() {
            if matches!(btn, Button::Sep) {
                continue;
            }
            assert!(
                !button_name(btn).is_empty(),
                "a focusable toolbar item must have a spoken name"
            );
            let is_tool = matches!(btn, Button::Tool(_));
            assert_eq!(
                button_control_type(btn) == UIA_RadioButtonControlTypeId,
                is_tool,
                "only the mutually exclusive tools may report as radio buttons"
            );
        }
    }

    /// The element list is layer 1's index space, separators excluded, and it must agree with
    /// `toolbar::hit` about which indices are real.
    #[test]
    fn element_indices_skip_separators_and_match_the_mouse_hit_test() {
        let buttons = laid_out();
        let ids = toolbar_elements(&buttons);
        assert_eq!(
            ids.len(),
            buttons
                .iter()
                .filter(|(b, _)| !matches!(b, Button::Sep))
                .count()
        );
        for id in &ids {
            let FocusTarget::Toolbar(i) = *id else {
                panic!("toolbar_elements must only produce toolbar targets");
            };
            let (btn, r) = buttons[i];
            assert!(!matches!(btn, Button::Sep));
            // The centre of the element's own rect must hit the element's own button.
            let cx = (r.left + r.right) / 2;
            let cy = (r.top + r.bottom) / 2;
            // `Button` carries no Debug impl (it is an icon id, not a value anyone
            // prints), so this is an assert rather than an assert_eq.
            assert!(
                toolbar::hit(&buttons, cx, cy) == Some(btn),
                "the centre of an element's rect must hit that element's own button"
            );
        }
    }

    /// A 150%-scaled display sitting left of and above the primary: the same case the layer-1
    /// DPI regression covers, but for the rect a screen reader draws its highlight with. Client
    /// space starts at (0, 0) whatever the desktop origin is, so an untranslated rect would put
    /// the highlight on the wrong monitor entirely.
    #[test]
    fn bounding_rectangles_are_reported_in_screen_coordinates() {
        let client = RECT {
            left: 320,
            top: 240,
            right: 420,
            bottom: 268,
        };
        let r = to_uia_rect(client, -2560, -120);
        assert_eq!(r.left, -2240.0);
        assert_eq!(r.top, 120.0);
        assert_eq!(r.width, 100.0);
        assert_eq!(r.height, 28.0);
        // An identity origin must leave client geometry untouched.
        let same = to_uia_rect(client, 0, 0);
        assert_eq!(same.left, 320.0);
        assert_eq!(same.top, 240.0);
    }

    #[test]
    fn palette_colours_get_a_spoken_name_rather_than_six_hex_digits() {
        let names: Vec<&str> = PALETTE
            .iter()
            .map(|&(r, g, b)| color_name(rgb(r, g, b)))
            .collect();
        assert_eq!(
            names,
            vec!["Red", "Green", "Blue", "Yellow", "Black", "White"]
        );
        assert_eq!(
            swatch_name(Swatch::Custom(None)),
            "Empty custom colour slot"
        );
        assert_eq!(swatch_name(Swatch::Picker), "More colours");
        assert_eq!(
            swatch_name(Swatch::Custom(Some(rgb(250, 250, 250)))),
            "Custom colour, White"
        );
    }

    /// The font field announces what the font IS, not just that it is the font field, and every
    /// preset row names its own face.
    #[test]
    fn text_flyout_rows_name_themselves() {
        assert_eq!(
            text_item_name(TextItem::FontField, "Segoe UI"),
            "Font, Segoe UI"
        );
        for (i, face) in toolbar::PRESET_FONTS.iter().enumerate() {
            assert_eq!(text_item_name(TextItem::FontOption(i), "Segoe UI"), *face);
        }
        // An index past the end must NOT panic: these come off a layout that lengthens and
        // shortens with the dropdown, and this binary aborts on panic.
        assert_eq!(
            text_item_name(TextItem::FontOption(usize::MAX), "Segoe UI"),
            "Font"
        );
        assert_eq!(text_item_name(TextItem::Bold, "Segoe UI"), "Bold");
    }

    /// Runtime ids and automation ids must separate the three groups, or a swatch and a toolbar
    /// button with the same index would look like the same element to a client.
    #[test]
    fn element_ids_are_unique_across_the_three_groups() {
        let ids = [
            FocusTarget::Toolbar(3),
            FocusTarget::ColorFlyout(3),
            FocusTarget::TextFlyout(3),
        ];
        let runtime: Vec<[i32; 3]> = ids.iter().map(|id| runtime_id_parts(*id)).collect();
        assert_ne!(runtime[0], runtime[1]);
        assert_ne!(runtime[1], runtime[2]);
        assert_ne!(runtime[0], runtime[2]);
        for parts in &runtime {
            assert_eq!(parts[0], UiaAppendRuntimeId as i32);
            assert_eq!(parts[2], 3);
        }
        let keys: Vec<u64> = ids.iter().map(|id| key_of(*id)).collect();
        assert_ne!(keys[0], keys[1]);
        assert_ne!(keys[1], keys[2]);
        assert_ne!(keys[0], keys[2]);
        assert!(!keys.contains(&ROOT_KEY));
        assert_eq!(automation_id(FocusTarget::Toolbar(3)), "toolbar.3");
        assert_eq!(automation_id(FocusTarget::ColorFlyout(3)), "colour.3");
        assert_eq!(automation_id(FocusTarget::TextFlyout(3)), "text.3");
    }

    /// The message payload is the only place an element id is flattened into two integers, so
    /// it has to round-trip for every group, and a payload that was never encoded (a zeroed
    /// message, or a stray WM_APP from another window) must decode to nothing at all.
    #[test]
    fn element_ids_round_trip_through_the_message_payload() {
        for id in [
            FocusTarget::Toolbar(0),
            FocusTarget::Toolbar(23),
            FocusTarget::ColorFlyout(10),
            FocusTarget::TextFlyout(7),
        ] {
            let (w, l) = encode_target(id);
            assert_eq!(decode_target(w, l), Some(id));
        }
        assert_eq!(decode_target(WPARAM(0), LPARAM(0)), None);
        assert_eq!(decode_target(WPARAM(9), LPARAM(1)), None);
        assert_eq!(decode_target(WPARAM(1), LPARAM(-1)), None);
    }
}
