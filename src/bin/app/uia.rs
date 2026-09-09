//! Shared UI Automation scaffolding: a screen reader has no route into any owner-drawn
//! surface in this app unless something answers `WM_GETOBJECT` and describes what it drew.
//! `screenshot::overlay::uia` solved that once, for a single owner-drawn window with no child
//! controls of its own (audit F28) — that module builds a virtual fragment tree from scratch.
//! This module solves the OTHER shape: a set of items that already ARE real child windows (the
//! Settings nav rail's rows), where the right answer is not a second virtual tree but a thin
//! override on top of each control's own native accessible object.
//!
//! ## The technique
//!
//! Each item keeps its real `HWND` (a `WS_TABSTOP` `STATIC`, for the nav rail) and its own
//! place in the parent's window tree, so UI Automation already knows where it sits, when it has
//! focus, and what its bounding rectangle is — none of that needs reinventing, and keyboard
//! focus changes are announced for free (they ride the OS's own `WM_SETFOCUS` → UIA focus-event
//! translation, the same path any native control gets). What the OS does NOT know is that this
//! static draws a *name* of its own, a *role* other than "static text", and a *selected* state;
//! that is exactly the three properties + one pattern this module adds, by answering that ONE
//! control's own `WM_GETOBJECT` and returning a provider whose `HostRawElementProvider` is the
//! control's own native one (`UiaHostProviderFromHwnd`). UI Automation composes the two, so
//! everything not overridden here still comes from the real window.
//!
//! ## What a second surface must supply
//!
//! A caller wires this in per real, focusable, `WM_GETOBJECT`-reachable child window:
//!
//! 1. A `'static` [`ItemOps`]: `describe` (name, control type, selected/focused/enabled state —
//!    read fresh on every call, never cached: a cached name goes stale the moment the active
//!    page changes) and `select` (do whatever "this item is now current" means for the surface;
//!    runs on the owning UI thread already, so it may touch the surface's own state directly).
//! 2. A call to [`on_get_object`] from the item's own window procedure (or subclass, as the nav
//!    rail's `nav_item_subclass` does) on `WM_GETOBJECT`.
//! 3. A call to [`on_destroy`] from the item's `WM_NCDESTROY`, so a cached provider is
//!    disconnected before the window it describes goes away.
//! 4. A call to [`raise_selection_changed`] whenever the surface's own notion of "the current
//!    item" moves, so a screen reader speaks the change instead of only picking it up on the
//!    next Tab.
//! 5. A dispatch of [`WM_UIA_JOB`] to [`run_job`] from that same window procedure/subclass —
//!    the thread hop below cannot marshal onto a thread that never runs it.
//!
//! **This does NOT cover an owner-drawn surface with no child windows of its own** (a toolbar
//! painted as one control, buttons and all) — that shape needs a virtual Fragment/FragmentRoot
//! tree instead, because there is no real per-item window for `HostRawElementProvider` to
//! delegate to. Build that the way `screenshot::overlay::uia` did (a root fragment owning the
//! window's own `WM_GETOBJECT`, virtual child elements addressed by index); [`ItemFacts`] and
//! the thread-hop pattern below are still the right shapes to reuse for it, `ListItem` and
//! [`ItemOps`] are not — do not force a real per-item window model onto rows that were never
//! given one.
//!
//! ## The thread hop
//!
//! Same rule as `overlay::uia`'s rule 3, and for the same reason: a UI Automation client can be
//! out of process, and `uiautomationcore` calls provider methods from ITS OWN worker thread, not
//! from this window's message loop. `describe`/`select` read and mutate this app's real Win32
//! state (`GetDlgItem`, a `thread_local`, keyboard focus), which is only valid on the thread
//! that owns the window. [`on_ui_thread`] is the same `SendMessageTimeoutW` hop `overlay::uia`
//! uses, simplified because there is no borrowed `&mut State` to hand across: state here lives
//! in the surface's own `thread_local`, reachable from any closure once it is actually running
//! on the right thread, so the payload is just "run this closure over there and hand back what
//! it returned".
//!
//! ## Panic discipline
//!
//! Same as every other file at this COM boundary (see `overlay/uia.rs`'s header): nothing here
//! unwraps, indexes without a bounds check, or assumes a lookup succeeded. `catch_unwind` cannot
//! save a `panic = "abort"` release build (see `safety.rs`'s own caveat about that), so not
//! panicking in the first place is the only guard that is real once shipped.

use core::ffi::c_void;
use std::sync::{Arc, Mutex};

use windows::core::{implement, ComObjectInterface, Error, IUnknown, Interface, Result, HRESULT};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    IRawElementProviderSimple, IRawElementProviderSimple_Impl, ISelectionItemProvider,
    ISelectionItemProvider_Impl, ProviderOptions, ProviderOptions_ServerSideProvider,
    UIA_AutomationIdPropertyId, UIA_ControlTypePropertyId, UIA_HasKeyboardFocusPropertyId,
    UIA_IsContentElementPropertyId, UIA_IsControlElementPropertyId, UIA_IsEnabledPropertyId,
    UIA_IsKeyboardFocusablePropertyId, UIA_NamePropertyId, UIA_SelectionItemPatternId,
    UIA_SelectionItem_ElementSelectedEventId, UiaClientsAreListening, UiaDisconnectProvider,
    UiaHostProviderFromHwnd, UiaRaiseAutomationEvent, UiaReturnRawElementProvider, UiaRootObjectId,
    UIA_CONTROLTYPE_ID, UIA_E_INVALIDOPERATION, UIA_PATTERN_ID, UIA_PROPERTY_ID,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IsWindow, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_ERRORONEXIT, WM_APP,
};

/// Run one closure on an item's owning UI thread; see [`run_job`]. Picked as `WM_APP + 60` —
/// clear of every `WM_APP_*` constant `settings_dlg` already defines (+7..+11, +30, +40..+42;
/// see those modules) even though a collision could only matter if the SAME window received
/// both, which none currently do.
pub(crate) const WM_UIA_JOB: u32 = WM_APP + 60;

/// How long a provider call waits for the owning thread before giving up and reporting nothing.
/// A timeout rather than a plain blocking send for the same reason `overlay::uia` uses one: the
/// owning thread can be sitting in a modal dialog, and a screen reader that hangs until the user
/// closes a dialog it cannot describe is worse than one that briefly reports nothing.
const JOB_TIMEOUT_MS: u32 = 250;

fn hwnd_of(raw: isize) -> HWND {
    HWND(raw as *mut c_void)
}

/// One marshalled unit of work, type-erased so the window procedure can run it without knowing
/// what it returns. Heap-allocated and receiver-owned for the same reason `overlay::uia`'s `Job`
/// is: a timed-out `SendMessageTimeoutW` does not cancel delivery, so the sender cannot know
/// when it is safe to free the payload — the receiver drops it, on a delivered message, and a
/// timed-out one deliberately leaks a few dozen bytes rather than risk a use-after-free in a
/// process that aborts on panic.
trait Job: Send + Sync {
    fn run(&self, hwnd: HWND);
}

struct Call<T> {
    work: Mutex<Option<Work<T>>>,
    out: Mutex<Option<T>>,
}

type Work<T> = Box<dyn FnOnce(HWND) -> T + Send>;

impl<T: Send> Job for Call<T> {
    fn run(&self, hwnd: HWND) {
        let Ok(mut work) = self.work.lock() else {
            return;
        };
        let Some(f) = work.take() else {
            return; // already run: a duplicate delivery must not run the closure twice
        };
        drop(work);
        let value = f(hwnd);
        if let Ok(mut out) = self.out.lock() {
            *out = Some(value);
        }
    }
}

/// Run `f` on `hwnd`'s owning thread and hand back what it returned. `None` means the answer is
/// not available: a dead window, a nested modal pump the caller declined to service, or the
/// owning thread did not answer in time. `hwnd` may be any window that thread owns and that runs
/// [`run_job`] on `WM_UIA_JOB` — the nav rail sends this to the ITEM's own window, since it is
/// on the same thread as everything else in the dialog and always valid while shown.
pub(crate) fn on_ui_thread<T, F>(hwnd: HWND, f: F) -> Option<T>
where
    T: Send + 'static,
    F: FnOnce(HWND) -> T + Send + 'static,
{
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
    // Bound to a local rather than returned straight out of the tail expression: the guard has
    // to be dropped before `call` is, and locals drop in reverse declaration order.
    let mut answer = call.out.lock().ok()?;
    answer.take()
}

/// `WM_UIA_JOB`: run the marshalled closure. Call this from the window procedure/subclass of
/// any window a caller passed to [`on_ui_thread`].
pub(crate) unsafe fn run_job(hwnd: HWND, lparam: LPARAM) -> LRESULT {
    let ptr = lparam.0 as *mut Arc<dyn Job>;
    if ptr.is_null() {
        return LRESULT(0);
    }
    // SAFETY: the sender handed this reference over and never touches the allocation again.
    let job = unsafe { Box::from_raw(ptr) };
    job.run(hwnd);
    LRESULT(1)
}

/// Everything a provider call can be asked about one item.
pub(crate) struct ItemFacts {
    pub name: String,
    pub automation_id: String,
    pub control_type: UIA_CONTROLTYPE_ID,
    pub enabled: bool,
    pub focused: bool,
    pub selected: bool,
}

/// What a surface supplies for one real child window — see the module doc's "what a second
/// surface must supply".
pub(crate) struct ItemOps {
    /// Describe the item `hwnd` IS, right now. Runs on the owning thread (via [`on_ui_thread`]);
    /// `None` means the item is not currently describable (already torn down, or its owning
    /// state vanished between the message hop and the read).
    pub describe: fn(HWND) -> Option<ItemFacts>,
    /// Make `hwnd` the surface's current item — the same effect a click on it has. Runs on the
    /// owning thread; must not block.
    pub select: fn(HWND),
}

/// "There is no element here, and that is not an error." See `overlay::uia::no_element` for the
/// full reasoning (`Error::empty()` carries S_OK, which is the documented answer for "no
/// pattern/no host provider" — a real failure HRESULT would read as a broken provider instead).
fn no_element<T>() -> Result<T> {
    Err(Error::empty())
}

/// Every provider object handed out, keyed by the real item `HWND` it describes. Raw addresses
/// rather than a typed collection because windows-rs interface wrappers are deliberately neither
/// `Send` nor `Sync`, and this table is written from assistive-technology threads and drained
/// on each item's own thread. It also doubles as a cache: UI Automation identifies an element by
/// (runtime id, object identity), so handing out a fresh object on every `WM_GETOBJECT` would
/// make the same row look like a new element on every re-query.
static LIVE: Mutex<Vec<(isize, usize)>> = Mutex::new(Vec::new());

fn cached_provider(item_hwnd: isize, ops: &'static ItemOps) -> Option<IRawElementProviderSimple> {
    let mut live = LIVE.lock().ok()?;
    if let Some(&(_, raw)) = live.iter().find(|&&(h, _)| h == item_hwnd) {
        let raw = raw as *mut c_void;
        // SAFETY: the registry owns one reference to this pointer until `on_destroy` takes it
        // back, so it is valid for as long as the entry is in the table.
        let borrowed = unsafe { IRawElementProviderSimple::from_raw_borrowed(&raw) }?;
        return Some(borrowed.clone());
    }
    let made: IRawElementProviderSimple = ListItem {
        hwnd: item_hwnd,
        ops,
    }
    .into();
    live.push((item_hwnd, made.clone().into_raw() as usize));
    Some(made)
}

/// `WM_GETOBJECT`: hand UIA this item's provider, or `None` for anything that is not the UIA
/// root request (the caller then falls back to its normal default handling).
pub(crate) unsafe fn on_get_object(
    hwnd: HWND,
    wparam: WPARAM,
    lparam: LPARAM,
    ops: &'static ItemOps,
) -> Option<LRESULT> {
    if lparam.0 as i32 != UiaRootObjectId {
        return None;
    }
    let provider = cached_provider(hwnd.0 as isize, ops)?;
    Some(unsafe { UiaReturnRawElementProvider(hwnd, wparam, lparam, &provider) })
}

/// `WM_NCDESTROY`: disconnect this item's cached provider before the window behind it goes away.
/// See `overlay::uia::disconnect_all` for why this matters: UIA core can still be holding the
/// pointer, and every one of its methods reaches for this HWND, which the caller is about to
/// invalidate.
pub(crate) unsafe fn on_destroy(hwnd: HWND) {
    let raw = hwnd.0 as isize;
    let taken = match LIVE.lock() {
        Ok(mut live) => {
            let mut out = None;
            live.retain(|&(h, p)| {
                if h == raw {
                    out = Some(p);
                    false
                } else {
                    true
                }
            });
            out
        }
        Err(_) => None,
    };
    let Some(raw_ptr) = taken else { return };
    // SAFETY: takes back the one reference the registry was holding for this entry.
    let simple = unsafe { IRawElementProviderSimple::from_raw(raw_ptr as *mut c_void) };
    let _ = unsafe { UiaDisconnectProvider(&simple) };
}

/// Tell UIA that `item_hwnd` is now the selected one, so a screen reader speaks it instead of
/// only picking the change up on the next Tab. `UiaClientsAreListening` makes the normal case —
/// no assistive technology running — cheap: nothing is built or cached for the benefit of
/// nobody.
pub(crate) unsafe fn raise_selection_changed(item_hwnd: HWND, ops: &'static ItemOps) {
    if !unsafe { UiaClientsAreListening() }.as_bool() {
        return;
    }
    let Some(provider) = cached_provider(item_hwnd.0 as isize, ops) else {
        return;
    };
    let _ = unsafe { UiaRaiseAutomationEvent(&provider, UIA_SelectionItem_ElementSelectedEventId) };
}

/// One real child window, described on top of its own native provider. Holds only the HWND and
/// the surface's `ops` — never a borrowed reference into the surface's state, for the same
/// reason `overlay::uia::ShotElement` holds neither: state is read fresh, on the owning thread,
/// on every call, never cached in the provider object.
#[implement(IRawElementProviderSimple, ISelectionItemProvider)]
struct ListItem {
    hwnd: isize,
    ops: &'static ItemOps,
}

impl ListItem_Impl {
    fn facts(&self) -> Option<ItemFacts> {
        let ops = self.ops;
        on_ui_thread(hwnd_of(self.hwnd), move |h| (ops.describe)(h)).flatten()
    }
}

impl IRawElementProviderSimple_Impl for ListItem_Impl {
    fn ProviderOptions(&self) -> Result<ProviderOptions> {
        Ok(ProviderOptions_ServerSideProvider)
    }

    fn GetPatternProvider(&self, patternid: UIA_PATTERN_ID) -> Result<IUnknown> {
        const P_SELECTION_ITEM: i32 = UIA_SelectionItemPatternId.0;
        if patternid.0 == P_SELECTION_ITEM {
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
            // VT_EMPTY is "I do not answer this" — what a vanished item should say rather than
            // an error or a stale value.
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
        // The whole trick: everything this struct does NOT override (fragment navigation,
        // bounding rect, native focus tracking) comes from the real window for free.
        unsafe { UiaHostProviderFromHwnd(hwnd_of(self.hwnd)) }
    }
}

impl ISelectionItemProvider_Impl for ListItem_Impl {
    fn Select(&self) -> Result<()> {
        let ops = self.ops;
        let _: Option<()> = on_ui_thread(hwnd_of(self.hwnd), move |h| (ops.select)(h));
        Ok(())
    }

    fn AddToSelection(&self) -> Result<()> {
        // Single-selection container: adding to the selection is not a thing you can do.
        Err(Error::from(HRESULT(UIA_E_INVALIDOPERATION as i32)))
    }

    fn RemoveFromSelection(&self) -> Result<()> {
        Err(Error::from(HRESULT(UIA_E_INVALIDOPERATION as i32)))
    }

    fn IsSelected(&self) -> Result<windows::core::BOOL> {
        Ok(self.facts().is_some_and(|f| f.selected).into())
    }

    fn SelectionContainer(&self) -> Result<IRawElementProviderSimple> {
        // No container is implemented (there is no single real window that owns every item as
        // its child in the tree this scaffolding builds) — optional per the UIA spec, and
        // `IsSelected`/`Select` alone are what Narrator/NVDA need to announce a selection.
        no_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request for any object id OTHER than the UIA root must never even try to build a
    /// provider — passing a null `HWND` through would be unsound if it did.
    #[test]
    fn on_get_object_ignores_requests_that_are_not_the_uia_root() {
        static OPS: ItemOps = ItemOps {
            describe: |_| None,
            select: |_| {},
        };
        let hwnd = HWND(core::ptr::null_mut());
        let r = unsafe { on_get_object(hwnd, WPARAM(0), LPARAM(0), &OPS) };
        assert!(
            r.is_none(),
            "a non-root object id must be declined, not answered"
        );
    }
}
