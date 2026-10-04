//! Read-only lifecycle observations for cross-module recap integration tests.

use super::RecapState;

#[derive(Debug, PartialEq, Eq)]
pub(in crate::app) struct PendingRecapWork {
    pub(in crate::app) scheduled_check: bool,
    pub(in crate::app) in_flight_request: bool,
}

pub(in crate::app) fn pending_work(state: &RecapState) -> PendingRecapWork {
    PendingRecapWork {
        scheduled_check: state.scheduled_check.is_some(),
        in_flight_request: state.in_flight_request.is_some(),
    }
}
