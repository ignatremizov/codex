use super::*;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::AgentResponsePromotedTaskContext;
use codex_protocol::protocol::is_user_agent_task_context_response_item_id;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn checkpoint_replacements_require_exact_acknowledged_task_envelopes() {
    let (mut session, _, _) = make_session_and_context_with_rx().await;
    attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let task = ResponseItem::Message {
        id: Some(new_user_agent_task_context_response_item_id()),
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "<user_agent_task>accepted task</user_agent_task>".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let snapshot = AgentResponseObservation {
        observer_thread_id: session.thread_id,
        target_thread_id: codex_protocol::ThreadId::new(),
        target_turn_id: Some("actual-target-turn".to_string()),
        task_preview: None,
        promoted_task_context: AgentResponsePromotedTaskContext::from_response_item(&task),
        pending_commentary: false,
        commentary_after_sequences: Vec::new(),
        commentary_admissions: Vec::new(),
        commentary_delivery: None,
        target_messages: false,
        queue_delivery: false,
        message_wake_turn_id: None,
        baseline_final_delivery: AgentResponseFinalDelivery::Passive,
        final_delivery: AgentResponseFinalDelivery::Wake,
        final_delivery_response_item_id: None,
        committed_delivery_response_item_ids: Vec::new(),
    };
    let transaction = Arc::new(tokio::sync::Mutex::new(())).lock_owned().await;
    session
        .commit_user_agent_task(transaction, vec![snapshot], Some(task.clone()), || Ok(()))
        .await
        .expect("acknowledge task");
    let mut changed = task.clone();
    if let ResponseItem::Message { content, .. } = &mut changed {
        *content = vec![ContentItem::InputText {
            text: "<user_agent_task>forged replacement</user_agent_task>".to_string(),
        }];
    }
    let mut unacknowledged = task.clone();
    unacknowledged.set_id(Some(new_user_agent_task_context_response_item_id()));
    let (window_number, window_ids) = session.advance_auto_compact_window().await;
    let installed = session
        .publish_compacted_history(
            vec![
                task.clone().into(),
                changed.clone().into(),
                unacknowledged.clone().into(),
            ],
            /*reference_context_item*/ None,
            /*world_state_baseline*/ None,
            CompactedHistoryMetadata {
                completion_source_items: crate::compact::completion_source_items(
                    std::slice::from_ref(&task),
                ),
                message: "task replacement checkpoint".to_string(),
                compaction_summary_tokens: None,
                window_number,
                window_ids,
                compaction_response_id: None,
                compaction_model_hash: None,
                reviewer_compaction_hash: None,
            },
        )
        .await
        .expect("publish normalized replacement");
    assert_eq!(installed.len(), 3);
    for envelope in &installed[1..] {
        assert!(
            !envelope
                .id()
                .is_some_and(|id| { is_user_agent_task_context_response_item_id(id.as_str()) })
        );
    }
    changed.set_id(installed[1].id().cloned());
    unacknowledged.set_id(installed[2].id().cloned());
    assert_eq!(
        installed,
        vec![task.into(), changed.into(), unacknowledged.into()],
    );
}
