use super::*;
use pretty_assertions::assert_eq;

#[test]
fn idle_resume_retains_status_without_creating_a_terminal_turn() {
    let status = AgentStatus::Completed(None);
    let result = InitialTerminalObservation::ReconcileOrObserveNextFrom(AgentStatus::PendingInit)
        .reconcile(
            /*active_turn_id*/ None,
            /*last_terminal*/ None,
            status.clone(),
        );
    assert_eq!((result.terminal, result.status), (None, status));
}

#[test]
fn resume_reconciles_a_genuine_empty_final_response() {
    let status = AgentStatus::Completed(None);
    let terminal = Some(("finished-turn".to_string(), status.clone()));
    let result = InitialTerminalObservation::ReconcileOrObserveNextFrom(AgentStatus::PendingInit)
        .reconcile(
            /*active_turn_id*/ None,
            terminal.clone(),
            status.clone(),
        );
    assert_eq!((result.terminal, result.status), (terminal, status));
}

#[test]
fn resume_reconciles_an_active_turn_terminal_status() {
    let status = AgentStatus::Completed(None);
    let result = InitialTerminalObservation::ReconcileIfAdvancedFrom(AgentStatus::Running)
        .reconcile(
            Some("active-turn".to_string()),
            /*last_terminal*/ None,
            status.clone(),
        );
    assert_eq!(
        (result.terminal, result.status),
        (Some(("active-turn".to_string(), status.clone())), status)
    );
}
