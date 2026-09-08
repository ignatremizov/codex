use super::*;
use test_case::test_case;

#[test]
fn root_audit_defers_only_to_an_explicit_observation_of_the_same_turn() {
    for policy in [
        FinalResponseObservation::None,
        FinalResponseObservation::Passive,
        FinalResponseObservation::Wake,
        FinalResponseObservation::PresentationOnly,
    ] {
        let control = AgentControl::default();
        let root = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
        let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
        control
            .wait_agent_presentations
            .state()
            .response_observation_by_observer_child
            .entry((root, child))
            .or_default()
            .turns
            .entry("observed".to_string())
            .or_default()
            .final_response = policy;
        assert!(
            control
                .record_agent_terminal_presentation(
                    root,
                    child,
                    "observed",
                    AgentStatus::Completed(Some("observed conclusion".to_string())),
                    TerminalPresentationDelivery::RootAudit,
                    || {},
                )
                .is_none()
        );
        let audit = control.record_agent_terminal_presentation(
            root,
            child,
            "later-peer-turn",
            AgentStatus::Completed(Some("later conclusion".to_string())),
            TerminalPresentationDelivery::RootAudit,
            || {},
        );
        assert!(
            audit.is_some(),
            "an earlier observation cannot hide a later audit"
        );
        assert!(
            control
                .record_agent_terminal_presentation(
                    root,
                    child,
                    "later-peer-turn",
                    AgentStatus::Completed(Some("later conclusion".to_string())),
                    TerminalPresentationDelivery::RootAudit,
                    || {},
                )
                .is_none(),
            "the same live terminal turn is registered once"
        );
    }
}

#[derive(Clone, Copy)]
enum WaitTiming {
    BeforeTerminal,
    AfterTerminal,
}

#[test_case(WaitTiming::BeforeTerminal; "wait_already_active")]
#[test_case(WaitTiming::AfterTerminal; "wait_after_terminal_before_audit")]
#[tokio::test]
async fn wait_can_claim_root_audit(timing: WaitTiming) {
    let control = AgentControl::default();
    let root = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let early_wait = match timing {
        WaitTiming::BeforeTerminal => {
            Some(control.register_targeted_wait_agent_presentation(root, &[child.thread_id]))
        }
        WaitTiming::AfterTerminal => None,
    };
    let audit = control
        .record_agent_terminal_presentation(
            root,
            child,
            "peer-turn",
            AgentStatus::Completed(Some("done".to_string())),
            TerminalPresentationDelivery::RootAudit,
            || {},
        )
        .expect("unobserved root audit");
    let wait = early_wait.unwrap_or_else(|| {
        control.register_targeted_wait_agent_presentation(root, &[child.thread_id])
    });
    wait.freeze_for_children([child.thread_id]).commit();
    assert!(audit.wait_owns_presentation().await);
}

#[test]
fn root_audit_dedupe_is_scoped_to_live_root_and_child_instances() {
    let control = AgentControl::default();
    let root_id = ThreadId::new();
    let child_id = ThreadId::new();
    let root = SessionPresentationId::new(root_id, Uuid::now_v7());
    let child = SessionPresentationId::new(child_id, Uuid::now_v7());
    for (root, child) in [
        (root, child),
        (SessionPresentationId::new(root_id, Uuid::now_v7()), child),
        (root, SessionPresentationId::new(child_id, Uuid::now_v7())),
    ] {
        assert!(
            control
                .record_agent_terminal_presentation(
                    root,
                    child,
                    "turn",
                    AgentStatus::Completed(Some("done".to_string())),
                    TerminalPresentationDelivery::RootAudit,
                    || {},
                )
                .is_some()
        );
    }
}
