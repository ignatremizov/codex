//! Deterministic publication gates shared with thread-manager integration fixtures.

use super::LocalAgentControl;
use super::SessionPresentationId;
use std::sync::PoisonError;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

pub(crate) fn pause_refresh_after_capture(
    control: &LocalAgentControl,
    captured: oneshot::Sender<()>,
    proceed: oneshot::Receiver<()>,
) {
    *control
        .wait_agent_presentations
        .messaging_refresh_capture_gate
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some((captured, proceed));
}

pub(crate) fn observe_refresh_attempts(
    control: &LocalAgentControl,
    observer: Option<mpsc::UnboundedSender<Vec<SessionPresentationId>>>,
) {
    *control
        .wait_agent_presentations
        .messaging_refresh_attempted
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = observer;
}
