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
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let (child_id, child) = harness
        .start_thread_with_source(harness.config.clone(), child_source.clone())
        .await;
    let control = harness.control.clone();
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
    assert!(control.has_completion_watcher(parent_presentation, child_presentation));
    let completed_status = AgentStatus::Completed(Some("final accepted before unload".to_string()));
    let identity = control
        .model_visible_agent_identity(&parent, child_id)
        .await
        .expect("child identity");
    let expected_message =
        crate::session_prefix::format_subagent_notification_message(identity, &completed_status);
    let store = control
        .upgrade()
        .expect("live thread manager")
        .thread_store();
    parent.ensure_rollout_materialized().await;
    child.ensure_rollout_materialized().await;
    parent.flush_rollout().await.expect("materialize parent");
    child.flush_rollout().await.expect("materialize child");

    // Capture first: the real unload now owns the child's lifecycle lock. Publishing the
    // terminal below synchronously reserves parent completion admission, but its watcher
    // cannot pass that lock. No manual presentation claim or synthetic token is involved.
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
    assert!(control.has_completion_watcher(parent_presentation, child_presentation));
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
            .response_observation_delivery_can_retry()
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
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: parent_id,
            include_archived: false,
        })
        .await
        .expect("read parent history after writer release");
    let final_messages = history
        .items
        .iter()
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
