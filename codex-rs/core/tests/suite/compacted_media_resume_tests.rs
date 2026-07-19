//! Cold resume repairs compacted media append-only before the next model request.

use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_history::CompactedItem;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_protocol::ResponseItemId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ImageReference;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutRecorder;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn cold_resume_repairs_media_once_without_rewriting_canonical_audit() -> Result<()> {
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::Legacy)
        .with_config(|config| {
            config
                .features
                .disable(Feature::BackgroundPaginatedRolloutMigration)
                .expect("fixture retains its canonical legacy source");
            config.model_auto_compact_token_limit = Some(100_000);
        })
        .build_with_auto_env(&server)
        .await?;
    let environments = test.codex.environment_selections().await;
    let original = test
        .thread_manager
        .start_thread(StartThreadOptions {
            history_mode: Some(ThreadHistoryMode::Legacy),
            environments: Some(environments.clone()),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    original
        .thread
        .append_rollout_items(&[RolloutItem::Compacted(CompactedItem {
            message: "historic media checkpoint".to_string(),
            replacement_history: Some(vec![
                ResponseItem::Message {
                    id: Some(ResponseItemId::with_suffix("msg", "historic-user")),
                    role: "user".to_string(),
                    content: vec![
                        ContentItem::InputText {
                            text: "KEEP_HISTORIC_TEXT".to_string(),
                        },
                        ContentItem::InputImage {
                            image: ImageReference::Inline {
                                image_url: "data:image/png;base64,AUDIT_MEDIA_SENTINEL".to_string(),
                            },
                            detail: None,
                        },
                    ],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }
                .into(),
            ]),
            window_number: Some(3),
            ..Default::default()
        })])
        .await?;
    let rollout_path = original.thread.rollout_path().context("rollout path")?;
    original.thread.shutdown_and_wait().await?;
    test.thread_manager.remove_thread(&original.thread_id).await;
    let audit_bytes = tokio::fs::read(&rollout_path).await?;

    for iteration in 0..2 {
        let (history, _, _) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
        let resumed = test
            .thread_manager
            .start_thread(StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Legacy),
                environments: Some(environments.clone()),
                initial_history: InitialHistory::Resumed(ResumedHistory {
                    conversation_id: original.thread_id,
                    history: Arc::new(history),
                    rollout_path: Some(rollout_path.clone()),
                }),
                ..StartThreadOptions::new(test.config.clone())
            })
            .await?;
        let mock = responses::mount_sse_once(
            &server,
            responses::sse(vec![
                responses::ev_assistant_message("answer", "done"),
                responses::ev_completed(&format!("resume-{iteration}")),
            ]),
        )
        .await;
        resumed
            .thread
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "continue".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        wait_for_event(&resumed.thread, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        let input = serde_json::to_string(&mock.single_request().input())?;
        assert!(input.contains("KEEP_HISTORIC_TEXT"));
        assert!(input.contains("<compacted_image_omission>"));
        assert!(!input.contains("AUDIT_MEDIA_SENTINEL"));
        resumed.thread.shutdown_and_wait().await?;
        test.thread_manager.remove_thread(&resumed.thread_id).await;
        let bytes = tokio::fs::read(&rollout_path).await?;
        assert_eq!(&bytes[..audit_bytes.len()], audit_bytes.as_slice());
        let (history, _, _) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
        let repairs = history
            .iter()
            .filter_map(|item| match item {
                RolloutItem::Compacted(checkpoint)
                    if checkpoint.replacement_history_media_repair =>
                {
                    Some(checkpoint)
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(repairs.len(), 1);
        assert_eq!(repairs[0].window_number, Some(3));
    }
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
