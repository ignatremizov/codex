//! Accepted V1 completion delivery must drain before subtree unload releases its writers.

use super::*;
use codex_protocol::models::plaintext_agent_message_content;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_thread_store::LoadThreadHistoryParams;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn subtree_unload_drains_v1_final_blocked_on_child_lifecycle_before_releasing_writers() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("enable V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("disable V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_id, parent) = harness.start_thread().await;
    let child_id = parent
        .spawn_agent(crate::UserAgentSpawnOptions::default())
        .await
        .expect("idle child")
        .target_thread_id;
    let child = harness.manager.get_thread(child_id).await.expect("child");
    let child_source = child.session_source.clone();
    let control = parent.session.services.agent_control.clone();
    let parent_presentation = parent.session.presentation_id();
    let child_presentation = child.session.presentation_id();
    let child_turn = child.session.new_default_turn().await;
    assert!(child.session.begin_agent_response_turn(&child_turn.sub_id));
    control
        .ensure_v1_completion_watcher(
            child_id,
            child_source,
            ResponseObservationPolicy::default(),
            child.agent_status().await,
        )
        .await
        .expect("register the real V1 completion observer");
    assert_eq!(
        control.completion_parent_for_child(child_presentation, parent_id),
        Some(parent_presentation),
    );
    let completed_status = AgentStatus::Completed(Some("final accepted before unload".to_string()));
    let identity = control
        .model_visible_agent_identity_for_version(MultiAgentVersion::V1, child_id)
        .await
        .expect("child identity");
    let expected_message =
        crate::session_prefix::format_subagent_notification_message(identity, &completed_status);
    let store = Arc::clone(&parent.session.services.thread_store);
    parent.ensure_rollout_materialized().await;
    child.ensure_rollout_materialized().await;
    parent.flush_rollout().await.expect("materialize parent");
    child.flush_rollout().await.expect("materialize child");

    // Capture first, then hold the observer transaction while publishing a real terminal.
    // The terminal reserves parent completion admission synchronously; its delivery cannot
    // finish until the transaction is released.
    let mut subtree = harness
        .manager
        .prepare_subtree_unload(child_id)
        .await
        .expect("capture the owning subtree");
    assert_eq!(subtree.thread_ids(), &[parent_id, child_id]);
    let observation_transaction = control
        .acquire_response_observation_transaction(parent_presentation)
        .await;
    timeout(
        Duration::from_secs(/*secs*/ 10),
        child.session.send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some("final accepted before unload".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        ),
    )
    .await
    .expect("terminal publication does not wait for the watcher lifecycle lock");
    assert_eq!(child.agent_status().await, completed_status);
    assert_eq!(
        control.completion_parent_for_child(child_presentation, parent_id),
        Some(parent_presentation),
    );
    assert!(
        parent
            .session
            .clone_history()
            .await
            .raw_items()
            .all(|item| {
                !matches!(item, ResponseItem::AgentMessage { id: Some(id), .. }
            if is_sub_agent_completion_context_response_item_id(id.as_str()))
            })
    );

    let unload = tokio::spawn(async move { subtree.shutdown_and_remove().await });
    timeout(Duration::from_secs(/*secs*/ 10), async {
        while parent
            .session
            .submission_admission
            .try_accept_completion_delivery()
            .is_some()
        {
            sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await
    .expect("unload reaches parent shutdown admission");
    // The second gate keeps the accepted final unfinished after unload releases lifecycle
    // guards. Parent shutdown must still wait, retaining both its actor and its writer.
    assert!(!unload.is_finished());
    assert!(!parent.io.durable_shutdown_succeeded());
    assert!(
        store.reserve_thread_writers(vec![parent_id]).await.is_err(),
        "the parent writer must not be released ahead of accepted completion delivery"
    );
    drop(observation_transaction);
    assert_eq!(
        timeout(Duration::from_secs(/*secs*/ 10), unload)
            .await
            .expect("accepted completion and subtree unload must not deadlock")
            .expect("unload task")
            .expect("durable subtree unload"),
        vec![parent_id, child_id],
    );
    assert!(harness.manager.list_thread_ids().await.is_empty());
    assert!(parent.io.durable_shutdown_succeeded());
    assert!(child.io.durable_shutdown_succeeded());
    let _writers = store
        .reserve_thread_writers(vec![parent_id, child_id])
        .await
        .expect("successful unload releases both writer leases");
    let history = store
        .load_canonical_artifact_segments(LoadThreadHistoryParams {
            thread_id: parent_id,
            include_archived: false,
        })
        .await
        .expect("read parent history after writer release");
    let final_messages = history
        .segments
        .iter()
        .flatten()
        .filter_map(|item| match item {
            RolloutItem::ResponseItem(envelope) => match &envelope.item {
                ResponseItem::AgentMessage {
                    id: Some(id),
                    content,
                    ..
                } if is_sub_agent_completion_context_response_item_id(id.as_str()) => {
                    plaintext_agent_message_content(content)
                }
                _ => None,
            },
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(final_messages, vec![expected_message]);
}
