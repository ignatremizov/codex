//! Each receiving thread identifies itself through compaction and ownership changes.

use anyhow::Context;
use anyhow::Result;
use codex_core::CodexThread;
use codex_core::TurnInputRequest;
use codex_core::UserAgentForkMode;
use codex_core::UserAgentResponseHandling;
use codex_core::UserAgentSpawnOptions;
use codex_features::Feature;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::user_input::UserInput;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::sync::Arc;
use test_case::test_case;
use wiremock::MockServer;

fn receiver_identity(request: &ResponsesRequest) -> Result<Value> {
    let messages = request.message_input_texts("developer");
    let body = messages
        .iter()
        .rev()
        .find_map(|text| text.strip_prefix("<agent_self>\n"))
        .and_then(|text| text.strip_suffix("\n</agent_self>"))
        .context("current receiver context")?;
    let (_, payload) = body.split_once('\n').context("receiver instructions")?;
    Ok(serde_json::from_str(payload)?)
}

async fn sample(
    thread: &Arc<CodexThread>,
    server: &MockServer,
    id: &str,
) -> Result<ResponsesRequest> {
    let captured = responses::mount_sse_once(
        server,
        responses::sse(vec![
            responses::ev_response_created(id),
            responses::ev_assistant_message(&format!("{id}-message"), "Done."),
            responses::ev_completed(id),
        ]),
    )
    .await;
    thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Report the assigned result.".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    Ok(captured.single_request())
}

#[test_case(ThreadHistoryMode::Legacy, false; "legacy_local")]
#[test_case(ThreadHistoryMode::Paginated, false; "paginated_local")]
#[test_case(ThreadHistoryMode::Legacy, true; "legacy_remote")]
#[test_case(ThreadHistoryMode::Paginated, true; "paginated_remote")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn self_identity_follows_receiver_through_compaction_promotion_and_adoption(
    history_mode: ThreadHistoryMode,
    remote_compaction: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = responses::start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(move |config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("use V1");
            config
                .features
                .disable(Feature::TokenBudget)
                .expect("ordinary context");
            config.include_environment_context = false;
            config.agent_max_depth = 3;
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
        })
        .build_with_auto_env(&server)
        .await?;
    let root = test.session_configured.thread_id;
    let child = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let child_id = child.target_thread_id;
    let thread = test.thread_manager.get_thread(child_id).await?;
    assert_eq!(
        receiver_identity(&sample(&test.codex, &server, "root").await?)?,
        json!({"agent_id": root, "self_ref": "1"}),
    );
    let child_identity = json!({
        "agent_id": child_id,
        "self_ref": child.agent_ref.map(|agent_ref| agent_ref.to_string()),
    });
    assert_eq!(
        receiver_identity(&sample(&thread, &server, "child").await?)?,
        child_identity,
    );
    let grandchild = thread
        .spawn_agent(UserAgentSpawnOptions {
            fork_mode: UserAgentForkMode::All,
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?;
    let grandchild_thread = test
        .thread_manager
        .get_thread(grandchild.target_thread_id)
        .await?;
    assert_eq!(
        receiver_identity(&sample(&grandchild_thread, &server, "grandchild").await?)?,
        json!({
            "agent_id": grandchild.target_thread_id,
            "self_ref": grandchild.agent_ref.map(|agent_ref| agent_ref.to_string()),
        }),
    );
    let compaction_events = if remote_compaction {
        vec![
            json!({"type": "response.output_item.done", "item": {
                "type": "compaction", "encrypted_content": "receiver-identity-compaction",
            }}),
            responses::ev_completed("compact-child"),
        ]
    } else {
        vec![
            responses::ev_response_created("compact-child"),
            responses::ev_assistant_message(
                "compact-summary",
                "Continue the child's assigned task.",
            ),
            responses::ev_completed("compact-child"),
        ]
    };
    let compacted = responses::mount_sse_once(&server, responses::sse(compaction_events)).await;
    thread.submit(Op::Compact).await?;
    wait_for_event(&thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    assert_eq!(
        receiver_identity(&compacted.single_request())?,
        child_identity
    );
    assert_eq!(
        receiver_identity(&sample(&thread, &server, "continued-child").await?)?,
        child_identity,
    );
    let rollout = thread.rollout_path().context("child rollout")?;
    test.thread_manager.detach_agent_branch(child_id).await?;
    let independent = test
        .thread_manager
        .resume_legacy_thread_from_rollout(
            test.config.clone(),
            rollout,
            test.thread_manager.auth_manager(),
            /*parent_trace*/ None,
            Default::default(),
        )
        .await?;
    assert_eq!(
        (
            independent.thread_id,
            independent.thread.config_snapshot().await.parent_thread_id,
        ),
        (child_id, None),
    );
    assert_eq!(
        receiver_identity(&sample(&independent.thread, &server, "independent-root").await?)?,
        json!({"agent_id": child_id, "self_ref": "1"}),
    );
    let adopted = test
        .codex
        .resume_agent(
            &child_id.to_string(),
            /*task*/ None,
            UserAgentResponseHandling::Presentation,
        )
        .await?;
    let restored = test.thread_manager.get_thread(child_id).await?;
    assert_eq!(
        receiver_identity(&sample(&restored, &server, "adopted-child").await?)?,
        json!({
            "agent_id": child_id,
            "self_ref": adopted.agent_ref.map(|agent_ref| agent_ref.to_string()),
        }),
    );
    assert!(test.thread_manager.get_thread(root).await.is_ok());
    Ok(())
}
