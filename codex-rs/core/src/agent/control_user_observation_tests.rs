//! Exercise user policy publication against real session history and exact controller state.

use super::*;
use crate::UserAgentFinalResponseHandling;
use crate::UserAgentForkMode;
use crate::UserAgentObservationBinding;
use crate::UserAgentObservationMode;
use crate::UserAgentResponseHandling;
use crate::UserAgentSpawnOptions;

#[tokio::test]
async fn close_response_cannot_hold_a_receipt_inside_the_subtree_being_closed() {
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let parent = root
        .spawn_agent(UserAgentSpawnOptions {
            fork_mode: UserAgentForkMode::None,
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn parent");
    let parent = harness
        .manager
        .get_thread(parent.target_thread_id)
        .await
        .expect("parent");
    let child = parent
        .spawn_agent(UserAgentSpawnOptions {
            fork_mode: UserAgentForkMode::None,
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn child");
    let child = harness
        .manager
        .get_thread(child.target_thread_id)
        .await
        .expect("child");
    let error = child
        .close_agent(
            &parent.session.thread_id().to_string(),
            UserAgentResponseHandling::Passive,
        )
        .await
        .expect_err("closing a source ancestor would strand its accepted receipt");
    assert!(error.to_string().contains("own subtree"));
    assert!(
        harness
            .manager
            .get_thread(parent.session.thread_id())
            .await
            .is_ok()
    );
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(/*secs*/ 5))
        .await;
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
}

#[tokio::test]
async fn user_resume_from_a_sibling_observer_preserves_the_durable_parent() {
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let parent = root
        .spawn_agent(UserAgentSpawnOptions {
            fork_mode: UserAgentForkMode::None,
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn parent");
    let parent = harness
        .manager
        .get_thread(parent.target_thread_id)
        .await
        .expect("parent runtime");
    let child = parent
        .spawn_agent(UserAgentSpawnOptions {
            fork_mode: UserAgentForkMode::None,
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn nested child");
    let control = &root.session.services.agent_control;
    control
        .close_agent(child.target_thread_id)
        .await
        .expect("close nested child");
    let resumed = root
        .resume_agent(
            &child.target_thread_id.to_string(),
            /*task*/ None,
            UserAgentResponseHandling::Passive,
        )
        .await
        .expect("observe resumed descendant from root");
    assert_eq!(resumed.ownership_transfer, None);
    let child = harness
        .manager
        .get_thread(child.target_thread_id)
        .await
        .expect("restored child");
    assert_eq!(
        child.session_source.parent_thread_id(),
        Some(parent.session.thread_id())
    );
    assert_eq!(
        control.completion_parent_for_child(
            child.session.presentation_id(),
            parent.session.thread_id()
        ),
        Some(parent.session.presentation_id()),
    );
    assert_eq!(
        control
            .current_response_observation_binding_for_thread(
                root.session.presentation_id(),
                child.session.thread_id(),
            )
            .await,
        Some(crate::agent::control::ReplacedFinalResponseObservationBinding::NextTurn),
    );
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(/*secs*/ 5))
        .await;
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
}

#[tokio::test]
async fn closed_user_agent_transfer_changes_the_live_owner_without_rewriting_history() {
    let harness = AgentControlHarness::new().await;
    let (_, old_root) = harness.start_thread().await;
    let child = old_root
        .spawn_agent(UserAgentSpawnOptions {
            fork_mode: UserAgentForkMode::None,
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await
        .expect("spawn child");
    old_root
        .session
        .services
        .agent_control
        .close_agent(child.target_thread_id)
        .await
        .expect("close child");
    let state = old_root
        .session
        .services
        .agent_control
        .upgrade()
        .expect("manager");
    let before = state
        .read_stored_thread(ReadThreadParams {
            thread_id: child.target_thread_id,
            include_archived: true,
            include_history: true,
        })
        .await
        .expect("canonical history")
        .history
        .expect("history")
        .items;
    let (_, new_root) = harness.start_thread().await;
    let result = new_root
        .resume_agent(
            &child.target_thread_id.to_string(),
            /*task*/ None,
            UserAgentResponseHandling::Presentation,
        )
        .await
        .expect("explicit UUID transfer");
    assert_eq!(
        result.ownership_transfer,
        Some(crate::UserAgentOwnershipTransfer {
            previous_session_id: Some(old_root.session.session_id()),
            new_session_id: new_root.session.session_id(),
            task_path_mapping: Vec::new(),
        })
    );
    assert_eq!(result.post_commit_warning, None);
    let restored = harness
        .manager
        .get_thread(child.target_thread_id)
        .await
        .expect("restored child");
    assert_eq!(restored.session.session_id(), new_root.session.session_id());
    assert_eq!(
        restored.session_source.parent_thread_id(),
        Some(new_root.session.thread_id())
    );
    let after = state
        .read_stored_thread(ReadThreadParams {
            thread_id: child.target_thread_id,
            include_archived: true,
            include_history: true,
        })
        .await
        .expect("canonical history after transfer")
        .history
        .expect("history")
        .items;
    assert_eq!(after.get(..before.len()), Some(before.as_slice()));
    assert!(
        old_root
            .session
            .services
            .agent_control
            .resolve_controlled_agent_target(
                old_root.session.thread_id(),
                &child.target_thread_id.to_string()
            )
            .await
            .is_err()
    );
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(/*secs*/ 5))
        .await;
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
}

#[tokio::test]
async fn promptless_user_spawn_reserves_one_policy_and_can_downgrade_it() {
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let spawned = root
        .spawn_agent(UserAgentSpawnOptions {
            fork_mode: UserAgentForkMode::None,
            response_handling: UserAgentResponseHandling::Wake,
            ..Default::default()
        })
        .await
        .expect("create idle user agent");
    assert_eq!(spawned.input_outcome, None);
    assert_eq!(spawned.post_admission_warning, None);
    let control = &root.session.services.agent_control;
    let child = harness
        .manager
        .get_thread(spawned.target_thread_id)
        .await
        .expect("published child");
    let parent = root.session.presentation_id();
    let child_id = child.session.presentation_id();
    let before = control.response_observation_snapshots(parent, child_id);
    let mut expected = before.clone();
    assert_eq!(expected.len(), 1);
    expected[0].final_delivery =
        codex_protocol::protocol::AgentResponseFinalDelivery::PresentationOnly;
    let replaced = root
        .observe_agent(
            &spawned.target_thread_id.to_string(),
            UserAgentObservationMode::Presentation,
        )
        .await
        .expect("authoritative user downgrade");
    assert_eq!(
        replaced,
        (
            spawned.target_thread_id,
            UserAgentFinalResponseHandling::Wake,
            UserAgentObservationBinding::NextTurn,
        )
    );
    assert_eq!(
        control.response_observation_snapshots(parent, child_id),
        expected
    );
    assert_eq!(
        control.reserved_response_observation_policy(parent, child_id),
        Some(ResponseObservationPolicy::from_parts(
            /*commentary*/ false,
            FinalResponseObservation::PresentationOnly,
        ))
    );
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(/*secs*/ 5))
        .await;
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
}

#[tokio::test]
async fn acknowledged_policy_cannot_overwrite_a_concurrently_bound_turn() {
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let control = &root.session.services.agent_control;
    let parent = root.session.presentation_id();
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let _registration = control
        .register_response_watcher_with_parent_at_sequence(
            child,
            &root,
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ false,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
            /*minimum_event_sequence*/ 0,
            /*after_item_id*/ None,
        )
        .expect("reserve next turn");
    let transaction = control
        .acquire_response_observation_transaction(parent)
        .await;
    let prepared = control
        .prepare_user_observation(
            parent,
            child,
            /*turn_id*/ None,
            Some(FinalResponseObservation::Wake),
            /*task_preview*/ None,
            /*task*/ None,
        )
        .expect("prepare policy");
    control.bind_response_observation_started_turn_at_sequence(
        parent,
        child,
        "real-turn",
        /*sequence*/ 1,
    );
    let snapshots = prepared.snapshots.clone();
    let commit_control = control.clone();
    let result = root
        .session
        .commit_user_agent_task(transaction, snapshots, /*task*/ None, move || {
            commit_control.commit_user_observation(prepared)
        })
        .await;
    assert!(result.is_err());
    assert!(root.session.submission_admission.check_ready().is_err());
    let snapshots = control.response_observation_snapshots(parent, child);
    assert!(snapshots.iter().any(|snapshot| {
        snapshot.target_turn_id.as_deref() == Some("real-turn")
            && snapshot.final_delivery
                == codex_protocol::protocol::AgentResponseFinalDelivery::Passive
    }));
    assert_eq!(
        control.reserved_response_observation_policy(parent, child),
        None
    );
    drop(_registration);
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(/*secs*/ 5))
        .await;
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
}

#[tokio::test]
async fn user_policy_replacement_cannot_reassign_an_accepted_final_delivery() {
    let harness = AgentControlHarness::new().await;
    let (_, root) = harness.start_thread().await;
    let control = &root.session.services.agent_control;
    let parent = root.session.presentation_id();
    let child = SessionPresentationId::new(ThreadId::new(), Uuid::now_v7());
    let _registration = control
        .register_response_watcher_with_parent_at_sequence(
            child,
            &root,
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ false,
            Some("actual-turn".to_owned()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
            /*minimum_event_sequence*/ 0,
            /*after_item_id*/ None,
        )
        .expect("bind turn");
    let terminal = control
        .record_response_observation_terminal(
            parent,
            child,
            "actual-turn",
            AgentStatus::Completed(Some("done".into())),
        )
        .expect("accepted terminal");
    let context_id = terminal.completion_context_response_item_id();
    control.prepare_final_response_observation_delivery(parent, child, "actual-turn", &context_id);
    let before = control.response_observation_snapshots(parent, child);
    assert!(
        control
            .prepare_user_observation(
                parent,
                child,
                Some("actual-turn".into()),
                Some(FinalResponseObservation::PresentationOnly),
                /*task_preview*/ None,
                /*task*/ None,
            )
            .is_err()
    );
    assert_eq!(
        control.response_observation_snapshots(parent, child),
        before
    );
    assert_eq!(terminal.completion_context_response_item_id(), context_id);
    drop(terminal.take_parent_thread());
    drop(terminal.take_accepted_completion_delivery());
    drop(_registration);
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(/*secs*/ 5))
        .await;
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
}
