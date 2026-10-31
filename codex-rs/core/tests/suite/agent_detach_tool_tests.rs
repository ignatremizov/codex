//! Detachment through close_agent preserves the target and refuses active work or adoption.

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::UserAgentFinalResponseHandling;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_extension_items::ExtensionItem;
use codex_features::Feature;
use codex_features::SleepToolMode;
use codex_protocol::CollabAgentLifecycle;
use codex_protocol::CollabAgentLifecycleAction;
use codex_protocol::ThreadId;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::items::TurnItem;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use test_case::test_case;
use wiremock::MockServer;

fn handling() -> UserAgentResponseHandling {
    UserAgentResponseHandling::from_parts(
        /*commentary*/ false,
        UserAgentFinalResponseHandling::None,
        /*queue_input*/ false,
    )
}

async fn fixture(history_mode: ThreadHistoryMode) -> Result<(MockServer, TestCodex, ThreadId)> {
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_model_info_override("gpt-5.2", |model| model.tool_mode = Some(ToolMode::Direct))
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("enable agent tools");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("use V1");
            config
                .features
                .enable(Feature::SleepTool)
                .expect("enable sleep gate");
            config.sleep_tool_mode = SleepToolMode::AlwaysOn;
        })
        .build_with_auto_env(&server)
        .await?;
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: handling(),
            ..Default::default()
        })
        .await?
        .target_thread_id;
    Ok((server, test, child))
}

fn finish(id: &str, text: &str) -> String {
    responses::sse(vec![
        responses::ev_response_created(id),
        responses::ev_assistant_message(&format!("{id}-message"), text),
        responses::ev_completed(id),
    ])
}

async fn lifecycle_operation(
    test: &TestCodex,
    server: &MockServer,
    call_id: &str,
    tool: &str,
    arguments: Value,
) -> Result<(String, Vec<CollabAgentToolCallItem>)> {
    let captured = responses::mount_sse_sequence(
        server,
        vec![
            responses::sse(vec![
                responses::ev_response_created(call_id),
                responses::ev_function_call_with_namespace(
                    call_id,
                    "multi_agent_v1",
                    tool,
                    &arguments.to_string(),
                ),
                responses::ev_completed(call_id),
            ]),
            finish(&format!("{call_id}-done"), "Done."),
        ],
    )
    .await;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Apply the requested child lifecycle operation.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut items = Vec::new();
    wait_for_event(&test.codex, |event| {
        let item = match event {
            EventMsg::ItemStarted(event) => Some(&event.item),
            EventMsg::ItemCompleted(event) => Some(&event.item),
            EventMsg::TurnComplete(_) => return true,
            _ => None,
        };
        if let Some(TurnItem::CollabAgentToolCall(item)) = item
            && item.id == call_id
        {
            items.push(item.clone());
        }
        false
    })
    .await;
    let output = captured
        .requests()
        .iter()
        .find_map(|request| request.function_call_output_text(call_id))
        .context("recorded lifecycle result")?;
    Ok((output, items))
}

async fn close(
    test: &TestCodex,
    server: &MockServer,
    call_id: &str,
    arguments: Value,
) -> Result<String> {
    Ok(
        lifecycle_operation(test, server, call_id, "close_agent", arguments)
            .await?
            .0,
    )
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_flag_preserves_history_and_requires_explicit_uuid_reattachment(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, test, child_id) = fixture(history_mode).await?;
    let child = test.thread_manager.get_thread(child_id).await?;
    responses::mount_sse_once(&server, finish("seed-child", "KEEP_DETACH_MARKER")).await;
    child
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Remember this conversation.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    let sibling = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: handling(),
            ..Default::default()
        })
        .await?
        .target_thread_id;
    let (output, detach_items) = lifecycle_operation(
        &test,
        &server,
        "detach-child",
        "close_agent",
        json!({
            "target": "2", "detach": true
        }),
    )
    .await?;
    let output: Value = serde_json::from_str(&output)?;
    let original_identity = detach_items
        .first()
        .and_then(|item| item.receiver_agents.first())
        .context("detaching child identity")?
        .clone();
    assert_eq!(
        (
            original_identity.thread_id,
            original_identity.agent_ref.as_deref()
        ),
        (child_id, Some("2"))
    );
    assert_eq!(
        detach_items
            .iter()
            .map(|item| (
                item.status,
                item.lifecycle.clone(),
                item.receiver_agents.clone()
            ))
            .collect::<Vec<_>>(),
        [
            CollabAgentToolCallStatus::InProgress,
            CollabAgentToolCallStatus::Completed,
        ]
        .map(|status| (
            status,
            Some(CollabAgentLifecycle {
                action: CollabAgentLifecycleAction::Detach,
                root_thread_id: test.session_configured.thread_id,
            }),
            vec![original_identity.clone()],
        ))
        .to_vec()
    );
    assert_eq!(
        output["detached"],
        json!({
            "thread_id": child_id,
            "previous_root_thread_id": test.session_configured.thread_id,
            "unloaded_thread_ids": [child_id],
        })
    );
    assert!(test.thread_manager.get_thread(child_id).await.is_err());
    assert!(test.thread_manager.get_thread(sibling).await.is_ok());
    let refused = close(
        &test,
        &server,
        "no-implicit-adoption",
        json!({
            "target": child_id, "detach": true
        }),
    )
    .await?;
    assert!(
        refused.contains("transferred out of this root")
            || refused.contains("not controlled by this root"),
        "{refused}"
    );
    assert!(test.thread_manager.get_thread(child_id).await.is_err());
    let (_, resume_items) = lifecycle_operation(
        &test,
        &server,
        "reattach-child",
        "resume_agent",
        json!({"id": child_id, "task": "reattached"}),
    )
    .await?;
    let mut reattached_identity = original_identity.clone();
    reattached_identity.task_path = Some("/root/reattached".into());
    assert_eq!(
        resume_items
            .iter()
            .map(|item| (
                item.status,
                item.lifecycle.clone(),
                item.receiver_agents.clone()
            ))
            .collect::<Vec<_>>(),
        vec![
            (
                CollabAgentToolCallStatus::InProgress,
                Some(CollabAgentLifecycle {
                    action: CollabAgentLifecycleAction::Reattach,
                    root_thread_id: test.session_configured.thread_id,
                }),
                vec![original_identity],
            ),
            (
                CollabAgentToolCallStatus::Completed,
                Some(CollabAgentLifecycle {
                    action: CollabAgentLifecycleAction::Reattach,
                    root_thread_id: test.session_configured.thread_id,
                }),
                vec![reattached_identity],
            ),
        ]
    );
    let restored = test.thread_manager.get_thread(child_id).await?;
    let recalled = responses::mount_sse_once(&server, finish("recall-child", "Retained.")).await;
    restored
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Recall the earlier marker.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&restored, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert!(
        recalled
            .single_request()
            .body_json()
            .to_string()
            .contains("KEEP_DETACH_MARKER")
    );
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn detach_refuses_busy_children_and_response_replay_flags(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let (server, test, child_id) = fixture(history_mode).await?;
    let invalid = close(
        &test,
        &server,
        "detach-with-replay",
        json!({
            "target": child_id, "detach": true, "w": "x"
        }),
    )
    .await?;
    assert!(
        invalid.contains("does not accept response replay flags"),
        "{invalid}"
    );
    let child = test.thread_manager.get_thread(child_id).await?;
    responses::mount_sse_once(
        &server,
        responses::sse(vec![
            responses::ev_response_created("child-working"),
            responses::ev_function_call_with_namespace(
                "work-gate",
                "clock",
                "sleep",
                &json!({"duration_ms": 60000}).to_string(),
            ),
            responses::ev_completed("child-working"),
        ]),
    )
    .await;
    child
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Wait for more input.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&child, |event| {
        matches!(event, EventMsg::ItemStarted(event)
            if matches!(&event.item, TurnItem::Extension(ExtensionItem::Sleep(item)) if item.id == "work-gate"))
    }).await;
    let refused = close(
        &test,
        &server,
        "detach-busy",
        json!({
            "target": child_id, "detach": true
        }),
    )
    .await?;
    assert!(refused.contains("active or queued work"), "{refused}");
    assert!(test.thread_manager.get_thread(child_id).await.is_ok());
    test.codex
        .interrupt_agent(&child_id.to_string(), /*follow_up*/ None)
        .await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnAborted(_))).await;
    Ok(())
}
