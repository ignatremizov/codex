use anyhow::Result;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_history::RolloutItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;

fn identities(request: &ResponsesRequest) -> Value {
    let messages = request.message_input_texts("developer");
    let blocks = messages
        .iter()
        .filter_map(|text| text.strip_prefix("<agent_identity_context>\n"))
        .map(|text| {
            let json = text
                .strip_suffix("\n</agent_identity_context>")
                .expect("closing identity marker")
                .split_once('\n')
                .expect("identity instructions and JSON")
                .1;
            serde_json::from_str::<Value>(json).expect("identity JSON")
        })
        .collect::<Vec<_>>();
    assert_eq!(blocks.len(), 1, "one current receiving-root mapping");
    blocks[0].clone()
}

#[test_case(ThreadHistoryMode::Legacy, false, false; "legacy_without_environment_context")]
#[test_case(ThreadHistoryMode::Paginated, false, false; "paginated_without_environment_context")]
#[test_case(ThreadHistoryMode::Legacy, true, false; "legacy_with_environment_context")]
#[test_case(ThreadHistoryMode::Paginated, true, false; "paginated_with_environment_context")]
#[test_case(ThreadHistoryMode::Legacy, false, true; "remote_legacy_without_environment_context")]
#[test_case(ThreadHistoryMode::Paginated, false, true; "remote_paginated_without_environment_context")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn durable_aliases_hydrate_sampling_and_compaction_without_rewriting_history(
    history_mode: ThreadHistoryMode,
    include_environment_context: bool,
    remote_compaction: bool,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let requests = responses::mount_sse_sequence(
        &server,
        [
            "child result",
            "first answer",
            "closed answer",
            "compact summary",
            "after compact",
        ]
        .into_iter()
        .filter(|message| !remote_compaction || *message != "compact summary")
        .enumerate()
        .map(|(index, message)| {
            let id = format!("response-{index}");
            responses::sse(vec![
                responses::ev_response_created(&id),
                responses::ev_assistant_message(&format!("message-{index}"), message),
                responses::ev_completed(&id),
            ])
        })
        .collect(),
    )
    .await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(move |config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
            config.include_environment_context = include_environment_context;
            config
                .features
                .disable(Feature::RemoteCompactionV2)
                .expect("legacy remote API");
            if remote_compaction {
                config
                    .features
                    .enable(Feature::RemoteCompaction)
                    .expect("remote compaction");
            } else {
                config
                    .features
                    .disable(Feature::RemoteCompaction)
                    .expect("local compaction");
            }
            config
                .features
                .disable(Feature::TokenBudget)
                .expect("ordinary compaction");
        })
        .build_with_auto_env(&server)
        .await?;
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("identity-task".to_string()),
            input: Some(vec![UserInput::Text {
                text: "produce the child result".to_string(),
                text_elements: Vec::new(),
            }]),
            response_handling: UserAgentResponseHandling::Passive,
            ..Default::default()
        })
        .await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::ItemCompleted(event)
            if event.item.is_sub_agent_completion_presentation()
                && serde_json::to_string(&event.item).is_ok_and(|text| text.contains("child result")))
    })
    .await;
    test.submit_turn("inspect the live child").await?;
    let first = requests.requests()[1].clone();
    let mut expected = json!([
        {"ref":"1", "nickname":"Main", "task_path":"/root", "state":"active"},
        {
            "ref": child.agent_ref.expect("durable child ref").to_string(),
            "nickname": child.nickname,
            "task_path": child.task_path,
            "state":"active",
        },
    ]);
    assert_eq!(identities(&first), expected);
    assert!(
        !first
            .message_input_texts("user")
            .iter()
            .any(|text| text.contains("<subagents>")),
        "V1 aliases must not be duplicated inside environment context",
    );
    let body = first.body_json();
    let envelope = body["input"]
        .as_array()
        .expect("request input")
        .iter()
        .filter(|item| item["type"] == "agent_message")
        .flat_map(|item| item["content"].as_array().expect("agent content"))
        .filter_map(|content| content["text"].as_str())
        .find(|text| text.contains("<subagent_notification>") && text.contains("child result"))
        .expect("projected child notification");
    assert!(!envelope.contains(&child.target_thread_id.to_string()));
    assert!(envelope.contains(&format!(
        "\"ref\":\"{}\"",
        child.agent_ref.expect("durable child ref"),
    )));
    test.codex.flush_rollout().await?;
    let history = test
        .thread_store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: test.session_configured.thread_id,
            include_archived: false,
        })
        .await?;
    let canonical = history
        .items
        .iter()
        .find_map(|item| {
            let RolloutItem::ResponseItem(envelope) = item else {
                return None;
            };
            let ResponseItem::AgentMessage { content, .. } = &envelope.item else {
                return None;
            };
            content.iter().find_map(|content| match content {
                AgentMessageInputContent::InputText { text }
                    if text.contains("<subagent_notification>")
                        && text.contains("child result") =>
                {
                    Some(envelope.item.clone())
                }
                _ => None,
            })
        })
        .expect("durable canonical notification");
    assert!(serde_json::to_string(&canonical)?.contains(&child.target_thread_id.to_string()));
    let compact_mock = if remote_compaction {
        Some(
            responses::mount_compact_json_once(
                &server,
                json!({"output": [
                    canonical.clone(),
                    ResponseItem::Compaction {
                        id: None,
                        encrypted_content: "identity-compaction".to_string(),
                        internal_chat_message_metadata_passthrough: None,
                    },
                ]}),
            )
            .await,
        )
    } else {
        None
    };
    test.codex
        .close_agent(
            &child.target_thread_id.to_string(),
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    test.submit_turn("inspect the closed child").await?;
    expected[1]["state"] = json!("closed");
    assert_eq!(identities(&requests.requests()[2]), expected);
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    if let Some(compact_mock) = &compact_mock {
        let request = compact_mock.single_request();
        assert_eq!(identities(&request), expected);
        assert!(
            request.input().contains(&serde_json::to_value(&canonical)?),
            "legacy remote compaction must retain exact canonical agent payloads",
        );
    } else {
        assert_eq!(identities(&requests.requests()[3]), expected);
    }
    test.submit_turn("inspect identities after compaction")
        .await?;
    let after = requests
        .requests()
        .last()
        .expect("post-compaction request")
        .clone();
    assert_eq!(identities(&after), expected);
    if remote_compaction {
        let mut projected = serde_json::to_value(&canonical)?;
        projected["content"][0]["text"] = json!(format!(
            "<subagent_notification>\n{}\n</subagent_notification>",
            json!({
                "ref": child.agent_ref.expect("durable child ref").to_string(),
                "status": {"completed": "child result"},
            }),
        ));
        assert!(
            after.input().contains(&projected),
            "canonical remote-retained notification can be projected on the next sample",
        );
    }
    Ok(())
}

#[tokio::test]
async fn root_only_sampling_omits_identity_hydration() -> Result<()> {
    let server = responses::start_mock_server().await;
    let request = responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_assistant_message("root-answer", "done"),
            responses::ev_completed("root-response"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("ordinary root-only turn").await?;
    assert!(
        !request
            .single_request()
            .message_input_texts("developer")
            .iter()
            .any(|text| text.contains("<agent_identity_context>"))
    );
    Ok(())
}
