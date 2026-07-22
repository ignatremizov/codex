//! Standalone checkpoints describe the state at their own rollback boundary.

use crate::session::tests::make_session_and_context;
use codex_history::CompactedItem;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(true; "checkpoint after rollback survives")]
#[test_case(false; "checkpoint before rollback is removed")]
#[tokio::test]
async fn reconstruction_respects_checkpoint_rollback_order(checkpoint_after_rollback: bool) {
    let (session, turn) = make_session_and_context().await;
    let replacement = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: "latest checkpoint".to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let checkpoint = RolloutItem::Compacted(CompactedItem {
        message: "latest checkpoint".to_string(),
        replacement_history: Some(vec![replacement.clone().into()]),
        replacement_history_media_sanitized_prefix_len: Some(1),
        window_number: Some(2),
        ..Default::default()
    });
    let rollback = RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
        num_turns: 1,
        materialized_turns: None,
        rollback_start_index: None,
    }));
    let mut items = vec![RolloutItem::EventMsg(EventMsg::UserMessage(
        UserMessageEvent {
            message: "removed user turn".to_string(),
            ..Default::default()
        },
    ))];
    items.extend(if checkpoint_after_rollback {
        [rollback, checkpoint]
    } else {
        [checkpoint, rollback]
    });

    let reconstructed = session
        .reconstruct_history_from_rollout(&turn, &items)
        .await;
    let expected: Vec<ResponseItemEnvelope> = if checkpoint_after_rollback {
        vec![replacement.into()]
    } else {
        Vec::new()
    };
    assert_eq!(reconstructed.history, expected);
}
