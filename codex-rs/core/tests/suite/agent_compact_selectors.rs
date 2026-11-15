//! Native V2 reuses root-scoped aliases without changing path or host-controller authority.

use anyhow::Context;
use anyhow::Result;
use codex_core::TurnInputRequest;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_core::config::MultiAgentMessageDelivery;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use test_case::test_case;

async fn fixture(server: &wiremock::MockServer, v2: bool) -> Result<TestCodex> {
    test_codex()
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("native agent tools");
            config
                .features
                .enable(Feature::Sqlite)
                .expect("durable aliases");
            config
                .features
                .disable(Feature::WakeOnAgentMessages)
                .expect("passive message delivery");
            config
                .features
                .set_enabled(Feature::MultiAgentV2, v2)
                .expect("selected agent version");
            config.agent_max_depth = 2;
            config.multi_agent_v2.tool_namespace = Some("collaboration".to_string());
            config.multi_agent_v2.message_delivery = MultiAgentMessageDelivery::EncryptedWithAudit;
        })
        .build_with_auto_env(server)
        .await
}

#[test_case(false; "native v1")]
#[test_case(true; "native v2")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_aliases_are_root_scoped_and_do_not_resume_closed_agents(v2: bool) -> Result<()> {
    let server = responses::start_mock_server().await;
    let test = fixture(&server, v2).await?;
    let root = &test.codex;
    let child = root
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("worker".to_string()),
            ..Default::default()
        })
        .await?;
    let child_thread = test
        .thread_manager
        .get_thread(child.target_thread_id)
        .await?;
    assert_eq!(
        child_thread.multi_agent_version(),
        Some(if v2 {
            MultiAgentVersion::V2
        } else {
            MultiAgentVersion::V1
        })
    );
    let compact = format!("ref:{}", child.agent_ref.context("durable child ref")?);
    let exact = format!("id:{}", child.target_thread_id);
    for selector in [&compact, &exact, "task:worker"] {
        assert_eq!(
            root.resolve_agent_reference(selector).await?,
            child.target_thread_id
        );
    }
    assert_eq!(
        child_thread.resolve_agent_reference("nick:Main").await?,
        test.session_configured.thread_id
    );
    if let Some(nickname) = &child.nickname {
        assert_eq!(
            root.resolve_agent_reference(&format!("nick:{nickname}"))
                .await?,
            child.target_thread_id
        );
    }
    if v2 {
        let path = child_thread
            .session_source()
            .get_agent_path()
            .context("V2 runtime path")?;
        assert_eq!(
            root.resolve_agent_reference(path.as_str()).await?,
            child.target_thread_id
        );
        assert_eq!(
            root.resolve_agent_reference(path.name()).await?,
            child.target_thread_id
        );
        assert_eq!(
            child_thread.resolve_agent_reference("/root").await?,
            test.session_configured.thread_id
        );
        assert!(child_thread.resolve_agent_reference("..").await.is_err());
    }
    for invalid in ["ref:0", "ref:999999", "nick:", "task:/root/no-such-task"] {
        assert!(
            root.resolve_agent_reference(invalid).await.is_err(),
            "{invalid}"
        );
    }

    let other_root = test
        .thread_manager
        .start_thread(test.start_thread_options().await)
        .await?;
    let other_child = other_root
        .thread
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("worker".to_string()),
            ..Default::default()
        })
        .await?;
    assert_eq!(other_child.agent_ref, child.agent_ref);
    assert_ne!(other_child.target_thread_id, child.target_thread_id);
    assert_eq!(
        other_root.thread.resolve_agent_reference(&compact).await?,
        other_child.target_thread_id
    );
    assert!(
        root.resolve_agent_reference(&format!("id:{}", other_child.target_thread_id))
            .await
            .is_err()
    );
    assert!(
        other_root
            .thread
            .resolve_agent_reference(&exact)
            .await
            .is_err()
    );

    root.close_agent(&compact, UserAgentResponseHandling::Presentation)
        .await?;
    assert!(
        test.thread_manager
            .get_thread(child.target_thread_id)
            .await
            .is_err()
    );
    assert_eq!(
        root.resolve_agent_reference(&compact).await?,
        child.target_thread_id
    );
    assert!(
        test.thread_manager
            .get_thread(child.target_thread_id)
            .await
            .is_err()
    );
    assert!(
        test.thread_manager
            .get_thread(other_child.target_thread_id)
            .await
            .is_ok()
    );
    other_root.thread.shutdown_and_wait().await?;
    root.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_message_tool_routes_a_compact_ref_without_exposing_readable_audit_to_the_model()
-> Result<()> {
    let server = responses::start_mock_server().await;
    let test = fixture(&server, true).await?;
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            task: Some("message-recipient".to_string()),
            ..Default::default()
        })
        .await?;
    let child_thread = test
        .thread_manager
        .get_thread(child.target_thread_id)
        .await?;
    let target = format!("ref:{}", child.agent_ref.context("durable child ref")?);
    let ciphertext = "opaque-selector-message-705a";
    let audit = "readable-selector-audit-f23c";
    let mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("send-compact"),
                responses::ev_function_call_with_namespace(
                    "compact-send",
                    "collaboration",
                    "send_message",
                    &json!({
                        "target": target, "message": ciphertext, "task_message": audit,
                    })
                    .to_string(),
                ),
                responses::ev_completed("send-compact"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("sender-finished"),
                responses::ev_completed("sender-finished"),
            ]),
            responses::sse(vec![
                responses::ev_response_created("recipient-finished"),
                responses::ev_completed("recipient-finished"),
            ]),
        ],
    )
    .await;
    test.submit_text_turn("Send the specified collaboration message to the existing child; do not create another agent.").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1]
            .function_call_output_text("compact-send")
            .as_deref(),
        Some("")
    );
    child_thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Inspect the queued agent message".to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&child_thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 3);
    let input = requests[2].input();
    let message = input
        .iter()
        .find(|item| item.get("type").and_then(serde_json::Value::as_str) == Some("agent_message"))
        .context("recipient received a typed agent message")?;
    assert!(serde_json::to_string(message)?.contains(ciphertext));
    assert!(!serde_json::to_string(&input)?.contains(audit));
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
