//! The hidden `--screenshot-automation` route: the real editor driven by an
//! integration test instead of a person.
//!
//! It exists so the overlay can be exercised end to end without a human at the mouse,
//! and it publishes its progress through the WINDOW TITLE, which is the one channel a
//! test harness can read from another process without hooking anything.

use super::*;

#[derive(Clone, Copy)]
pub(super) struct AutomationDrag {
    pub(super) tool: Tool,
    pub(super) anchor: POINT,
    pub(super) raw: POINT,
    pub(super) final_point: POINT,
    pub(super) snapped: bool,
}

pub(super) struct AutomationState {
    pub(super) forced_shift: bool,
    pub(super) commit_gen: u64,
    pub(super) painted_gen: u64,
    pub(super) last_drag: Option<AutomationDrag>,
    pub(super) status: &'static str,
    pub(super) published_title: String,
}

pub(super) const AUTOMATION_TITLE_PREFIX: &str = "SageThumbs 2K Screenshot Automation";

pub(super) fn automation_title(state: &AutomationState) -> String {
    let base = format!(
        "{AUTOMATION_TITLE_PREFIX} | snap={} | commit={} | painted={} | status={}",
        u8::from(state.forced_shift),
        state.commit_gen,
        state.painted_gen,
        state.status,
    );
    if let Some(drag) = state.last_drag {
        format!(
            "{base} | tool={} | anchor={},{} | raw={},{} | final={},{} | shifted={}",
            drag.tool.label(),
            drag.anchor.x,
            drag.anchor.y,
            drag.raw.x,
            drag.raw.y,
            drag.final_point.x,
            drag.final_point.y,
            u8::from(drag.snapped),
        )
    } else {
        base
    }
}

pub(super) fn block_automation_output(s: &mut Shot, status: &'static str) -> bool {
    if let Some(state) = s.automation.as_mut() {
        state.status = status;
        true
    } else {
        false
    }
}

pub(super) unsafe fn publish_automation_title(hwnd: HWND, s: &mut Shot) {
    let Some(state) = s.automation.as_mut() else {
        return;
    };
    // This runs only after EndPaint. Matching commit==painted in the externally
    // visible title therefore proves the real committed-shape paint path completed.
    state.painted_gen = state.commit_gen;
    let title = automation_title(state);
    if title != state.published_title {
        let title_w = wide(&title);
        let _ = SetWindowTextW(hwnd, PCWSTR(title_w.as_ptr()));
        state.published_title = title;
    }
}
