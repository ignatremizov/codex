use super::*;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_thread_store::LoadSubAgentCompletionPresentationParams;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use std::time::Duration;
use test_case::test_case;

fn audit_with_claim(
    control: &LocalAgentControl,
    root: SessionPresentationId,
) -> (AgentTerminalPresentation, RootCompletionAuditClaim) {
    // An unbacked control isolates token arbitration from the independently spawned
    // live-root lookup. The publisher still uses a real Session and canonical store.
    let audit = control
        .record_root_completion_audit(
            PreparedRootCompletionAudit {
                parent: root,
                child_generation: 0,
            },
            SessionPresentationId::new(ThreadId::new(), Uuid::now_v7()),
            "peer-turn",
            AgentStatus::Completed(Some("peer conclusion".to_string())),
        )
        .expect("root audit token");
    let claim = RootCompletionAuditClaim {
        control: control.clone(),
        terminal: audit.clone(),
    };
    (audit, claim)
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "The test deliberately holds admission while polling both futures to prove a blocked audit does not hold the observer transaction."
)]
async fn root_audit_waits_for_admission_without_holding_observation_and_rechecks_policy() {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let control = LocalAgentControl::default();
    let root = session.presentation_id();
    let (audit, claim) = audit_with_claim(&control, root);
    let presentation = audit
        .hidden_observation_presentation("child")
        .expect("hidden row");
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let admission = session
        .submission_admission
        .admit_injection()
        .await
        .expect("hold admission");
    let mut publication =
        Box::pin(session.publish_root_completion_item(presentation, &accepted, &claim));
    assert!(futures::poll!(publication.as_mut()).is_pending());

    let mut observer = Box::pin(control.acquire_response_observation_transaction(root));
    let std::task::Poll::Ready(transaction) = futures::poll!(observer.as_mut()) else {
        panic!("an audit blocked on admission must not own the observation transaction");
    };
    control
        .runtime
        .wait_agent_presentations
        .state()
        .response_observation_by_observer_child
        .entry((root, audit.child()))
        .or_default()
        .turns
        .entry("peer-turn".to_string())
        .or_default()
        .final_response = FinalResponseObservation::PresentationOnly;
    drop(transaction);
    drop(admission);
    assert!(
        !tokio::time::timeout(Duration::from_secs(/*secs*/ 5), publication)
            .await
            .expect("audit recheck finishes")
            .expect("superseded audit")
    );
    assert_eq!(store.calls().await.append_completion_items_and_flush, 0);
    assert!(events.try_recv().is_err());
    assert!(!audit.inner.ownership.lock().expect("ownership").committed);
    assert!(
        control
            .runtime
            .wait_agent_presentations
            .state()
            .contexts
            .contains_key(&audit.inner.context_id)
    );
}

#[tokio::test]
async fn root_audit_does_not_hold_history_while_waiting_for_observer_transaction() {
    let (mut session, _, _events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let control = LocalAgentControl::default();
    let root = session.presentation_id();
    let (audit, claim) = audit_with_claim(&control, root);
    let presentation = audit
        .hidden_observation_presentation("child")
        .expect("hidden row");
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    let transaction = control.acquire_response_observation_transaction(root).await;
    let mut publication =
        Box::pin(session.publish_root_completion_item(presentation, &accepted, &claim));
    assert!(futures::poll!(publication.as_mut()).is_pending());
    let mut history = Box::pin(session.reserve_history_publication());
    let std::task::Poll::Ready(permit) = futures::poll!(history.as_mut()) else {
        panic!("an observer waiting for history must not be blocked by an audit waiting for it");
    };
    drop(permit);
    drop(transaction);
    assert!(
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), publication)
            .await
            .expect("publication finishes")
            .expect("published audit")
    );
    assert!(audit.inner.ownership.lock().expect("ownership").committed);
    assert_eq!(store.calls().await.append_completion_items_and_flush, 1);
}

#[tokio::test]
async fn root_audit_releases_claim_for_taskless_cleanup_and_commits_only_the_hidden_row() {
    let (mut session, _, events) = make_session_and_context_with_rx().await;
    let store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let control = LocalAgentControl::default();
    let root = session.presentation_id();
    let (audit, claim) = audit_with_claim(&control, root);
    let presentation = audit
        .hidden_observation_presentation("child")
        .expect("hidden row");
    let accepted = session
        .submission_admission
        .try_accept_completion_delivery()
        .expect("accepted");
    *session.active_turn.lock().await = Some(crate::state::ActiveTurn {
        terminal_pending: true,
        ..Default::default()
    });
    let mut publication =
        Box::pin(session.publish_root_completion_item(presentation, &accepted, &claim));
    assert!(futures::poll!(publication.as_mut()).is_pending());
    let mut observer = Box::pin(control.acquire_response_observation_transaction(root));
    let std::task::Poll::Ready(transaction) = futures::poll!(observer.as_mut()) else {
        panic!("taskless cleanup must be able to acquire the observer transaction");
    };
    assert_eq!(store.calls().await.append_completion_items_and_flush, 0);
    drop(transaction);
    *session.active_turn.lock().await = None;
    session.active_turn_transition.notify_waiters();
    assert!(
        tokio::time::timeout(Duration::from_secs(/*secs*/ 5), publication)
            .await
            .expect("cleanup unblocks publication")
            .expect("published audit")
    );
    assert!(
        audit.wait_owns_presentation().await,
        "canonical commit owns the shared token"
    );
    assert!(
        !control
            .runtime
            .wait_agent_presentations
            .state()
            .contexts
            .contains_key(&audit.inner.context_id)
    );
    let stored = store
        .load_sub_agent_completion_presentation(LoadSubAgentCompletionPresentationParams {
            thread_id: root.thread_id,
            include_archived: false,
            item_id: presentation.item.id(),
            turn_id: presentation.history_only_turn_id.clone(),
        })
        .await
        .expect("read canonical presentation");
    assert_eq!(
        serde_json::to_value(stored.item_completed.expect("acknowledged row").item)
            .expect("stored item"),
        serde_json::to_value(&presentation.item).expect("hidden source item"),
    );
    assert_eq!(
        control.response_observation_snapshots(root, audit.child()),
        Vec::new()
    );
    assert_eq!(
        session.clone_history().await.into_annotated_items(),
        Vec::new()
    );
    let delivered = std::iter::from_fn(|| events.try_recv().ok())
        .filter(|event| {
            matches!(&event.msg, codex_protocol::protocol::EventMsg::ItemCompleted(event)
            if event.item.is_sub_agent_completion_presentation())
        })
        .count();
    assert_eq!(delivered, 1);
}

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
            .runtime
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
