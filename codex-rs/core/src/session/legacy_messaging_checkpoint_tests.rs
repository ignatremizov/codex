use crate::context::AgentContextIdentity;
use crate::context::AgentReplyRoute;
use crate::context::ContextualUserFragment;
use crate::context::world_state::WorldStateSnapshot;
use crate::session::CompactedHistoryMetadata;
use crate::session::tests::attach_in_memory_thread_store;
use crate::session::tests::make_session_and_context_with_rx;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ThreadId;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use std::sync::Arc;

fn legacy_route(source: ThreadId) -> ResponseItem {
    ContextualUserFragment::into(AgentReplyRoute::until_disabled(
        AgentContextIdentity::Canonical { agent_id: source },
    ))
}

#[tokio::test]
async fn stale_checkpoint_retains_legacy_routes_without_importing_new_sources() {
    let (mut session, turn, _) = make_session_and_context_with_rx().await;
    let _store =
        attach_in_memory_thread_store(Arc::get_mut(&mut session).expect("unique session")).await;
    let source = ThreadId::new();
    // Seed historical evidence through ordinary canonical publication; there is no live
    // permission publisher or permission installation attached to this record.
    session
        .record_durable_context_items(
            turn.as_ref(),
            turn.model_info(),
            vec![legacy_route(source)],
            /*acknowledgement*/ None,
        )
        .await
        .expect("record legacy evidence");
    let accepted = session.clone_history().await.annotated_items().to_vec();
    for replacement in [
        Vec::new(),
        vec![
            ResponseItemEnvelope::new(legacy_route(source)),
            ResponseItemEnvelope::new(legacy_route(ThreadId::new())),
        ],
    ] {
        let (window_number, window_ids) = session.advance_auto_compact_window().await;
        let installed = session
            .replace_compacted_history(
                replacement,
                turn.to_turn_context_item(),
                WorldStateSnapshot::default(),
                CompactedHistoryMetadata {
                    input_goal_ids: Default::default(),
                    completion_source_items: Vec::new(),
                    message: "summary".to_string(),
                    compaction_summary_tokens: None,
                    window_number,
                    window_ids,
                    compaction_response_id: None,
                    compaction_model_hash: None,
                    reviewer_compaction_hash: None,
                },
            )
            .await
            .expect("replace history")
            .expect("complete checkpoint installed");
        assert_eq!(installed, accepted);
        assert_eq!(session.clone_history().await.annotated_items(), accepted);
    }
}
