//! Spawn keeps captured settings after handoff, but cannot report success for an early removal.

use super::*;
use codex_core::CodexThread;
use codex_core::ThreadManager;
use codex_extension_api::ExtensionFuture;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ToolCallOutcome;
use codex_extension_api::ToolFinishInput;
use codex_extension_api::ToolLifecycleContributor;
use codex_extension_api::TurnLifecycleContributor;
use codex_extension_api::TurnStartInput;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::protocol::CollabAgentRef;
use core_test_support::ThreadIdle;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::Weak;
use test_case::test_case;
use tokio::sync::oneshot;

type RemovedChild = (ThreadId, Arc<CodexThread>);

#[derive(Clone, Copy, PartialEq, Eq)]
enum RemovalBoundary {
    BeforeHandoff,
    AfterHandoff,
}

#[derive(Clone, Copy)]
enum SettingsSelection {
    RoleDefaults,
    RequestedOverrides,
}

struct RemoveChild {
    boundary: RemovalBoundary,
    context: OnceLock<(Weak<ThreadManager>, ThreadId)>,
    child_id: OnceLock<ThreadId>,
    removed: Mutex<Option<oneshot::Sender<RemovedChild>>>,
}

impl RemoveChild {
    async fn remove(&self, thread_id: ThreadId) {
        let (manager, _) = self.context.get().expect("test context initialized");
        let child = manager
            .upgrade()
            .expect("test manager is alive")
            .remove_thread(&thread_id)
            .await
            .expect("spawn registered the child");
        assert!(
            self.removed
                .lock()
                .expect("removed child sender lock")
                .take()
                .expect("only one child is removed")
                .send((thread_id, child))
                .is_ok()
        );
    }
}

impl TurnLifecycleContributor for RemoveChild {
    fn on_turn_start<'a>(&'a self, input: TurnStartInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let (_, root_thread_id) = self.context.get().expect("test context initialized");
            let thread_id = ThreadId::from_string(input.thread_store.level_id())
                .expect("thread store has a thread ID");
            if thread_id == *root_thread_id {
                return;
            }
            assert!(self.child_id.set(thread_id).is_ok());
            if self.boundary == RemovalBoundary::BeforeHandoff {
                // Removal closes this exact runtime's admission before the provisional
                // owner can transfer its receipt. This must be a failed spawn, not a
                // successful result with metadata reconstructed from the request.
                self.remove(thread_id).await;
            }
        })
    }
}

impl ToolLifecycleContributor for RemoveChild {
    fn on_tool_finish<'a>(&'a self, input: ToolFinishInput<'a>) -> ExtensionFuture<'a, ()> {
        Box::pin(async move {
            if self.boundary != RemovalBoundary::AfterHandoff || input.call_id != SPAWN_CALL_ID {
                return;
            }
            assert_eq!(input.outcome, ToolCallOutcome::Completed { success: true });
            let child_id = *self
                .child_id
                .get()
                .expect("accepted child entered its turn");
            let (manager, _) = self.context.get().expect("test context initialized");
            let child = manager
                .upgrade()
                .expect("test manager is alive")
                .get_thread(child_id)
                .await
                .expect("accepted child remains registered");
            // Finish the real child request before removing it, while the parent still
            // holds the successful tool output ahead of its next model request.
            timeout(Duration::from_secs(10), async {
                loop {
                    match child.next_event().await.expect("child event stream").msg {
                        EventMsg::TurnComplete(_) => break,
                        EventMsg::Error(error) => panic!("child failed: {error:?}"),
                        _ => {}
                    }
                }
            })
            .await
            .expect("accepted child completes its request");
            ThreadIdle::wait(&child).await;
            timeout(Duration::from_secs(10), child.shutdown_durably_and_wait())
                .await
                .expect("accepted child acknowledges shutdown")
                .expect("accepted child releases its writer");
            self.remove(child_id).await;
        })
    }
}

#[test_case(RemovalBoundary::BeforeHandoff, SettingsSelection::RequestedOverrides; "early removal is rejected")]
#[test_case(RemovalBoundary::AfterHandoff, SettingsSelection::RequestedOverrides; "accepted requested settings survive removal")]
#[test_case(RemovalBoundary::AfterHandoff, SettingsSelection::RoleDefaults; "accepted role settings survive removal")]
#[tokio::test]
async fn spawn_reports_effective_settings_after_child_runtime_is_removed(
    boundary: RemovalBoundary,
    settings: SettingsSelection,
) -> Result<()> {
    let server = start_mock_server().await;
    let mut arguments = json!({"message": CHILD_PROMPT, "agent_type": "custom"});
    let (expected_model, expected_effort) = match settings {
        SettingsSelection::RoleDefaults => (ROLE_MODEL, ROLE_REASONING_EFFORT),
        SettingsSelection::RequestedOverrides => {
            arguments["model"] = json!(REQUESTED_MODEL);
            arguments["reasoning_effort"] = json!(REQUESTED_REASONING_EFFORT);
            (REQUESTED_MODEL, REQUESTED_REASONING_EFFORT)
        }
    };
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, TURN_1_PROMPT) && !body_contains(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("parent-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &arguments.to_string(),
            ),
            ev_completed("parent-spawn"),
        ]),
    )
    .await;
    let child_requests = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_PROMPT) && !body_contains(request, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("child-done"),
            ev_completed("child-done"),
        ]),
    )
    .await;
    let parent_result = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("parent-done"),
            ev_completed("parent-done"),
        ]),
    )
    .await;
    let (removed_tx, removed_rx) = oneshot::channel();
    let remover = Arc::new(RemoveChild {
        boundary,
        context: OnceLock::new(),
        child_id: OnceLock::new(),
        removed: Mutex::new(Some(removed_tx)),
    });
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.turn_lifecycle_contributor(remover.clone());
    extensions.tool_lifecycle_contributor(remover.clone());
    extensions.thread_lifecycle_contributor(Arc::new(ThreadIdle));
    let test = test_codex()
        .with_model(INHERITED_MODEL)
        .with_extensions(Arc::new(extensions.build()))
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable collab");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("use V1 spawn");
            let role_path = config.codex_home.join("custom-role.toml");
            fs::write(
                &role_path,
                format!(
                    "model = \"{ROLE_MODEL}\"\nmodel_reasoning_effort = \"{ROLE_REASONING_EFFORT}\"\n"
                ),
            )
            .expect("write role config");
            config.agent_roles.insert(
                "custom".to_string(),
                AgentRoleConfig {
                    description: Some("Custom role".to_string()),
                    config_file: Some(role_path.to_path_buf()),
                    nickname_candidates: Some(vec!["Captured".to_string()]),
                },
            );
        })
        .build_with_auto_env(&server)
        .await?;
    assert!(
        remover
            .context
            .set((
                Arc::downgrade(&test.thread_manager),
                test.session_configured.thread_id,
            ))
            .is_ok()
    );

    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: TURN_1_PROMPT.to_string(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let (child_id, child) = timeout(Duration::from_secs(/*secs*/ 10), removed_rx).await??;
    assert!(matches!(
        test.thread_manager.get_thread(child_id).await,
        Err(error) if matches!(error.details(), codex_protocol::error::CodexErrorDetails::ThreadNotFound(id) if *id == child_id)
    ));
    let spawn = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemCompleted(completed) => match &completed.item {
            TurnItem::CollabAgentToolCall(item) if item.id == SPAWN_CALL_ID => Some(item.clone()),
            _ => None,
        },
        _ => None,
    })
    .await;
    assert_eq!(
        spawn.status,
        if boundary == RemovalBoundary::BeforeHandoff {
            CollabAgentToolCallStatus::Failed
        } else {
            CollabAgentToolCallStatus::Completed
        },
    );
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    ThreadIdle::wait(&test.codex).await;
    timeout(Duration::from_secs(10), child.wait_until_terminated()).await?;

    let alias = test
        .codex
        .state_db()
        .expect("state DB enabled")
        .find_agent_alias_by_thread(
            codex_protocol::SessionId::from(test.session_configured.thread_id),
            child_id,
        )
        .await?
        .expect("published spawn retains its canonical alias");
    let output = parent_result
        .single_request()
        .function_call_output_text(SPAWN_CALL_ID)
        .expect("parent receives the correlated spawn result");
    if boundary == RemovalBoundary::BeforeHandoff {
        assert_eq!(spawn.receiver_thread_ids, Vec::<ThreadId>::new());
        assert_eq!(spawn.receiver_agents, Vec::<CollabAgentRef>::new());
        assert_eq!(alias.state, codex_state::AgentAliasState::Closed);
        assert!(output.starts_with("collab spawn failed:"), "{output}");
        test.codex.shutdown_durably_and_wait().await?;
        return Ok(());
    }
    assert_eq!(spawn.receiver_thread_ids, vec![child_id]);
    assert_eq!(
        (spawn.model, spawn.reasoning_effort, spawn.receiver_agents),
        (
            Some(expected_model.to_string()),
            Some(expected_effort.clone()),
            vec![CollabAgentRef {
                agent_ref: Some(alias.agent_ref.to_string()),
                task_path: None,
                thread_id: child_id,
                agent_nickname: Some("Captured".to_string()),
                agent_role: Some("custom".to_string()),
            }],
        )
    );
    let child_request = child_requests.single_request();
    assert_eq!(
        child_request.header("thread-id"),
        Some(child_id.to_string())
    );
    let child_body = child_request.body_json();
    assert_eq!(
        (&child_body["model"], &child_body["reasoning"]["effort"]),
        (&json!(expected_model), &json!(expected_effort)),
    );
    assert_eq!(
        serde_json::from_str::<Value>(&output)?,
        json!({ "agent_id": child_id.to_string(), "nickname": "Captured", "ref": alias.agent_ref.to_string(), "task_path": null }),
    );
    test.codex.shutdown_durably_and_wait().await?;
    Ok(())
}
