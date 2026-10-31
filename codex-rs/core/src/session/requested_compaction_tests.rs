use super::PendingInputContinuationTask;
use super::make_session_and_context_with_auth_and_config_and_rx;
use anyhow::Context;
use anyhow::Result;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::timeout;

#[test]
fn compaction_accepted_after_the_final_checkpoint_continues_before_task_removal() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(/*val*/ 2)
        .thread_stack_size(/*val*/ 16 * 1024 * 1024)
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let server = responses::start_mock_server().await;
        let captured = responses::mount_sse_sequence(
            &server,
            vec![
                responses::sse(vec![
                    responses::ev_assistant_message("checkpoint", "Finish the original task."),
                    responses::ev_completed("compacted"),
                ]),
                responses::sse(vec![
                    responses::ev_assistant_message("continued", ""),
                    responses::ev_completed("continued"),
                ]),
            ],
        )
        .await;
        let base_url = server.uri();
        let (session, turn, events) = make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("test-api-key"),
            Vec::new(),
            move |config| {
                config.model_provider.base_url = Some(base_url);
                config.model_provider.supports_websockets = false;
                config.features.disable(Feature::RemoteCompaction).unwrap();
                config.features.disable(Feature::TokenBudget).unwrap();
                config.features.disable(Feature::AsyncCompaction).unwrap();
                config.model_post_turn_compact_threshold_percent = 0;
            },
        )
        .await;
        let user_input = ResponseInputItem::from(vec![UserInput::Text {
            text: "Finish the original task.".into(),
            text_elements: Vec::new(),
        }]);
        session
            .replace_history(
                vec![user_input.into()],
                /*reference_context_item*/ None,
            )
            .await;
        let checkpoint = Arc::new(Notify::new());
        let finish = Arc::new(Notify::new());
        session
            .spawn_task(
                Arc::clone(&turn),
                Vec::new(),
                PendingInputContinuationTask {
                    final_pending_input_check_reached: Arc::clone(&checkpoint),
                    allow_initial_run_to_finish: Arc::clone(&finish),
                },
            )
            .await;
        timeout(Duration::from_secs(/*secs*/ 10), checkpoint.notified()).await?;
        super::super::handlers::compact(&session, "late-compact".into()).await;
        assert!(super::super::inline_compaction::is_requested(&turn));
        assert_eq!(captured.requests().len(), 0);
        finish.notify_one();
        let (compacted_turns, complete) = timeout(Duration::from_secs(/*secs*/ 30), async {
            let mut compacted_turns = Vec::new();
            loop {
                let event = events.recv().await?;
                match event.msg {
                    EventMsg::ItemCompleted(event)
                        if matches!(event.item, TurnItem::ContextCompaction(_)) =>
                    {
                        compacted_turns.push(event.turn_id);
                    }
                    EventMsg::TurnComplete(complete) => {
                        return Ok::<_, anyhow::Error>((compacted_turns, complete));
                    }
                    unexpected @ (EventMsg::TurnStarted(_)
                    | EventMsg::TurnAborted(_)
                    | EventMsg::Error(_)) => {
                        anyhow::bail!(
                            "late compaction replaced or failed its turn: {unexpected:?}"
                        );
                    }
                    _ => {}
                }
            }
        })
        .await??;
        assert_eq!(
            (
                compacted_turns,
                complete.turn_id,
                complete.last_agent_message,
                complete.error
            ),
            (
                vec![turn.sub_id.clone()],
                turn.sub_id.clone(),
                Some("answer before the late steer".into()),
                None
            )
        );
        let requests = captured.requests();
        assert_eq!(requests.len(), 2);
        let continued = requests.last().context("continued sampling request")?;
        assert!(
            continued
                .message_input_texts("user")
                .iter()
                .any(|message| message.contains("Finish the original task."))
        );
        assert!(!super::super::inline_compaction::is_requested(&turn));
        Ok(())
    })
}
