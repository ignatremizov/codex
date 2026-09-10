use super::*;
use pretty_assertions::assert_eq;
use std::collections::HashSet;

#[test]
fn startup_reconciliation_only_suppresses_exact_delivered_finals() {
    for status in [
        AgentStatus::Completed(Some("same final".to_string())),
        AgentStatus::Completed(None),
        AgentStatus::Errored("real error".to_string()),
    ] {
        for delivered_turns in [
            HashSet::new(),
            HashSet::from(["finished-turn".to_string()]),
            HashSet::from(["earlier-turn".to_string()]),
        ] {
            let terminal = Some(("finished-turn".to_string(), status.clone()));
            let result =
                InitialTerminalObservation::ReconcileIfAdvancedFrom(AgentStatus::PendingInit)
                    .reconcile(
                        /*active_turn_id*/ None,
                        terminal.clone(),
                        status.clone(),
                    )
                    .without_delivered_finals(&delivered_turns);
            let suppress = delivered_turns.contains("finished-turn")
                && matches!(&status, AgentStatus::Completed(_));
            assert_eq!(
                result,
                InitialTerminalReconciliation {
                    terminal: if suppress { None } else { terminal },
                    status: status.clone(),
                },
            );
        }
    }
}

#[test]
fn resume_does_not_catch_up_history_when_already_terminal_or_running_next_turn() {
    let completed = AgentStatus::Completed(Some("historical final".to_string()));
    for (previous, active_turn_id, current) in [
        (completed.clone(), None, completed.clone()),
        (
            AgentStatus::PendingInit,
            Some("next-turn".to_string()),
            AgentStatus::Running,
        ),
    ] {
        let result = InitialTerminalObservation::ReconcileIfAdvancedFrom(previous).reconcile(
            active_turn_id,
            Some(("finished-turn".to_string(), completed.clone())),
            current.clone(),
        );
        assert_eq!((result.terminal, result.status), (None, current));
    }
}

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
        )
        .without_delivered_finals(&HashSet::from(["earlier-turn".to_string()]));
    assert_eq!(
        result,
        InitialTerminalReconciliation {
            terminal: Some(("active-turn".to_string(), status.clone())),
            status,
        },
    );
}
