//! Local countdown and expiry handling; the originating core alone decides execution.

use super::*;
use crate::bottom_pane::BottomPaneView;
use crate::bottom_pane::CancellationEvent;
use crate::bottom_pane::popup_consts::accept_cancel_hint_line;
use crate::keymap::ListAction;
use std::time::Duration;

impl ApprovalOverlay {
    fn expire_current_at(&mut self, now: Instant) -> bool {
        if self
            .current_deadline
            .is_some_and(|deadline| now >= deadline)
        {
            self.current_complete = true;
            self.advance_queue();
            return true;
        }
        false
    }
}

impl BottomPaneView for ApprovalOverlay {
    fn prefer_esc_to_handle_key_event(&self) -> bool {
        // Expiry can install the next queued request without completing this view.
        // Route Escape once, so it cannot then cancel that different request.
        true
    }

    fn keymap_contexts(&self) -> crate::keymap::KeymapContextSet {
        crate::keymap::KeymapContextSet::new(crate::keymap::KeymapContext::Approval)
            .with(crate::keymap::KeymapContext::List)
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        if self.expire_current_at(Instant::now()) {
            return;
        }
        if key_event.code == KeyCode::Esc {
            self.cancel_current_request();
            return;
        }
        if self.try_handle_shortcut(&key_event) {
            return;
        }
        self.list.handle_key_event(key_event);
        if let Some(idx) = self.list.take_last_selected_index() {
            self.apply_selection(idx);
        }
    }

    fn on_ctrl_c(&mut self) -> CancellationEvent {
        if !self.expire_current_at(Instant::now()) {
            self.cancel_current_request();
        }
        CancellationEvent::Handled
    }

    fn is_complete(&self) -> bool {
        self.done
    }

    fn try_consume_approval_request(
        &mut self,
        request: ApprovalRequest,
    ) -> Option<ApprovalRequest> {
        self.enqueue_request(request);
        None
    }

    fn dismiss_app_server_request(&mut self, request: &ResolvedAppServerRequest) -> bool {
        self.dismiss_resolved_request(request)
    }

    fn terminal_title_requires_action(&self) -> bool {
        !self.done
            && self
                .current_deadline
                .is_none_or(|deadline| Instant::now() < deadline)
    }

    fn pre_draw_tick(&mut self, now: Instant) -> bool {
        if self.expire_current_at(now) {
            return true;
        }
        let Some(request) = self.current_request.as_ref() else {
            return false;
        };
        self.list
            .set_footer_hint(Some(approval_footer_hint_with_remaining(
                request,
                &self.approval_keymap,
                &self.list_keymap,
                self.current_deadline
                    .map(|deadline| deadline.saturating_duration_since(now)),
            )));
        false
    }

    fn next_frame_delay(&self) -> Option<Duration> {
        let remaining = self
            .current_deadline?
            .saturating_duration_since(Instant::now());
        approval_expiration_next_frame_delay(remaining)
    }
}

pub(super) fn approval_footer_hint_with_remaining(
    request: &ApprovalRequest,
    approval_keymap: &ApprovalKeymap,
    list_keymap: &ListKeymap,
    remaining: Option<Duration>,
) -> Line<'static> {
    if remaining.is_some_and(|remaining| remaining.is_zero()) {
        return "Rejecting automatically".red().into();
    }
    let mut spans = accept_cancel_hint_line(
        list_keymap.primary_hint(ListAction::Accept),
        "to confirm",
        list_keymap.primary_hint(ListAction::Cancel),
        "to cancel",
    )
    .spans;
    if request.thread_label().is_some()
        && let Some(open_thread) =
            approval_keymap.primary_hint("open_thread", &approval_keymap.open_thread)
    {
        if !spans.is_empty() {
            spans.push(" or ".into());
        } else {
            spans.push("Press ".into());
        }
        spans.extend([open_thread.into(), " to open thread".into()]);
    }
    if let Some(remaining) = remaining {
        spans.push(" · ".dim());
        let countdown = format!(
            "Rejects automatically in {}",
            format_approval_expiration_remaining(remaining)
        );
        spans.push(countdown.red());
    }
    Line::from(spans)
}

pub(super) fn approval_request_timeout(request: &ApprovalRequest) -> Option<Duration> {
    let ApprovalRequest::Exec(request) = request else {
        return None;
    };
    let started_at_ms = request.started_at_ms;
    let expires_at_ms = request.expires_at_ms?;
    Some(Duration::from_millis(
        u64::try_from(expires_at_ms.saturating_sub(started_at_ms)).unwrap_or(0),
    ))
}

pub(super) fn saturating_instant_add(now: Instant, timeout: Duration) -> Instant {
    if let Some(deadline) = now.checked_add(timeout) {
        return deadline;
    }
    let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
    let mut lower = 0;
    let mut upper = timeout_ms;
    while lower < upper {
        let midpoint = lower + (upper - lower).div_ceil(2);
        if now.checked_add(Duration::from_millis(midpoint)).is_some() {
            lower = midpoint;
        } else {
            upper = midpoint - 1;
        }
    }
    now.checked_add(Duration::from_millis(lower)).unwrap_or(now)
}

fn approval_expiration_next_frame_delay(remaining: Duration) -> Option<Duration> {
    (!remaining.is_zero()).then_some(remaining.min(Duration::from_secs(/*secs*/ 1)))
}

fn format_approval_expiration_remaining(remaining: Duration) -> String {
    let mut seconds = remaining.as_secs();
    if remaining.subsec_nanos() > 0 {
        seconds = seconds.saturating_add(1);
    }
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    let seconds = seconds % 60;
    format!("{minutes}m {seconds:02}s")
}

#[cfg(test)]
#[path = "approval_countdown_tests.rs"]
mod tests;
