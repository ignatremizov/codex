use super::*;
use crate::UserAgentSpawnOptions;
use codex_protocol::CollabAgentInputBatch;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn live_batch_audit_preserves_turn_scoped_ids_without_writing_root_history() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::Collab).expect("V1");
    config
        .features
        .disable(Feature::MultiAgentV2)
        .expect("not V2");
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (root_id, root) = harness.start_thread().await;
    let sender_id = root
        .spawn_agent(UserAgentSpawnOptions::default())
        .await
        .expect("idle sender")
        .target_thread_id;
    let sender = harness
        .manager
        .get_thread(sender_id)
        .await
        .expect("native sender");
    let control = sender
        .session
        .services
        .local_agent_runtime
        .control(sender.session.session_id());
    root.ensure_rollout_materialized().await;
    root.flush_rollout().await.expect("settle setup history");
    let before = root
        .session
        .services
        .thread_store
        .load_mailbox_canonical_history(root_id)
        .await
        .expect("canonical root history");
    let model_before = root.session.clone_history().await.into_annotated_items();
    while root.try_next_event().expect("root event stream").is_some() {}

    let mut expected = Vec::new();
    for (source_turn, call_id) in [
        ("first-turn", "reused-call"),
        ("second-turn", "reused-call"),
        ("opaque/turn", "call"),
        ("opaque", "turn/call"),
    ] {
        let mut item = CollabAgentToolCallItem {
            input_batch: Some(CollabAgentInputBatch {
                flags: String::new(),
                sender_thread_id: Some(sender_id),
                results: Vec::new(),
            }),
            id: call_id.into(),
            tool: CollabAgentTool::SendInput,
            status: CollabAgentToolCallStatus::Completed,
            deadline_at_ms: None,
            observe_commentary: None,
            wake_on_completion: None,
            target_messages: None,
            queue_input: None,
            mailbox_input: None,
            sender_thread_id: sender_id,
            receiver_thread_ids: Vec::new(),
            receiver_agents: Vec::new(),
            prompt: Some("presentation-only batch evidence".into()),
            model: None,
            reasoning_effort: None,
            agents_states: Default::default(),
            completion_presentation_agent_ids: None,
        };
        control
            .mirror_agent_input_batch(source_turn, item.clone())
            .await
            .expect("live mirror");
        item.id = format!(
            "agent-input-batch/{}",
            serde_json::to_string(&(sender_id, source_turn, call_id)).expect("opaque source tuple")
        );
        expected.push(item);
    }
    let mut received = Vec::new();
    while let Some(event) = root.try_next_event().expect("root event stream") {
        if let EventMsg::ItemCompleted(event) = event.msg
            && let TurnItem::CollabAgentToolCall(item) = event.item
            && item.input_batch.is_some()
        {
            assert_eq!(event.thread_id, root_id);
            received.push(item);
        }
    }
    assert_eq!(received, expected);
    assert_eq!(
        received
            .iter()
            .map(|item| item.id.as_str())
            .collect::<std::collections::HashSet<_>>()
            .len(),
        4
    );
    assert_eq!(
        root.session.clone_history().await.into_annotated_items(),
        model_before
    );
    root.flush_rollout()
        .await
        .expect("settle root after live audit");
    let after = root
        .session
        .services
        .thread_store
        .load_mailbox_canonical_history(root_id)
        .await
        .expect("unchanged canonical root history");
    assert_eq!(
        serde_json::to_value(&after).expect("serialize current canonical history"),
        serde_json::to_value(&before).expect("serialize original canonical history"),
    );
    assert!(root.session.active_turn.lock().await.is_none());
    sender.shutdown_and_wait().await.expect("shutdown sender");
    root.shutdown_and_wait().await.expect("shutdown root");
}
