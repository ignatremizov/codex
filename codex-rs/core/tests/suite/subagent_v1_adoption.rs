use super::*;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy, None; "legacy_unlabelled")]
#[test_case(ThreadHistoryMode::Legacy, Some("imported/backend"); "legacy_explicit_task")]
#[test_case(ThreadHistoryMode::Paginated, None; "paginated_unlabelled")]
#[test_case(ThreadHistoryMode::Paginated, Some("imported/backend"); "paginated_explicit_task")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_resume_reports_committed_foreign_root_assignment(
    history_mode: ThreadHistoryMode,
    task: Option<&str>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const PROMPT: &str = "adopt the closed foreign root";
    const CALL_ID: &str = "native-adoption-task";
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
        })
        .build_with_auto_env(&server)
        .await?;
    let foreign = test
        .thread_manager
        .start_thread(StartThreadOptions {
            initial_history: InitialHistory::Forked(vec![RolloutItem::ResponseItem(
                ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "materialized foreign root".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }
                .into(),
            )]),
            environments: Some(test.codex.config_snapshot().await.environments.environments),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    foreign.thread.flush_rollout().await?;
    // Root aliases are lazy. Materialize the source namespace so adoption must report
    // clearing or replacing its reserved /root assignment, not an absent legacy label.
    test.thread_manager
        .ensure_agent_alias_namespace_for_thread(foreign.thread_id)
        .await?;
    foreign.thread.submit(Op::Shutdown {}).await?;
    wait_for_event(foreign.thread.as_ref(), |event| {
        matches!(event, EventMsg::ShutdownComplete)
    })
    .await;
    test.thread_manager.remove_thread(&foreign.thread_id).await;
    let mut args = json!({"id": foreign.thread_id.to_string(), "w": "x"});
    if let Some(task) = task {
        args["task"] = json!(task);
    }
    let invocation = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, PROMPT) && !body_contains(req, CALL_ID),
        sse(vec![
            ev_response_created("native-adoption"),
            ev_function_call_with_namespace(
                CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "resume_agent",
                &serde_json::to_string(&args)?,
            ),
            ev_completed("native-adoption"),
        ]),
    )
    .await;
    let completed = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, CALL_ID),
        sse(vec![
            ev_response_created("native-adoption-done"),
            ev_assistant_message("native-adoption-final", "done"),
            ev_completed("native-adoption-done"),
        ]),
    )
    .await;
    test.submit_turn(PROMPT).await?;
    let request = wait_for_request_containing_text(&completed, CALL_ID).await?;
    let output = request
        .function_call_output_text(CALL_ID)
        .ok_or_else(|| anyhow::anyhow!("missing adoption result"))?;
    let result: Value = serde_json::from_str(&output)?;
    let expected_path = task.map(|task| format!("/root/{task}"));
    assert_eq!(
        result["adoption"],
        json!({
            "task_path": expected_path,
            "task_path_mapping": [{
                "threadId": foreign.thread_id.to_string(),
                "previousTaskPath": "/root",
                "taskPath": expected_path,
            }],
        }),
    );
    let resumed = test.thread_manager.get_thread(foreign.thread_id).await?;
    assert!(!matches!(
        resumed.agent_status().await,
        AgentStatus::Running
    ));
    assert_eq!(invocation.requests().len(), 1);
    Ok(())
}

#[derive(Clone, Copy)]
enum OwnedTargetState {
    Loaded,
    Closed,
}

#[test_case(ThreadHistoryMode::Legacy, OwnedTargetState::Loaded; "legacy_loaded")]
#[test_case(ThreadHistoryMode::Legacy, OwnedTargetState::Closed; "legacy_closed")]
#[test_case(ThreadHistoryMode::Paginated, OwnedTargetState::Loaded; "paginated_loaded")]
#[test_case(ThreadHistoryMode::Paginated, OwnedTargetState::Closed; "paginated_closed")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_resume_cannot_relabel_an_owned_agent(
    history_mode: ThreadHistoryMode,
    target_state: OwnedTargetState,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const PROMPT: &str = "try assigning task through same-root resume";
    const CALL_ID: &str = "reject-native-resume-task";
    let server = start_mock_server().await;
    let test = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config.features.enable(Feature::Collab).expect("enable V1");
            config
                .features
                .disable(Feature::MultiAgentV2)
                .expect("disable V2");
        })
        .build_with_auto_env(&server)
        .await?;
    let child_id = test
        .codex
        .spawn_agent(UserAgentSpawnOptions {
            response_handling: UserAgentResponseHandling::Presentation,
            ..Default::default()
        })
        .await?
        .target_thread_id;
    if let OwnedTargetState::Closed = target_state {
        test.codex
            .close_agent(
                &child_id.to_string(),
                UserAgentResponseHandling::Presentation,
            )
            .await?;
    }
    let before = test.thread_manager.get_thread(child_id).await.ok();
    let invocation = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, PROMPT) && !body_contains(req, CALL_ID),
        sse(vec![
            ev_response_created("reject-native-task"),
            ev_function_call_with_namespace(
                CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "resume_agent",
                &serde_json::to_string(&json!({
                    "id": child_id.to_string(),
                    "task": "replacement",
                    "w": "mx",
                }))?,
            ),
            ev_completed("reject-native-task"),
        ]),
    )
    .await;
    let completed = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, CALL_ID),
        sse(vec![
            ev_response_created("reject-native-task-done"),
            ev_assistant_message("reject-native-task-final", "done"),
            ev_completed("reject-native-task-done"),
        ]),
    )
    .await;
    test.submit_turn(PROMPT).await?;
    let output = wait_for_request_containing_text(&completed, CALL_ID)
        .await?
        .function_call_output(CALL_ID)
        .to_string();
    assert!(
        output.contains("task can only be assigned during cross-root adoption"),
        "{output}"
    );
    let after = test.thread_manager.get_thread(child_id).await.ok();
    assert_eq!(
        before.is_some(),
        after.is_some(),
        "rejected task must not resume the target"
    );
    if let (Some(before), Some(after)) = (before, after) {
        assert!(Arc::ptr_eq(&before, &after));
    }
    assert_eq!(invocation.requests().len(), 1);
    Ok(())
}
