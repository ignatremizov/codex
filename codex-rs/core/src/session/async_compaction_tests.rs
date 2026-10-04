//! Cancellation prevents new speculation without cancelling session-owned work.

use super::*;
use codex_analytics::CompactionReason;
use codex_analytics::CompactionTrigger;
use codex_login::CodexAuth;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::TokenUsage;
use core_test_support::responses;
use pretty_assertions::assert_eq;

#[test_case::test_case(CompactionTrigger::Auto, false; "optional ready adoption")]
#[test_case::test_case(CompactionTrigger::Manual, false; "manual ready reuse")]
#[test_case::test_case(CompactionTrigger::Auto, true; "optional ready adoption with decoder")]
#[test_case::test_case(CompactionTrigger::Manual, true; "manual ready reuse with decoder")]
#[tokio::test]
async fn cancelled_turn_skips_launch_but_preserves_existing_work(
    trigger: CompactionTrigger,
    decode_handoff: bool,
) {
    let phase = CompactionPhase::MidTurn;
    let server = responses::start_mock_server().await;
    let mut provider =
        codex_model_provider_info::built_in_model_providers(/*openai_base_url*/ None)["openai"]
            .clone();
    provider.base_url = Some(format!("{}/v1", server.uri()));
    provider.supports_websockets = false;
    let server_uri = server.uri();
    let (sess, turn, events) =
        crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
            Vec::new(),
            move |config| {
                config.model = Some("gpt-5.2".to_string());
                config.model_context_window = Some(100_000);
                config.model_auto_compact_token_limit = Some(90_000);
                config.model_provider = provider;
                config.chatgpt_base_url = server_uri;
                config.remote_compaction_handoff_enabled = decode_handoff;
                config.features.enable(Feature::AsyncCompaction).unwrap();
                config.features.disable(Feature::TokenBudget).unwrap();
            },
        )
        .await;
    let mut replies = vec![responses::sse(vec![
        serde_json::json!({"type":"response.output_item.done", "item": {
            "type":"compaction", "encrypted_content":"prepared-summary"
        }}),
        responses::ev_completed("prepared"),
    ])];
    if decode_handoff {
        replies.push(responses::sse(vec![
            responses::ev_assistant_message("decoded", "Prepared handoff"),
            responses::ev_completed("decoder"),
        ]));
    }
    let request = responses::mount_sse_sequence(&server, replies).await;
    let turn_cancellation = CancellationToken::new();
    let step = sess
        .capture_step_context(Arc::clone(&turn), &turn_cancellation)
        .await
        .expect("capture step");
    let original: ResponseItem = serde_json::from_value(serde_json::json!({
        "type": "message", "role": "user",
        "content": [{"type": "input_text", "text": "original turn"}]
    }))
    .expect("user message");
    sess.replace_history(vec![original], /*reference_context_item*/ None)
        .await;
    sess.update_token_usage_info(
        &turn,
        Some(&TokenUsage {
            input_tokens: 70_000,
            total_tokens: 70_000,
            ..Default::default()
        }),
    )
    .await
    .expect("record usage");
    let status = super::super::context_window::context_window_token_status(&sess, &turn).await;
    let world_state = Arc::new(WorldState::default());
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let auth = sess.services.model_client.auth_owner_generation();
    maybe_start(
        &sess,
        &step,
        &world_state,
        phase,
        &cancelled,
        &status,
        (Some("routing".to_string()), auth),
    )
    .await;
    assert!(
        sess.state
            .lock()
            .await
            .async_compaction
            .last_attempt
            .is_none()
    );
    assert_eq!(request.requests().len(), 0);

    // The same eligible work can launch on a live turn, then outlive its cancellation.
    maybe_start(
        &sess,
        &step,
        &world_state,
        phase,
        &turn_cancellation,
        &status,
        (Some("routing".to_string()), auth),
    )
    .await;
    turn_cancellation.cancel();
    maybe_start(
        &sess,
        &step,
        &world_state,
        phase,
        &turn_cancellation,
        &status,
        (Some("routing".to_string()), auth),
    )
    .await;
    // Wait for client publication before exercising either ready-result consumer.
    wait_for_background_compaction_for_test(&sess).await;
    let next_step = sess
        .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
        .await
        .expect("capture next step");
    match trigger {
        CompactionTrigger::Auto => {
            let candidate = take_ready(&sess, &turn).await.expect("published candidate");
            let mut client_session = sess.services.model_client.new_session();
            assert!(
                crate::compact_remote_v2::run_inline_remote_auto_compact_task(
                    Arc::clone(&sess),
                    Arc::clone(&next_step),
                    next_step,
                    &mut client_session,
                    world_state,
                    crate::compact_remote_v2::AutoCompactRun {
                        reason: CompactionReason::ContextLimit,
                        phase,
                        cancellation: &CancellationToken::new(),
                    },
                    Some(candidate),
                    crate::compact_remote_v2::CompactionFallback::Skip,
                )
                .await
                .expect("optional adoption")
            );
        }
        CompactionTrigger::Manual => {
            crate::compact_remote_v2::run_remote_compact_task(
                Arc::clone(&sess),
                next_step,
                world_state,
                &CancellationToken::new(),
            )
            .await
            .expect("manual compaction reuses the completed request");
        }
    }
    let history = sess.clone_history().await;
    let summaries = history
        .raw_items()
        .filter_map(|item| match item {
            ResponseItem::Compaction {
                encrypted_content, ..
            } => Some(encrypted_content.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(summaries, ["prepared-summary"]);
    assert_eq!(request.requests().len(), if decode_handoff { 2 } else { 1 });
    let completion = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event.msg {
            codex_protocol::protocol::EventMsg::ItemCompleted(completed) => match completed.item {
                codex_protocol::items::TurnItem::ContextCompaction(item) => Some(item),
                _ => None,
            },
            _ => None,
        })
        .expect("prepared compaction completed");
    assert_eq!(
        completion.message,
        decode_handoff.then(|| "Prepared handoff".to_string()),
    );
    shutdown(&sess).await;
}
