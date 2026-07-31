use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::write_models_cache;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ItemCompletedNotification;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::SessionSource;
use codex_app_server_protocol::Thread;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::ThreadListParams;
use codex_app_server_protocol::ThreadListResponse;
use codex_app_server_protocol::ThreadLoadedListParams;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadReadParams;
use codex_app_server_protocol::ThreadReadResponse;
use codex_app_server_protocol::ThreadResumeParams;
use codex_app_server_protocol::ThreadResumeResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::ThreadStatus;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::SubAgentSource;
use codex_state::DirectionalThreadSpawnEdgeStatus;
use codex_state::SqliteConfig;
use codex_state::StateRuntime;
use codex_utils_absolute_path::test_support::PathExt;
use core_test_support::responses;
use core_test_support::responses::ResponseMock;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::timeout;

#[cfg(windows)]
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(25);
#[cfg(not(windows))]
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

const ROOT_SPAWN_CALL_ID: &str = "spawn-restorable-worker";
const NESTED_SPAWN_CALL_ID: &str = "spawn-nested-worker";
const FOLLOWUP_CALL_ID: &str = "followup-restored-worker";
const SIBLING_SPAWN_CALL_ID: &str = "spawn-replacement-worker";
const ROOT_PROMPT: &str = "spawn the restorable worker";
const WORKER_TASK: &str = "perform the initial durable task";
const NESTED_TASK: &str = "perform the nested durable task";
const FOLLOWUP_PROMPT: &str = "follow up with the restored worker";
const FOLLOWUP_TASK: &str = "perform the restored follow-up";
const FOLLOWUP_RESULT: &str = "restored worker finished";
const SIBLING_PROMPT: &str = "spawn the replacement worker";
const SIBLING_TASK: &str = "perform the replacement task";
const SIBLING_RESIDENT_TASK: &str = "confirm the replacement is resident";
const ROLE_NAME: &str = "restored_worker";
const ROLE_NICKNAME: &str = "Keeper";
const ROLE_INSTRUCTIONS: &str = "Preserve the restored worker role.";
const MULTI_AGENT_V2_NAMESPACE: &str = "collaboration";

fn body_contains(request: &wiremock::Request, text: &str) -> bool {
    String::from_utf8(request.body.clone())
        .ok()
        .is_some_and(|body| body.contains(text))
}

fn write_v2_config(codex_home: &Path, server_uri: &str, max_threads: usize) -> Result<()> {
    std::fs::write(
        codex_home.join("restored-worker.toml"),
        format!(
            "model = \"gpt-5.4\"\nmodel_reasoning_effort = \"high\"\ndeveloper_instructions = {ROLE_INSTRUCTIONS:?}\n"
        ),
    )?;
    MockResponsesConfig::new(server_uri)
        .with_model("gpt-5.4")
        .with_extra_config(&format!(
            r#"
[features.multi_agent_v2]
enabled = true
max_concurrent_threads_per_session = {max_threads}
message_delivery = "plaintext"
tool_namespace = "collaboration"
non_code_mode_only = false

[agents.{ROLE_NAME}]
description = "Persisted restoration test role"
config_file = "./restored-worker.toml"
nickname_candidates = ["{ROLE_NICKNAME}"]
"#
        ))
        .write(codex_home)?;
    write_models_cache(codex_home)?;
    Ok(())
}

async fn wait_for_mock_request(mock: &ResponseMock) -> Result<()> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            if !mock.requests().is_empty() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?;
    Ok(())
}

async fn wait_for_direct_children(
    app_server: &mut TestAppServer,
    parent_thread_id: &str,
    expected_count: usize,
) -> Result<Vec<Thread>> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let mut response: ThreadListResponse = app_server
                .request(|request_id| ClientRequest::ThreadList {
                    request_id,
                    params: ThreadListParams {
                        cursor: None,
                        limit: Some(20),
                        sort_key: None,
                        sort_direction: None,
                        model_providers: None,
                        source_kinds: None,
                        archived: None,
                        section_id: None,
                        cwd: None,
                        use_state_db_only: true,
                        search_term: None,
                        parent_thread_id: Some(parent_thread_id.to_string()),
                        ancestor_thread_id: None,
                    },
                })
                .await?;
            if response.data.len() == expected_count {
                response.data.sort_by(|left, right| left.id.cmp(&right.id));
                return Ok::<Vec<Thread>, anyhow::Error>(response.data);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await?
}

async fn wait_for_thread_idle(app_server: &mut TestAppServer, thread_id: &str) -> Result<()> {
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let response: ThreadReadResponse = app_server
                .request(|request_id| ClientRequest::ThreadRead {
                    request_id,
                    params: ThreadReadParams {
                        thread_id: thread_id.to_string(),
                        include_turns: false,
                    },
                })
                .await?;
            if response.thread.status == ThreadStatus::Idle {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await??;
    Ok(())
}

async fn wait_for_loaded_threads(
    app_server: &mut TestAppServer,
    expected_thread_ids: &[&str],
) -> Result<()> {
    let mut expected_thread_ids = expected_thread_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    expected_thread_ids.sort();
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let mut response: ThreadLoadedListResponse = app_server
                .request(|request_id| ClientRequest::ThreadLoadedList {
                    request_id,
                    params: ThreadLoadedListParams::default(),
                })
                .await?;
            response.data.sort();
            if response.data == expected_thread_ids {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await??;
    Ok(())
}

async fn wait_for_spawn_edge(
    state_db: &StateRuntime,
    parent_thread_id: ThreadId,
    status: DirectionalThreadSpawnEdgeStatus,
    expected_child_ids: &[ThreadId],
) -> Result<()> {
    let mut expected_child_ids = expected_child_ids
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    expected_child_ids.sort();
    timeout(DEFAULT_TIMEOUT, async {
        loop {
            let mut child_ids = state_db
                .list_thread_spawn_children_with_status(parent_thread_id, status)
                .await?
                .into_iter()
                .map(|thread_id| thread_id.to_string())
                .collect::<Vec<_>>();
            child_ids.sort();
            if child_ids == expected_child_ids {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 10)).await;
        }
    })
    .await??;
    Ok(())
}

async fn start_turn(
    app_server: &mut TestAppServer,
    thread_id: &str,
    prompt: &str,
) -> Result<TurnStartResponse> {
    app_server
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread_id.to_string(),
                input: vec![UserInput::Text {
                    text: prompt.to_string(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            },
        })
        .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn generic_resume_restores_closed_v2_subagent_through_live_owner() -> Result<()> {
    let server = responses::start_mock_server().await;
    let root_spawn_args = serde_json::to_string(&json!({
        "message": WORKER_TASK,
        "task_name": "worker",
        "agent_type": ROLE_NAME,
        "fork_turns": "none",
    }))?;
    let root_spawn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("root-spawn"),
            responses::ev_function_call_with_namespace(
                ROOT_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &root_spawn_args,
            ),
            responses::ev_completed("root-spawn"),
        ]),
    )
    .await;
    let nested_spawn_args = serde_json::to_string(&json!({
        "message": NESTED_TASK,
        "task_name": "nested",
        "fork_turns": "none",
    }))?;
    let worker_spawn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, WORKER_TASK) && !body_contains(request, ROOT_SPAWN_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("worker-spawn"),
            responses::ev_function_call_with_namespace(
                NESTED_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &nested_spawn_args,
            ),
            responses::ev_completed("worker-spawn"),
        ]),
    )
    .await;
    let nested_work = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, NESTED_TASK) && !body_contains(request, NESTED_SPAWN_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("nested-work"),
            responses::ev_assistant_message("nested-result", "nested worker finished"),
            responses::ev_completed("nested-work"),
        ]),
    )
    .await;
    let worker_finished = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, NESTED_SPAWN_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("worker-finished"),
            responses::ev_assistant_message("worker-result", "worker finished"),
            responses::ev_completed("worker-finished"),
        ]),
    )
    .await;
    let root_finished = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, ROOT_SPAWN_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("root-finished"),
            responses::ev_assistant_message("root-result", "root finished"),
            responses::ev_completed("root-finished"),
        ]),
    )
    .await;

    let codex_home = TempDir::new()?;
    write_v2_config(codex_home.path(), &server.uri(), /*max_threads*/ 3)?;
    let mut initial = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let ThreadStartResponse {
        thread: root_thread,
        ..
    } = initial.start_thread(ThreadStartParams::default()).await?;
    initial
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: root_thread.id.clone(),
            input: vec![UserInput::Text {
                text: ROOT_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    for mock in [
        &root_spawn,
        &worker_spawn,
        &nested_work,
        &worker_finished,
        &root_finished,
    ] {
        wait_for_mock_request(mock).await?;
    }

    let worker_thread = wait_for_direct_children(&mut initial, &root_thread.id, 1)
        .await?
        .pop()
        .expect("root should have one worker");
    let nested_thread = wait_for_direct_children(&mut initial, &worker_thread.id, 1)
        .await?
        .pop()
        .expect("worker should have one nested child");
    wait_for_thread_idle(&mut initial, &worker_thread.id).await?;
    wait_for_thread_idle(&mut initial, &nested_thread.id).await?;

    let root_thread_id = ThreadId::from_string(&root_thread.id)?;
    let worker_thread_id = ThreadId::from_string(&worker_thread.id)?;
    let nested_thread_id = ThreadId::from_string(&nested_thread.id)?;
    assert_eq!(worker_thread.parent_thread_id, Some(root_thread.id.clone()));
    assert_eq!(worker_thread.agent_nickname.as_deref(), Some(ROLE_NICKNAME));
    assert_eq!(worker_thread.agent_role.as_deref(), Some(ROLE_NAME));
    assert_eq!(
        worker_thread.source,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: root_thread_id,
            depth: 1,
            agent_path: Some(
                AgentPath::try_from("/root/worker").expect("static worker path should be valid"),
            ),
            agent_nickname: Some(ROLE_NICKNAME.to_string()),
            agent_role: Some(ROLE_NAME.to_string()),
        })
    );
    assert_eq!(
        nested_thread.source,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: worker_thread_id,
            depth: 2,
            agent_path: Some(
                AgentPath::try_from("/root/worker/nested")
                    .expect("static nested worker path should be valid"),
            ),
            agent_nickname: nested_thread.agent_nickname.clone(),
            agent_role: nested_thread.agent_role.clone(),
        })
    );

    let sibling_args = serde_json::to_string(&json!({
        "message": SIBLING_TASK,
        "task_name": "replacement",
        "agent_type": ROLE_NAME,
        "fork_turns": "none",
    }))?;
    let root_sibling_spawn = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SIBLING_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("root-sibling-spawn"),
            responses::ev_function_call_with_namespace(
                SIBLING_SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &sibling_args,
            ),
            responses::ev_completed("root-sibling-spawn"),
        ]),
    )
    .await;
    let sibling_work = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, SIBLING_TASK) && !body_contains(request, SIBLING_SPAWN_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("sibling-work"),
            responses::ev_assistant_message("sibling-result", "replacement finished"),
            responses::ev_completed("sibling-work"),
        ]),
    )
    .await;
    let root_sibling_finished = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SIBLING_SPAWN_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("root-sibling-finished"),
            responses::ev_assistant_message("root-sibling-result", "replacement spawned"),
            responses::ev_completed("root-sibling-finished"),
        ]),
    )
    .await;
    initial
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: root_thread.id.clone(),
            input: vec![UserInput::Text {
                text: SIBLING_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    wait_for_mock_request(&root_sibling_spawn).await?;
    wait_for_mock_request(&sibling_work).await?;
    wait_for_mock_request(&root_sibling_finished).await?;
    let sibling_thread = wait_for_direct_children(&mut initial, &root_thread.id, 2)
        .await?
        .into_iter()
        .find(|thread| thread.id != worker_thread.id)
        .expect("root should have a replacement child");
    wait_for_thread_idle(&mut initial, &sibling_thread.id).await?;
    assert_eq!(
        sibling_thread.parent_thread_id,
        Some(root_thread.id.clone())
    );

    timeout(DEFAULT_TIMEOUT, initial.shutdown_gracefully()).await??;
    drop(initial);

    let sibling_thread_id = ThreadId::from_string(&sibling_thread.id)?;
    let state_db = StateRuntime::init(
        SqliteConfig::new_for_testing(codex_home.path().abs()),
        "mock_provider".to_string(),
    )
    .await?;
    state_db
        .set_thread_spawn_edge_status(worker_thread_id, DirectionalThreadSpawnEdgeStatus::Closed)
        .await?;
    wait_for_spawn_edge(
        &state_db,
        root_thread_id,
        DirectionalThreadSpawnEdgeStatus::Closed,
        &[worker_thread_id],
    )
    .await?;
    wait_for_spawn_edge(
        &state_db,
        root_thread_id,
        DirectionalThreadSpawnEdgeStatus::Open,
        &[sibling_thread_id],
    )
    .await?;
    wait_for_spawn_edge(
        &state_db,
        worker_thread_id,
        DirectionalThreadSpawnEdgeStatus::Open,
        &[nested_thread_id],
    )
    .await?;

    // The resumed process has room for one resident V2 child. Preload the persisted open sibling,
    // then require Closed worker restoration itself to reserve capacity and evict that sibling.
    write_v2_config(codex_home.path(), &server.uri(), /*max_threads*/ 2)?;
    let resumed_workspace_root = TempDir::new()?;
    let mut resumed = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;
    let root_resume_id = resumed
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: root_thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let root_resume: ThreadResumeResponse =
        timeout(DEFAULT_TIMEOUT, resumed.read_response(root_resume_id)).await??;
    assert_eq!(root_resume.thread.id, root_thread.id);
    assert_eq!(root_resume.thread.parent_thread_id, None);
    assert_eq!(root_resume.thread.agent_nickname, None);
    assert_eq!(root_resume.thread.agent_role, None);
    assert!(!matches!(
        root_resume.thread.source,
        SessionSource::SubAgent(_)
    ));
    wait_for_loaded_threads(&mut resumed, &[&root_thread.id]).await?;

    let sibling_resume_id = resumed
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: sibling_thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let sibling_resume: ThreadResumeResponse =
        timeout(DEFAULT_TIMEOUT, resumed.read_response(sibling_resume_id)).await??;
    assert_eq!(sibling_resume.thread.source, sibling_thread.source);
    assert_eq!(
        sibling_resume.thread.parent_thread_id,
        Some(root_thread.id.clone())
    );
    wait_for_loaded_threads(&mut resumed, &[&root_thread.id, &sibling_thread.id]).await?;

    // Make the existing resident explicitly terminal so closed-child restoration can exercise
    // the normal V2 eviction path rather than depending on startup status reconstruction.
    let sibling_resident_work = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SIBLING_RESIDENT_TASK),
        responses::sse(vec![
            responses::ev_response_created("sibling-resident"),
            responses::ev_assistant_message("sibling-resident-result", "resident confirmed"),
            responses::ev_completed("sibling-resident"),
        ]),
    )
    .await;
    resumed
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: sibling_thread.id.clone(),
            input: vec![UserInput::Text {
                text: SIBLING_RESIDENT_TASK.to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    wait_for_mock_request(&sibling_resident_work).await?;
    wait_for_thread_idle(&mut resumed, &sibling_thread.id).await?;

    // The root control is live, but the nested child's direct parent is not. Resuming it must
    // fail before loading or rewriting the graph instead of attaching it as a root child.
    let nested_resume_id = resumed
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: nested_thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let nested_resume_error = timeout(
        DEFAULT_TIMEOUT,
        resumed.read_stream_until_error_message(RequestId::Integer(nested_resume_id)),
    )
    .await??;
    assert!(
        nested_resume_error.error.message.contains("direct parent")
            && nested_resume_error
                .error
                .message
                .contains("resume the parent"),
        "unexpected nested resume error: {}",
        nested_resume_error.error.message
    );
    wait_for_loaded_threads(&mut resumed, &[&root_thread.id, &sibling_thread.id]).await?;
    wait_for_spawn_edge(
        &state_db,
        worker_thread_id,
        DirectionalThreadSpawnEdgeStatus::Open,
        &[nested_thread_id],
    )
    .await?;

    let worker_resume_id = resumed
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: worker_thread.id.clone(),
            runtime_workspace_roots: Some(vec![resumed_workspace_root.path().abs()]),
            ..Default::default()
        })
        .await?;
    let worker_resume: ThreadResumeResponse =
        timeout(DEFAULT_TIMEOUT, resumed.read_response(worker_resume_id)).await??;
    assert_eq!(worker_resume.thread.id, worker_thread.id);
    assert_eq!(worker_resume.thread.session_id, root_thread.session_id);
    assert_eq!(
        worker_resume.thread.parent_thread_id,
        Some(root_thread.id.clone())
    );
    assert_eq!(worker_resume.thread.source, worker_thread.source);
    assert_eq!(
        worker_resume.thread.agent_nickname,
        worker_thread.agent_nickname
    );
    assert_eq!(worker_resume.thread.agent_role, worker_thread.agent_role);
    assert_eq!(worker_resume.reasoning_effort, Some(ReasoningEffort::High));
    assert_eq!(
        worker_resume.runtime_workspace_roots,
        vec![resumed_workspace_root.path().abs()]
    );
    wait_for_spawn_edge(
        &state_db,
        root_thread_id,
        DirectionalThreadSpawnEdgeStatus::Open,
        &[worker_thread_id, sibling_thread_id],
    )
    .await?;
    // Closed restoration must reserve before constructing the worker runtime. With one child
    // residency slot, the already-terminal sibling is evicted during this request, not during
    // unrelated later work.
    wait_for_loaded_threads(&mut resumed, &[&root_thread.id, &worker_thread.id]).await?;

    // Running resume is idempotent and must retain the same graph/control identity.
    let repeated_resume_id = resumed
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: worker_thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let repeated_resume: ThreadResumeResponse =
        timeout(DEFAULT_TIMEOUT, resumed.read_response(repeated_resume_id)).await??;
    assert_eq!(
        (
            repeated_resume.thread.id,
            repeated_resume.thread.session_id,
            repeated_resume.thread.parent_thread_id,
            repeated_resume.thread.source,
            repeated_resume.thread.agent_nickname,
            repeated_resume.thread.agent_role,
        ),
        (
            worker_resume.thread.id,
            worker_resume.thread.session_id,
            worker_resume.thread.parent_thread_id,
            worker_resume.thread.source,
            worker_resume.thread.agent_nickname,
            worker_resume.thread.agent_role,
        )
    );
    wait_for_loaded_threads(&mut resumed, &[&root_thread.id, &worker_thread.id]).await?;

    let followup_args = serde_json::to_string(&json!({
        "target": "worker",
        "message": FOLLOWUP_TASK,
    }))?;
    let root_followup = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FOLLOWUP_PROMPT),
        responses::sse(vec![
            responses::ev_response_created("root-followup"),
            responses::ev_function_call_with_namespace(
                FOLLOWUP_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "followup_task",
                &followup_args,
            ),
            responses::ev_completed("root-followup"),
        ]),
    )
    .await;
    let worker_followup = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, FOLLOWUP_TASK) && !body_contains(request, FOLLOWUP_CALL_ID)
        },
        responses::sse(vec![
            responses::ev_response_created("worker-followup"),
            responses::ev_assistant_message("worker-followup-result", FOLLOWUP_RESULT),
            responses::ev_completed("worker-followup"),
        ]),
    )
    .await;
    let root_followup_finished = responses::mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, FOLLOWUP_CALL_ID),
        responses::sse(vec![
            responses::ev_response_created("root-followup-finished"),
            responses::ev_assistant_message("root-followup-result", "follow-up dispatched"),
            responses::ev_completed("root-followup-finished"),
        ]),
    )
    .await;
    let _: TurnStartResponse = start_turn(&mut resumed, &root_thread.id, FOLLOWUP_PROMPT).await?;
    wait_for_mock_request(&root_followup).await?;
    wait_for_mock_request(&worker_followup).await?;
    wait_for_mock_request(&root_followup_finished).await?;
    let followup_request = worker_followup.single_request();
    assert_eq!(
        followup_request.body_json()["client_metadata"]["thread_id"],
        json!(worker_thread.id)
    );
    assert_eq!(
        followup_request.body_json()["client_metadata"]["x-codex-parent-thread-id"],
        json!(root_thread.id)
    );

    let completion_item = timeout(DEFAULT_TIMEOUT, async {
        loop {
            let completed: ItemCompletedNotification =
                resumed.read_notification("item/completed").await?;
            if completed.thread_id == root_thread.id
                && let ThreadItem::AgentMessage { text, .. } = &completed.item
                && text.contains("Agent final answer from `/root/worker`")
                && text.contains(FOLLOWUP_RESULT)
            {
                return Ok::<ThreadItem, anyhow::Error>(completed.item);
            }
        }
    })
    .await??;
    assert!(matches!(completion_item, ThreadItem::AgentMessage { .. }));
    wait_for_thread_idle(&mut resumed, &root_thread.id).await?;
    wait_for_thread_idle(&mut resumed, &worker_thread.id).await?;

    Ok(())
}
