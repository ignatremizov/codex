//! The remote checkpoint remains authoritative; its isolated decoder supplies presentation only.

use anyhow::Context;
use anyhow::Result;
use codex_config::test_support::CloudConfigBundleFixture;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_core::config::CurrentTimeReminderConfig;
use codex_core::config::RolloutBudgetConfig;
use codex_extension_api::ContextContributor;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::PromptFragment;
use codex_extension_api::TurnContextContributionInput;
use codex_features::Feature;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_login::CodexAuth;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ContentItemKind;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::CONTEXT_COMPACTION_DECODING_MESSAGE;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use codex_rollout::RolloutRecorder;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use test_case::test_case;
use tokio::time::timeout;
use wiremock::ResponseTemplate;

const PROMPT: &str =
    "Repeat the compacted handoff content verbatim. Do not summarize, explain, or add any text.";
const INVENTORY: &str = "<skills_instructions>\n### Available skills\n- retained: retained skill\n</skills_instructions>";

#[derive(Clone, Copy)]
enum DecoderOutcome {
    Disabled,
    Primary,
    Fallback,
    Failed,
}

#[test_case(DecoderOutcome::Disabled; "ordinary builder disables decoder")]
#[test_case(DecoderOutcome::Primary; "primary succeeds")]
#[test_case(DecoderOutcome::Fallback; "fallback succeeds")]
#[test_case(DecoderOutcome::Failed; "both fail without undoing compaction")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_checkpoint_and_retained_inventory_are_independent_of_decoder(
    outcome: DecoderOutcome,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let mut replies = vec![
        responses::sse(vec![
            responses::ev_assistant_message("seed", "before compaction"),
            responses::ev_completed("seed"),
        ]),
        responses::sse(vec![
            json!({
                "type": "response.output_item.done",
                "item": {"type": "compaction", "encrypted_content": "CANONICAL_CHECKPOINT"},
            }),
            responses::ev_completed("compact"),
        ]),
    ];
    let decoder_count = match outcome {
        DecoderOutcome::Disabled => 0,
        DecoderOutcome::Primary => {
            replies.push(responses::sse(vec![
                responses::ev_assistant_message("primary", "DECODED_PRESENTATION"),
                responses::ev_completed("primary"),
            ]));
            1
        }
        DecoderOutcome::Fallback | DecoderOutcome::Failed => {
            // Prompt echo is not a decoded handoff and must use the fallback.
            replies.push(responses::sse(vec![
                responses::ev_assistant_message("echo", PROMPT),
                responses::ev_completed("primary"),
            ]));
            let mut fallback = Vec::new();
            if matches!(outcome, DecoderOutcome::Fallback) {
                fallback.push(responses::ev_assistant_message(
                    "fallback",
                    "DECODED_PRESENTATION",
                ));
            }
            fallback.push(responses::ev_completed("fallback"));
            replies.push(responses::sse(fallback));
            2
        }
    };
    replies.push(responses::sse(vec![responses::ev_completed("followup")]));
    let mock = responses::mount_sse_sequence(&server, replies).await;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_cloud_config_bundle(
            CloudConfigBundleFixture::loader_with_enterprise_requirement(
                "additional_developer_instructions = \"PARENT_MANAGED_INSTRUCTIONS\"",
            ),
        )
        .with_model_info_override("gpt-5.5", |model| {
            model.supports_reasoning_effort_updates = true;
        })
        .with_model_info_override("gpt-5.4", |model| {
            model.supports_reasoning_effort_updates = true;
        })
        .with_config(move |config| {
            config.model = Some("gpt-5.5".to_string());
            // Leave the shared builder's default untouched in the disabled case.
            if !matches!(outcome, DecoderOutcome::Disabled) {
                config.remote_compaction_handoff_enabled = true;
            }
            config.remote_compaction_handoff_model = Some("gpt-5.5".to_string());
            config.remote_compaction_handoff_fallback_model = Some("gpt-5.4".to_string());
            config.base_instructions = Some("PARENT_BASE_INSTRUCTIONS".to_string());
            config.base_instructions_provenance =
                Some(codex_protocol::models::BaseInstructionsProvenance::Custom);
            config
                .features
                .enable(Feature::RetainClientDeveloperMessages)
                .expect("retain client-authored skill catalog");
            config
                .features
                .enable(Feature::CurrentTimeReminder)
                .expect("enable parent time reminders");
            config.current_time_reminder = Some(CurrentTimeReminderConfig {
                reminder_interval_seconds: 0,
                ..Default::default()
            });
            config
                .features
                .enable(Feature::ReasoningEffortOverride)
                .expect("enable parent reasoning overrides");
            config.model_reasoning_effort =
                Some(codex_protocol::openai_models::ReasoningEffort::High);
            config.rollout_budget = Some(RolloutBudgetConfig {
                limit_tokens: 1_000_000,
                reminder_at_remaining_tokens: vec![500_000],
                sampling_token_weight: 1.0,
                prefill_token_weight: 1.0,
            });
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .inject_response_items(vec![ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: INVENTORY.to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }])
        .await?;
    test.submit_turn("seed").await?;
    test.codex.submit(Op::Compact).await?;
    let (started, statuses, completed) = timeout(Duration::from_secs(/*secs*/ 30), async {
        let mut started = None;
        let mut statuses = Vec::new();
        loop {
            let event = test.codex.next_event().await?;
            match event.msg {
                EventMsg::ItemStarted(item)
                    if matches!(item.item, TurnItem::ContextCompaction(_)) =>
                {
                    started = Some(item);
                }
                EventMsg::ContextCompactionStatus(status) => {
                    let started = started.as_ref().context("status follows item start")?;
                    assert_eq!(event.id, started.turn_id);
                    assert_eq!(status.item_id, started.item.id());
                    statuses.push(status.message);
                }
                EventMsg::ItemCompleted(item)
                    if matches!(item.item, TurnItem::ContextCompaction(_)) =>
                {
                    break Ok::<_, anyhow::Error>((started, statuses, item));
                }
                _ => {}
            }
        }
    })
    .await
    .context("compaction completed")??;
    let started = started.context("compaction started")?;
    assert_eq!(
        (completed.thread_id, &completed.turn_id, completed.item.id()),
        (started.thread_id, &started.turn_id, started.item.id()),
    );
    assert_eq!(
        statuses,
        if matches!(outcome, DecoderOutcome::Disabled) {
            Vec::new()
        } else {
            vec![CONTEXT_COMPACTION_DECODING_MESSAGE.to_string()]
        },
    );
    let TurnItem::ContextCompaction(compacted) = completed.item else {
        unreachable!()
    };
    assert_eq!(
        serde_json::to_value(&compacted)?,
        serde_json::to_value(ContextCompactionItem {
            id: compacted.id.clone(),
            summary: None,
            message: matches!(outcome, DecoderOutcome::Primary | DecoderOutcome::Fallback)
                .then(|| "DECODED_PRESENTATION".to_string()),
            available_skills: vec!["retained".to_string()],
            decode_error: matches!(outcome, DecoderOutcome::Failed).then(|| {
                "decoder model `gpt-5.5` failed: decoder returned no usable text\ndecoder model `gpt-5.4` failed: decoder returned no text".to_string()
            }),
        })?,
    );
    let terminal = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(terminal) = terminal else {
        unreachable!()
    };
    assert_eq!(terminal.error, None);
    test.submit_text_turn("after compaction").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3 + decoder_count);
    assert!(requests[0].body_contains_text("PARENT_MANAGED_INSTRUCTIONS"));
    assert!(requests[0].body_contains_text("<current_time_reminder>"));
    assert!(requests[0].body_contains_text("<rollout_budget>"));
    assert!(
        !requests[0]
            .inputs_of_type("configuration_update")
            .is_empty()
    );
    assert_eq!(requests[1].inputs_of_type("compaction_trigger").len(), 1);
    for (offset, decoder) in requests[2..2 + decoder_count].iter().enumerate() {
        let body = decoder.body_json();
        assert_eq!(
            body["model"],
            if offset == 0 { "gpt-5.5" } else { "gpt-5.4" }
        );
        assert_eq!(body["instructions"], PROMPT);
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["client_metadata"]["x-openai-subagent"], "compact");
        assert_eq!(decoder.inputs_of_type("compaction").len(), 1);
        assert!(decoder.inputs_of_type("compaction_trigger").is_empty());
        assert!(!decoder.body_contains_text("PARENT_BASE_INSTRUCTIONS"));
        assert!(!decoder.body_contains_text("PARENT_MANAGED_INSTRUCTIONS"));
        assert!(!decoder.body_contains_text("<current_time_reminder>"));
        assert!(!decoder.body_contains_text("<rollout_budget>"));
        assert!(decoder.inputs_of_type("configuration_update").is_empty());
        assert!(!decoder.body_contains_text("<environment_context>"));
        assert!(decoder.body_contains_text(INVENTORY));
    }
    let followup = requests.last().context("follow-up request")?;
    assert_eq!(
        followup.inputs_of_type("compaction")[0]["encrypted_content"],
        "CANONICAL_CHECKPOINT"
    );
    assert!(!followup.body_contains_text("DECODED_PRESENTATION"));
    assert!(!followup.body_contains_text(PROMPT));

    let rollout_path = test.codex.rollout_path().context("persisted rollout")?;
    test.codex.shutdown_and_wait().await?;
    let checkpoint = std::fs::read_to_string(&rollout_path)?
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .filter_map(|line| match line.item {
            RolloutItem::Compacted(compacted) => compacted.replacement_history,
            _ => None,
        })
        .next_back()
        .context("canonical checkpoint")?;
    assert!(checkpoint.iter().any(|envelope| matches!(
        &envelope.item, ResponseItem::Compaction { encrypted_content, id: Some(_), .. }
            if encrypted_content == "CANONICAL_CHECKPOINT"
    )));
    let checkpoint_items = checkpoint
        .iter()
        .map(|envelope| serde_json::to_value(&envelope.item))
        .collect::<serde_json::Result<Vec<_>>>()?;
    // Inspect the supported persisted representation, including harness metadata,
    // independently of the response-only items sent to the decoder.
    let checkpoint_records = checkpoint
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    assert!(!serde_json::to_string(&checkpoint_records)?.contains("DECODED_PRESENTATION"));
    for decoder in &requests[2..2 + decoder_count] {
        let input = decoder.input();
        assert_eq!(input.len(), checkpoint.len() + 1);
        assert_eq!(
            input[..checkpoint.len()],
            checkpoint_items,
            "decoder must use the exact installed items, including assigned IDs",
        );
        assert_eq!(input[checkpoint.len()]["role"], "developer");
        assert_eq!(input[checkpoint.len()]["content"][0]["text"], PROMPT);
    }

    // Reconstruct a fresh runtime from disk, retaining the selected executor identities.
    let environments = test.codex.environment_selections().await;
    let (history, _, _) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
    assert!(!history.iter().any(|item| matches!(
        item,
        RolloutItem::EventMsg(EventMsg::ContextCompactionStatus(_))
    )));
    let persisted = history.iter().find_map(|item| match item {
        RolloutItem::EventMsg(EventMsg::ItemCompleted(completed)) => match &completed.item {
            TurnItem::ContextCompaction(item) => Some(item),
            _ => None,
        },
        _ => None,
    });
    assert_eq!(
        serde_json::to_value(persisted.context("durable compaction item")?)?,
        serde_json::to_value(&compacted)?,
    );
    let resumed = test
        .thread_manager
        .start_thread(StartThreadOptions {
            environments: Some(environments),
            initial_history: InitialHistory::Resumed(ResumedHistory {
                conversation_id: test.session_configured.thread_id,
                history: Arc::new(history),
                rollout_path: Some(rollout_path),
            }),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    let cold = responses::mount_sse_once(
        &server,
        responses::sse(vec![responses::ev_completed("cold-followup")]),
    )
    .await;
    resumed
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "cold followup".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&resumed.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let cold = cold.single_request();
    assert_eq!(
        cold.inputs_of_type("compaction")[0]["encrypted_content"],
        "CANONICAL_CHECKPOINT"
    );
    assert!(!cold.body_contains_text("DECODED_PRESENTATION"));
    assert!(!cold.body_contains_text(PROMPT));
    assert!(!cold.body_contains_text("decoder model"));
    resumed.thread.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancellation_during_decoder_preserves_installed_checkpoint_and_new_settings() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let replies = vec![
        responses::sse(vec![responses::ev_completed("seed")]),
        responses::sse(vec![
            json!({"type":"response.output_item.done", "item":{
                "type":"compaction", "encrypted_content":"INSTALLED_BEFORE_DECODER"
            }}),
            responses::ev_completed("compact"),
        ]),
        responses::sse(vec![
            responses::ev_assistant_message("late", "MUST_NOT_INSTALL"),
            responses::ev_completed("decoder"),
        ]),
        responses::sse(vec![responses::ev_completed("followup")]),
    ]
    .into_iter()
    .enumerate()
    .map(|(index, body)| {
        let response = ResponseTemplate::new(/*status*/ 200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(body);
        if index == 2 {
            response.set_delay(Duration::from_secs(/*secs*/ 30))
        } else {
            response
        }
    })
    .collect();
    let mock = responses::mount_response_sequence(&server, replies).await;
    let test = test_codex()
        .with_history_mode(ThreadHistoryMode::Paginated)
        .with_auth(CodexAuth::create_dummy_chatgpt_auth_for_testing())
        .with_config(|config| {
            config.remote_compaction_handoff_enabled = true;
            config.remote_compaction_handoff_model = Some("gpt-5.5".to_string());
            config.remote_compaction_handoff_fallback_model = Some("gpt-5.4".to_string());
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_text_turn("seed").await?;
    test.codex.submit(Op::Compact).await?;
    timeout(Duration::from_secs(/*secs*/ 30), async {
        while mock.requests().len() < 3 {
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
    })
    .await
    .context("decoder request reached the server")?;
    test.codex.flush_rollout().await?;
    let rollout_path = test.codex.rollout_path().context("rollout")?;
    let checkpoint = std::fs::read_to_string(&rollout_path)?
        .lines()
        .filter_map(|line| codex_rollout::parse_rollout_line(line).ok())
        .find_map(|line| match line.item {
            RolloutItem::Compacted(checkpoint) => Some(checkpoint),
            _ => None,
        })
        .context("compaction was installed before the stalled decoder")?;
    assert!(serde_json::to_string(&checkpoint)?.contains("INSTALLED_BEFORE_DECODER"));
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            effort: Some(Some(codex_protocol::openai_models::ReasoningEffort::High)),
            ..Default::default()
        },
    )
    .await?;
    test.codex.submit(Op::Interrupt).await?;
    wait_for_event_with_timeout(
        &test.codex,
        |event| matches!(event, EventMsg::TurnAborted(_)),
        Duration::from_secs(/*secs*/ 30),
    )
    .await;
    test.submit_text_turn("after interruption").await?;
    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        4,
        "cancellation must not start the fallback decoder"
    );
    let followup = &requests[3];
    assert_eq!(followup.body_json()["reasoning"]["effort"], "high");
    assert_eq!(
        followup.inputs_of_type("compaction")[0]["encrypted_content"],
        "INSTALLED_BEFORE_DECODER"
    );
    assert!(!followup.body_contains_text("MUST_NOT_INSTALL"));
    test.codex.shutdown_and_wait().await?;
    let (history, _, _) = RolloutRecorder::load_rollout_items(&rollout_path).await?;
    for item in history {
        match item {
            RolloutItem::EventMsg(EventMsg::ContextCompactionStatus(_)) => {
                anyhow::bail!("transient decoding status was persisted");
            }
            RolloutItem::EventMsg(EventMsg::ItemCompleted(completed)) => {
                if let TurnItem::ContextCompaction(item) = completed.item {
                    assert_eq!((item.message, item.decode_error), (None, None));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_midturn_compaction_reports_the_reinjected_inventory() -> Result<()> {
    struct InventoryContributor;
    impl ContextContributor for InventoryContributor {
        fn contribute_turn_context<'a>(
            &'a self,
            _input: TurnContextContributionInput<'a>,
        ) -> ExtensionFuture<'a, Vec<PromptFragment>> {
            Box::pin(async {
                vec![PromptFragment::developer_policy(
                    INVENTORY,
                    ContentItemKind("skills.catalog".to_string()),
                )]
            })
        }
    }
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_function_call(
                    "plan",
                    "update_plan",
                    r#"{"plan":[{"step":"done","status":"completed"}]}"#,
                ),
                responses::ev_completed_with_tokens("plan", /*total_tokens*/ 330_000),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("summary", "LOCAL_HANDOFF"),
                responses::ev_completed_with_tokens("compact", /*total_tokens*/ 200),
            ]),
            responses::sse(vec![responses::ev_completed("final")]),
        ],
    )
    .await;
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.prompt_contributor(Arc::new(InventoryContributor));
    let test = test_codex()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config
                .features
                .disable(Feature::RemoteCompaction)
                .expect("use local compaction");
            config.model_auto_compact_token_limit = Some(200_000);
            config.update_plan_enabled = true;
        })
        .build_with_auto_env(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "make a plan".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let event = wait_for_event_with_timeout(
        &test.codex,
        |event| {
            matches!(
                event, EventMsg::ItemCompleted(completed)
                    if matches!(completed.item, TurnItem::ContextCompaction(_))
            )
        },
        Duration::from_secs(/*secs*/ 30),
    )
    .await;
    let EventMsg::ItemCompleted(completed) = event else {
        unreachable!()
    };
    let TurnItem::ContextCompaction(compacted) = completed.item else {
        unreachable!()
    };
    assert_eq!(compacted.available_skills, vec!["retained".to_string()]);
    assert_eq!(compacted.decode_error, None);
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = requests.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[2].body_contains_text(INVENTORY));
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
