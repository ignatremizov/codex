use super::ContextManager;
use crate::tasks::InterruptedTurnHistoryMarker;
use crate::tasks::interrupted_turn_history_marker;
use codex_guardian_context::TranscriptHistory;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TurnContextItem;
use pretty_assertions::assert_eq;
use serde_json::json;

fn context(ids: &[&str]) -> anyhow::Result<TurnContextItem> {
    Ok(serde_json::from_value(json!({
        "turn_id": "interrupted",
        "cwd": std::env::current_dir()?,
        "approval_policy": "never",
        "sandbox_policy": {"type": "danger-full-access"},
        "model": "test-model",
        "summary": "auto",
        "recovery_start_options": {"cleared_interrupt_message_ids": ids},
    }))?)
}

#[test]
fn continuation_omits_only_own_typed_notice_and_preserves_other_context() -> anyhow::Result<()> {
    for marker_kind in [
        InterruptedTurnHistoryMarker::ContextualUser,
        InterruptedTurnHistoryMarker::Developer,
    ] {
        let mut marker = interrupted_turn_history_marker(marker_kind)
            .ok_or_else(|| anyhow::anyhow!("enabled marker"))?;
        marker.set_id(Some(ResponseItemId::from_server("notice".to_string())));
        let mut previous = marker.clone();
        previous.set_turn_id_if_missing("previous");
        let previous = ResponseItemEnvelope::new(previous);
        marker.set_turn_id_if_missing("interrupted");
        let marker = ResponseItemEnvelope::new(marker);
        let mut client = marker.clone();
        client.metadata = Some(CodexHarnessMetadata {
            client_authored: true,
            ..Default::default()
        });
        let mut inherited = marker.clone();
        inherited.metadata = Some(CodexHarnessMetadata {
            inherited_user_message: true,
            ..Default::default()
        });
        let user = ResponseItemEnvelope::new(serde_json::from_value::<ResponseItem>(json!({
            "type": "message",
            "id": "notice",
            "role": "user",
            "content": [{"type": "input_text", "text": "<turn_aborted>user text</turn_aborted>"}],
            "internal_chat_message_metadata_passthrough": {"turn_id": "interrupted"}
        }))?);
        let tool = ResponseItemEnvelope::new(serde_json::from_value::<ResponseItem>(json!({
            "type": "function_call_output",
            "call_id": "completed-tool",
            "output": "already executed"
        }))?);
        let retained = vec![user, previous, client, inherited, tool];
        let mut original = retained.clone();
        original.insert(1, marker);
        let mut history = ContextManager::new();
        history.replace_annotated(original.clone());
        history.set_compacted_prefix_len(Some(3));
        let mut guardian = TranscriptHistory::new(/*generation*/ 0);
        for item in &original {
            guardian.record(item);
        }
        history.review_history = Some(guardian);
        let edit = context(&["notice"])?;

        assert_eq!(history.apply_turn_continuation(&edit), 1);
        assert_eq!(history.annotated_items(), retained.as_slice());
        assert_eq!(history.compacted_prefix_len(), Some(2));
        let expected_guardian = {
            let mut guardian = TranscriptHistory::new(/*generation*/ 0);
            for item in &retained {
                guardian.record(item);
            }
            guardian.checkpoint()
        };
        assert_eq!(
            history.guardian_history_checkpoint(),
            Some(expected_guardian)
        );
        // Replaying the same durable edit is harmless.
        assert_eq!(history.apply_turn_continuation(&edit), 0);
        assert_eq!(history.annotated_items(), retained.as_slice());
    }
    Ok(())
}
