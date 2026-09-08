use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[tokio::test]
async fn root_audit_defers_only_to_an_explicit_final_observation_of_the_same_turn() {
    for policy in [
        FinalResponseObservation::None,
        FinalResponseObservation::Passive,
        FinalResponseObservation::Wake,
        FinalResponseObservation::PresentationOnly,
    ] {
        let control = LocalAgentControl::default();
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
        assert_eq!(
            control
                .record_root_completion_audit(
                    PreparedRootCompletionAudit {
                        parent: root,
                        child_generation: 0
                    },
                    child,
                    "observed",
                    AgentStatus::Completed(Some("observed conclusion".to_string())),
                )
                .is_some(),
            policy == FinalResponseObservation::None,
        );
        let audit = control.record_root_completion_audit(
            PreparedRootCompletionAudit {
                parent: root,
                child_generation: 0,
            },
            child,
            "later-peer-turn",
            AgentStatus::Completed(Some("later conclusion".to_string())),
        );
        assert!(
            audit.is_some(),
            "an earlier observation cannot hide a later audit"
        );
        assert!(
            control
                .record_root_completion_audit(
                    PreparedRootCompletionAudit {
                        parent: root,
                        child_generation: 0
                    },
                    child,
                    "later-peer-turn",
                    AgentStatus::Completed(Some("later conclusion".to_string())),
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
    let control = LocalAgentControl::default();
    let root = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let early_wait = match timing {
        WaitTiming::BeforeTerminal => {
            Some(control.register_targeted_wait_agent_presentation(root, &[child.thread_id]))
        }
        WaitTiming::AfterTerminal => None,
    };
    let audit = control
        .record_root_completion_audit(
            PreparedRootCompletionAudit {
                parent: root,
                child_generation: 0,
            },
            child,
            "peer-turn",
            AgentStatus::Completed(Some("done".to_string())),
        )
        .expect("unobserved root audit");
    assert!(audit.take_accepted_completion_delivery().is_none());
    assert!(audit.take_parent_thread().is_none());
    assert_eq!(
        control.response_observation_snapshots(root, child),
        Vec::new()
    );
    let wait = early_wait.unwrap_or_else(|| {
        control.register_targeted_wait_agent_presentation(root, &[child.thread_id])
    });
    wait.freeze_for_children([child.thread_id]).commit();
    assert!(audit.wait_owns_presentation().await);
}

#[tokio::test]
async fn root_audit_dedupe_is_scoped_to_live_root_and_child_instances() {
    let control = LocalAgentControl::default();
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
                .record_root_completion_audit(
                    PreparedRootCompletionAudit {
                        parent: root,
                        child_generation: 0
                    },
                    child,
                    "turn",
                    AgentStatus::Completed(Some("done".to_string())),
                )
                .is_some()
        );
    }
}
