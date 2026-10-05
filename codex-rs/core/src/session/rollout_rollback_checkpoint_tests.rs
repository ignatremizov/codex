//! Standalone checkpoints describe the state at their own rollback boundary.

use crate::session::tests::make_session_and_context;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::UserMessageEvent;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(true, false, ThreadHistoryMode::Legacy; "legacy checkpoint after rollback")]
#[test_case(false, false, ThreadHistoryMode::Legacy; "legacy checkpoint before rollback")]
#[test_case(true, true, ThreadHistoryMode::Legacy; "resume checkpoint after rollback")]
#[test_case(false, true, ThreadHistoryMode::Legacy; "resume checkpoint before rollback")]
#[test_case(true, false, ThreadHistoryMode::Paginated; "paginated checkpoint after rollback")]
#[test_case(false, false, ThreadHistoryMode::Paginated; "paginated checkpoint before rollback")]
#[test_case(true, true, ThreadHistoryMode::Paginated; "paginated resume checkpoint after rollback")]
#[test_case(false, true, ThreadHistoryMode::Paginated; "paginated resume checkpoint before rollback")]
#[tokio::test]
async fn reconstruction_respects_checkpoint_rollback_order(
    checkpoint_after_rollback: bool,
    has_resume_metadata: bool,
    history_mode: ThreadHistoryMode,
) {
    let (session, mut turn) = make_session_and_context().await;
    turn.history_mode = history_mode;
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
        resume_metadata: has_resume_metadata.then_some(CompactionResumeMetadata {
            multi_agent_version: None,
            last_started_turn_id: None,
            previous_turn_settings: None,
        }),
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

#[test_case(ThreadHistoryMode::Legacy, false; "legacy distinct turn ids")]
#[test_case(ThreadHistoryMode::Legacy, true; "legacy repeated turn ids")]
#[test_case(ThreadHistoryMode::Paginated, false; "paginated distinct turn ids")]
#[test_case(ThreadHistoryMode::Paginated, true; "paginated repeated turn ids")]
#[tokio::test]
async fn count_only_rollback_restores_last_started_from_surviving_occurrences(
    history_mode: ThreadHistoryMode,
    repeated_turn_id: bool,
) {
    for has_checkpoint in [false, true] {
        for removed_turns in [1, 2] {
            let (session, mut turn) = make_session_and_context().await;
            turn.history_mode = history_mode;
            let message = |text: &str| ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText { text: text.to_string() }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            };
            let recorded_turn = |id: &str, text: &str| vec![
                RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
                    turn_id: id.to_string(),
                    root_turn_id: None,
                    trace_id: None,
                    started_at: None,
                    model_context_window: None,
                    collaboration_mode_kind: codex_protocol::config_types::ModeKind::Default,
                })),
                RolloutItem::EventMsg(EventMsg::UserMessage(UserMessageEvent {
                    message: text.to_string(),
                    ..Default::default()
                })),
                RolloutItem::ResponseItem(message(text).into()),
                RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
                    turn_id: id.to_string(),
                    last_agent_message: None,
                    error: None,
                    started_at: None,
                    completed_at: None,
                    duration_ms: None,
                    time_to_first_token_ms: None,
                })),
            ];
            let mut items = Vec::new();
            let mut expected_history = Vec::<ResponseItemEnvelope>::new();
            if has_checkpoint {
                let baseline = message("checkpoint baseline");
                items.push(RolloutItem::Compacted(CompactedItem {
                    replacement_history: Some(vec![baseline.clone().into()]),
                    replacement_history_media_sanitized_prefix_len: Some(1),
                    window_number: Some(4),
                    resume_metadata: Some(CompactionResumeMetadata {
                        multi_agent_version: None,
                        last_started_turn_id: Some("checkpoint-turn".to_string()),
                        previous_turn_settings: None,
                    }),
                    ..Default::default()
                }));
                expected_history.push(baseline.into());
            }
            items.extend(recorded_turn("surviving-turn", "first user"));
            items.extend(recorded_turn(
                if repeated_turn_id {
                    "surviving-turn"
                } else {
                    "removed-turn"
                },
                "second user",
            ));
            items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                ThreadRolledBackEvent {
                    num_turns: removed_turns,
                    materialized_turns: None,
                    rollback_start_index: None,
                },
            )));
            let expected_last_started = if removed_turns == 1 {
                expected_history.push(message("first user").into());
                Some("surviving-turn".to_string())
            } else {
                has_checkpoint.then(|| "checkpoint-turn".to_string())
            };

            let reconstructed = session
                .reconstruct_history_from_rollout(&turn, &items)
                .await;

            assert_eq!(
                (reconstructed.history, reconstructed.last_started_turn_id),
                (expected_history, expected_last_started),
                "checkpoint={has_checkpoint}, removed_turns={removed_turns}",
            );
        }
    }
}

#[test_case(ThreadHistoryMode::Legacy; "legacy canonical indices")]
#[test_case(ThreadHistoryMode::Paginated; "paginated canonical indices")]
#[tokio::test]
async fn exact_rollback_keeps_checkpoint_metadata_and_masks_its_raw_suffix(
    history_mode: ThreadHistoryMode,
) {
    let (session, mut turn) = make_session_and_context().await;
    turn.history_mode = history_mode;
    let text = |text: &str| ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    let checkpoint = CompactedItem {
        message: "surviving checkpoint".to_string(),
        replacement_history: Some(vec![text("surviving checkpoint").into()]),
        replacement_history_media_sanitized_prefix_len: Some(1),
        window_number: Some(4),
        resume_metadata: Some(CompactionResumeMetadata {
            multi_agent_version: None,
            last_started_turn_id: Some("surviving-turn".to_string()),
            previous_turn_settings: None,
        }),
        ..Default::default()
    };
    let removed = CompactedItem {
        replacement_history: Some(vec![text("removed checkpoint").into()]),
        window_number: Some(5),
        resume_metadata: Some(CompactionResumeMetadata {
            multi_agent_version: None,
            last_started_turn_id: Some("removed-turn".to_string()),
            previous_turn_settings: None,
        }),
        ..checkpoint.clone()
    };
    let items = vec![
        RolloutItem::ResponseItem(text("older canonical record").into()),
        RolloutItem::Compacted(checkpoint),
        RolloutItem::EventMsg(EventMsg::TurnStarted(
            codex_protocol::protocol::TurnStartedEvent {
                turn_id: "removed-turn".to_string(),
                root_turn_id: None,
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: codex_protocol::config_types::ModeKind::Default,
            },
        )),
        RolloutItem::Compacted(removed),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 0,
            materialized_turns: Some(1),
            rollback_start_index: Some(2),
        })),
        RolloutItem::ResponseItem(text("new surviving suffix").into()),
    ];
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn, &items)
        .await;
    assert_eq!(
        (
            reconstructed.history,
            reconstructed.last_started_turn_id,
            reconstructed.window_number,
            reconstructed.previous_turn_settings,
        ),
        (
            vec![
                text("surviving checkpoint").into(),
                text("new surviving suffix").into()
            ],
            Some("surviving-turn".to_string()),
            4,
            None,
        ),
    );
}
