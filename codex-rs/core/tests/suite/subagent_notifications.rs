use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::ThreadConfigSnapshot;
use codex_core::config::AgentRoleConfig;
use codex_core::config::MultiAgentMessageDelivery;
use codex_core::config::ThreadStoreConfig;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_models_manager::bundled_models_response;
use codex_protocol::ThreadId;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::TurnItem;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InitialHistory;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ResumedHistory;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentCompletionStatus;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::new_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::sub_agent_completion_status_from_response_item_id;
use codex_protocol::protocol::sub_agent_completion_transcript_parts;
use codex_protocol::rollout::rollout_without_exact_rollback_ranges;
use codex_protocol::user_input::UserInput;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadStore;
use core_test_support::hooks::trust_discovered_hooks;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::assert_parent_turn;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call_with_namespace;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::ev_tool_search_call;
use core_test_support::responses::mount_responder_once_match;
use core_test_support::responses::mount_response_once_match;
use core_test_support::responses::mount_sse_once_match;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::namespace_child_tool;
use core_test_support::responses::sse;
use core_test_support::responses::sse_response;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::strip_metadata_from_json;
use core_test_support::responses::strip_response_item_ids_from_json;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event_match;
use core_test_support::wait_for_event_with_timeout;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc;
use std::time::Duration;
use test_case::test_case;
use tokio::time::Instant;
use tokio::time::sleep;
use tracing::Level;
use tracing_test::internal::MockWriter;
use wiremock::MockServer;
use wiremock::Respond;
use wiremock::ResponseTemplate;

const SPAWN_CALL_ID: &str = "spawn-call-1";
const MULTI_AGENT_V1_NAMESPACE: &str = "multi_agent_v1";
const MULTI_AGENT_V2_NAMESPACE: &str = "collaboration";
const TURN_0_FORK_PROMPT: &str = "seed fork context";
const TURN_1_PROMPT: &str = "spawn a child and continue";
const TURN_2_NO_WAIT_PROMPT: &str = "follow up without wait";
const TURN_AFTER_ABORT_PROMPT: &str = "follow up after abort";
const TURN_AFTER_RESUME_PROMPT: &str = "follow up after resume";
const CHILD_PROMPT: &str = "child: do work";
const INHERITED_MODEL: &str = "gpt-5.2";
const INHERITED_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::XHigh;
const REQUESTED_MODEL: &str = "gpt-5.4";
const REQUESTED_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::Low;
const V2_DEFAULT_MODEL: &str = "gpt-5.6-terra";
const V2_DEFAULT_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::High;
const V2_REQUESTED_MODEL: &str = "gpt-5.6-sol";
const V2_REQUESTED_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::Low;

enum ChildResponseTiming {
    Immediate,
    Delayed(Duration),
    Gated(mpsc::Receiver<()>),
}

struct GatedSseResponse {
    gate_rx: Mutex<Option<mpsc::Receiver<()>>>,
    response: String,
}

impl Respond for GatedSseResponse {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        if let Some(gate_rx) = self
            .gate_rx
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            let _ = gate_rx.recv();
        }
        sse_response(self.response.clone())
    }
}
const ROLE_MODEL: &str = "gpt-5.4";
const ROLE_REASONING_EFFORT: ReasoningEffort = ReasoningEffort::High;
const SUBAGENT_START_CONTEXT: &str = "subagent start context reaches child";
const SUBAGENT_STOP_CONTINUATION: &str = "continue only the child";
const INTERNAL_SUBAGENT_PROMPT: &str = "internal subagent: review";

fn body_contains(req: &wiremock::Request, text: &str) -> bool {
    request_body_bytes(req)
        .and_then(|body| String::from_utf8(body).ok())
        .is_some_and(|body| body.contains(text))
}

fn ev_commentary_message(id: &str, text: &str) -> Value {
    let mut event = ev_assistant_message(id, text);
    event["item"]["phase"] = json!("commentary");
    event
}

fn request_has_input_type(req: &wiremock::Request, ty: &str) -> bool {
    request_body_bytes(req)
        .and_then(|body| serde_json::from_slice::<Value>(&body).ok())
        .and_then(|body| body.get("input").and_then(Value::as_array).cloned())
        .is_some_and(|items| {
            items
                .iter()
                .any(|item| item.get("type").and_then(Value::as_str) == Some(ty))
        })
}

fn request_body_bytes(req: &wiremock::Request) -> Option<Vec<u8>> {
    let is_zstd = req
        .headers
        .get("content-encoding")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|entry| entry.trim().eq_ignore_ascii_case("zstd"))
        });

    if is_zstd {
        zstd::stream::decode_all(std::io::Cursor::new(&req.body)).ok()
    } else {
        Some(req.body.clone())
    }
}

fn log_field<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}=");
    line.split_ascii_whitespace()
        .find_map(|field| field.strip_prefix(&prefix))
        .map(|value| value.trim_matches('"'))
}

fn request_has_agent_message_text(req: &wiremock::Request, expected_text: &str) -> bool {
    let Some(body) = request_body_bytes(req) else {
        return false;
    };
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return false;
    };
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return false;
    };
    input.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("agent_message")
            && item
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|content| {
                    content.iter().any(|part| {
                        part.get("type").and_then(Value::as_str) == Some("input_text")
                            && part.get("text").and_then(Value::as_str) == Some(expected_text)
                    })
                })
    })
}

fn request_has_agent_message_route(
    req: &wiremock::Request,
    expected_author: &str,
    expected_recipient: &str,
) -> bool {
    let Some(body) = request_body_bytes(req) else {
        return false;
    };
    let Ok(body) = serde_json::from_slice::<Value>(&body) else {
        return false;
    };
    let Some(input) = body.get("input").and_then(Value::as_array) else {
        return false;
    };
    input.iter().any(|item| {
        item.get("type").and_then(Value::as_str) == Some("agent_message")
            && item.get("author").and_then(Value::as_str) == Some(expected_author)
            && item.get("recipient").and_then(Value::as_str) == Some(expected_recipient)
    })
}

async fn wait_for_agent_messages(
    mock: &core_test_support::responses::ResponseMock,
    expected_agent_messages: &[Value],
    description: &str,
) -> Result<ResponsesRequest> {
    let expected_agent_messages = normalize_agent_messages(expected_agent_messages.to_vec());
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let requests = mock.requests();
        if let Some(request) = requests.into_iter().find(|request| {
            normalize_agent_messages(request.inputs_of_type("agent_message"))
                == expected_agent_messages
        }) {
            return Ok(request);
        }
        if Instant::now() >= deadline {
            let observed_agent_messages: Vec<Vec<Value>> = mock
                .requests()
                .iter()
                .map(|request| request.inputs_of_type("agent_message"))
                .collect();
            anyhow::bail!("{description}, got {observed_agent_messages:#?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn normalize_agent_messages(messages: Vec<Value>) -> Value {
    Value::Array(
        messages
            .into_iter()
            .map(strip_metadata_from_json)
            .map(|mut message| {
                if let Value::Object(message) = &mut message {
                    message.remove("id");
                }
                message
            })
            .collect(),
    )
}

async fn wait_for_request_containing_text(
    mock: &core_test_support::responses::ResponseMock,
    text: &str,
) -> Result<ResponsesRequest> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(request) = mock
            .requests()
            .into_iter()
            .find(|request| request.body_contains_text(text))
        {
            return Ok(request);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for request containing {text:?}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

fn read_test_rollout_items(test: &TestCodex) -> Result<Vec<RolloutItem>> {
    let rollout_path = test
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    fs::read_to_string(rollout_path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str::<RolloutLine>(line)
                .map(|rollout_line| rollout_line.item)
                .map_err(Into::into)
        })
        .collect()
}

fn has_subagent_notification(req: &ResponsesRequest) -> bool {
    req.message_input_texts("user")
        .iter()
        .any(|text| text.contains("<subagent_notification>"))
}

fn assert_input_item_ids_are_provider_compatible(request: &ResponsesRequest) {
    let body = request.body_json();
    for item in body["input"].as_array().into_iter().flatten() {
        if let Some(id) = item["id"].as_str() {
            assert!(id.len() <= 64, "input item ID exceeds provider limit: {id}");
            if item["type"].as_str() == Some("message") {
                assert!(
                    id.starts_with("msg"),
                    "message input item ID has invalid provider prefix: {id}"
                );
            }
        }
    }
}

fn sub_agent_completion_item(
    item: &TurnItem,
) -> Option<(String, SubAgentCompletionStatus, String, String)> {
    let TurnItem::AgentMessage(item) = item else {
        return None;
    };
    if item.phase != Some(MessagePhase::Commentary) || !item.has_sub_agent_completion_identity() {
        return None;
    }
    let status = sub_agent_completion_status_from_response_item_id(&item.id)?;
    let [AgentMessageContent::Text { text }] = item.content.as_slice() else {
        return None;
    };
    let (agent_reference, payload) = sub_agent_completion_transcript_parts(text)?;
    Some((
        item.id.clone(),
        status,
        agent_reference.to_string(),
        payload.to_string(),
    ))
}

fn sub_agent_completion_started_id(event: &EventMsg) -> Option<String> {
    let EventMsg::ItemStarted(event) = event else {
        return None;
    };
    sub_agent_completion_item(&event.item).map(|(id, _, _, _)| id)
}

fn sub_agent_completion_event(
    event: &EventMsg,
) -> Option<(String, SubAgentCompletionStatus, String, String)> {
    let EventMsg::ItemCompleted(event) = event else {
        return None;
    };
    sub_agent_completion_item(&event.item)
}

fn completed_wait_agent_states(
    event: &EventMsg,
) -> Option<HashMap<ThreadId, codex_protocol::protocol::AgentStatus>> {
    let EventMsg::ItemCompleted(event) = event else {
        return None;
    };
    let TurnItem::CollabAgentToolCall(item) = &event.item else {
        return None;
    };
    (item.tool == CollabAgentTool::Wait && !item.agents_states.is_empty())
        .then(|| item.agents_states.clone())
}

fn completed_parent_turn_has_error(event: &EventMsg) -> Option<bool> {
    let EventMsg::TurnComplete(event) = event else {
        return None;
    };
    (event.last_agent_message.as_deref() == Some("done")).then_some(event.error.is_some())
}

async fn wait_for_terminal_status(thread: &codex_core::CodexThread) -> Result<AgentStatus> {
    let deadline = Instant::now() + Duration::from_secs(/*secs*/ 10);
    loop {
        let status = thread.agent_status().await;
        if matches!(
            status,
            AgentStatus::Completed(_)
                | AgentStatus::Errored(_)
                | AgentStatus::Shutdown
                | AgentStatus::NotFound
        ) {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for child terminal publication");
        }
        tokio::task::yield_now().await;
    }
}

async fn assert_no_pending_response_observation(
    store: &InMemoryThreadStore,
    observer_thread_id: ThreadId,
    target_thread_id: ThreadId,
) -> Result<()> {
    assert!(
        response_observation_has_no_pending_work(store, observer_thread_id, target_thread_id)
            .await?
    );
    Ok(())
}

async fn wait_for_no_pending_response_observation(
    store: &InMemoryThreadStore,
    observer_thread_id: ThreadId,
    target_thread_id: ThreadId,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if response_observation_has_no_pending_work(store, observer_thread_id, target_thread_id)
            .await?
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for durable response observation to become idle");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn response_observation_has_no_pending_work(
    store: &InMemoryThreadStore,
    observer_thread_id: ThreadId,
    target_thread_id: ThreadId,
) -> Result<bool> {
    let history = store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: observer_thread_id,
            include_archived: false,
        })
        .await?;
    let mut latest = HashMap::new();
    for item in history.items {
        if let RolloutItem::AgentResponseObservation(observation) = item
            && observation.observer_thread_id == observer_thread_id
            && observation.target_thread_id == target_thread_id
        {
            latest.insert(observation.target_turn_id.clone(), observation);
        }
    }
    Ok(!latest.is_empty()
        && latest.values().all(|observation| {
            !observation.pending_commentary
                && observation.commentary_after_sequences.is_empty()
                && observation.commentary_admissions.is_empty()
                && observation.commentary_delivery.is_none()
                && observation.final_delivery == AgentResponseFinalDelivery::None
        }))
}

async fn diagnostic_stage<T>(
    stage: &'static str,
    future: impl std::future::Future<Output = T>,
) -> T {
    tokio::time::timeout(Duration::from_secs(30), future)
        .await
        .unwrap_or_else(|_| panic!("timed out during {stage}"))
}

async fn wait_for_rollback_or_error(codex: &codex_core::CodexThread, stage: &'static str) {
    diagnostic_stage(stage, async {
        loop {
            let event = codex.next_event().await.expect("rollback event stream");
            match event.msg {
                EventMsg::ThreadRolledBack(_) => return,
                EventMsg::Error(error) => panic!("rollback failed: {}", error.message),
                _ => {}
            }
        }
    })
    .await;
}

async fn wait_for_completion_after_rollback(
    codex: &codex_core::CodexThread,
) -> (String, SubAgentCompletionStatus, String, String) {
    let mut rollback_completed = false;
    let mut completion = None;
    wait_for_event_with_timeout(
        codex,
        |event| {
            if matches!(event, EventMsg::ThreadRolledBack(_)) {
                rollback_completed = true;
            }
            if let Some(event) = sub_agent_completion_event(event) {
                assert!(
                    rollback_completed,
                    "completion must not cross a pending rollback reservation"
                );
                completion = Some(event);
            }
            rollback_completed && completion.is_some()
        },
        Duration::from_secs(/*secs*/ 30),
    )
    .await;
    completion.expect("completion event")
}

async fn resume_in_memory_thread_from_store(
    test: &TestCodex,
) -> Result<Arc<codex_core::CodexThread>> {
    let stored_thread = test
        .codex
        .read_thread(
            /*include_archived*/ false, /*include_history*/ true,
        )
        .await?;
    let thread_id = stored_thread.thread_id;
    let history = stored_thread
        .history
        .ok_or_else(|| anyhow::anyhow!("stored thread should include history"))?;
    test.thread_manager.remove_thread(&thread_id).await;
    let resumed = test
        .thread_manager
        .resume_thread_with_history(
            test.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: thread_id,
                history: Arc::new(history.items),
                rollout_path: None,
            }),
            codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key("test")),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?;
    Ok(resumed.thread)
}

async fn submit_turn_on_thread(codex: &codex_core::CodexThread, prompt: &str) -> Result<()> {
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: prompt.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let turn_id = wait_for_event_match(codex, |event| match event {
        EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
        _ => None,
    })
    .await;
    wait_for_event_match(codex, |event| match event {
        EventMsg::TurnComplete(event) if event.turn_id == turn_id => Some(()),
        _ => None,
    })
    .await;
    Ok(())
}

fn tool_parameter_description(tool: &Value, parameter_name: &str) -> Option<String> {
    tool.get("parameters")
        .and_then(|parameters| parameters.get("properties"))
        .and_then(|properties| properties.get(parameter_name))
        .and_then(|parameter| parameter.get("description"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn role_block(description: &str, role_name: &str) -> Option<String> {
    let role_header = format!("{role_name}: {{");
    let mut lines = description.lines().skip_while(|line| *line != role_header);
    let first_line = lines.next()?;
    let mut block = vec![first_line];
    for line in lines {
        if line.ends_with(": {") {
            break;
        }
        block.push(line);
    }
    Some(block.join("\n"))
}

fn write_home_skill(codex_home: &Path, dir: &str, name: &str, description: &str) -> Result<()> {
    let skill_dir = codex_home.join("skills").join(dir);
    fs::create_dir_all(&skill_dir)?;
    let contents = format!("---\nname: {name}\ndescription: {description}\n---\n\n# Body\n");
    fs::write(skill_dir.join("SKILL.md"), contents)?;
    Ok(())
}

fn write_subagent_lifecycle_hooks(
    home: &Path,
    stop_prompts: &[&str],
    subagent_stop_matcher: &str,
) -> Result<()> {
    let session_start_script_path = home.join("session_start_hook.py");
    let session_start_log_path = home.join("session_start_hook_log.jsonl");
    let session_start_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{session_start_log_path}")
payload = json.load(sys.stdin)
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
"#,
        session_start_log_path = session_start_log_path.display(),
    );

    let start_script_path = home.join("subagent_start_hook.py");
    let start_log_path = home.join("subagent_start_hook_log.jsonl");
    let start_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{start_log_path}")
payload = json.load(sys.stdin)
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
print(json.dumps({{"hookSpecificOutput": {{"hookEventName": "SubagentStart", "additionalContext": {SUBAGENT_START_CONTEXT:?}}}}}))
"#,
        start_log_path = start_log_path.display(),
    );

    let user_prompt_submit_script_path = home.join("user_prompt_submit_hook.py");
    let user_prompt_submit_log_path = home.join("user_prompt_submit_hook_log.jsonl");
    let user_prompt_submit_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{user_prompt_submit_log_path}")
payload = json.load(sys.stdin)
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
"#,
        user_prompt_submit_log_path = user_prompt_submit_log_path.display(),
    );

    let subagent_stop_script_path = home.join("subagent_stop_hook.py");
    let subagent_stop_log_path = home.join("subagent_stop_hook_log.jsonl");
    let prompts_json = serde_json::to_string(stop_prompts)?;
    let subagent_stop_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{subagent_stop_log_path}")
block_prompts = {prompts_json}

payload = json.load(sys.stdin)
existing = []
if log_path.exists():
    existing = [line for line in log_path.read_text(encoding="utf-8").splitlines() if line.strip()]

with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")

invocation_index = len(existing)
if invocation_index < len(block_prompts):
    print(json.dumps({{"decision": "block", "reason": block_prompts[invocation_index]}}))
else:
    print(json.dumps({{"systemMessage": f"subagent stop pass {{invocation_index + 1}} complete"}}))
"#,
        subagent_stop_log_path = subagent_stop_log_path.display(),
        prompts_json = prompts_json,
    );

    let stop_script_path = home.join("stop_hook.py");
    let stop_log_path = home.join("stop_hook_log.jsonl");
    let stop_script = format!(
        r#"import json
from pathlib import Path
import sys

log_path = Path(r"{stop_log_path}")
payload = json.load(sys.stdin)
with log_path.open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(payload) + "\n")
print(json.dumps({{"systemMessage": "root stop complete"}}))
"#,
        stop_log_path = stop_log_path.display(),
    );

    let hooks = serde_json::json!({
        "hooks": {
            "SessionStart": [{
                "matcher": "startup",
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", session_start_script_path.display()),
                }]
            }],
            "SubagentStart": [{
                "matcher": "worker",
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", start_script_path.display()),
                }]
            }],
            "UserPromptSubmit": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", user_prompt_submit_script_path.display()),
                }]
            }],
            "SubagentStop": [{
                "matcher": subagent_stop_matcher,
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", subagent_stop_script_path.display()),
                }]
            }],
            "Stop": [{
                "hooks": [{
                    "type": "command",
                    "command": format!("python3 {}", stop_script_path.display()),
                }]
            }]
        }
    });

    fs::write(&session_start_script_path, session_start_script)?;
    fs::write(&start_script_path, start_script)?;
    fs::write(&user_prompt_submit_script_path, user_prompt_submit_script)?;
    fs::write(&subagent_stop_script_path, subagent_stop_script)?;
    fs::write(&stop_script_path, stop_script)?;
    fs::write(home.join("hooks.json"), hooks.to_string())?;
    Ok(())
}

fn read_hook_log(home: &Path, filename: &str) -> Result<Vec<serde_json::Value>> {
    let path = home.join(filename);
    if !path.exists() {
        return Ok(Vec::new());
    }
    fs::read_to_string(path)?
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

async fn wait_for_hook_log(
    home: &Path,
    filename: &str,
    expected_len: usize,
) -> Result<Vec<serde_json::Value>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let inputs = read_hook_log(home, filename)?;
        if inputs.len() >= expected_len {
            return Ok(inputs);
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "expected at least {expected_len} entries in {filename}, got {}",
                inputs.len()
            );
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_spawned_thread_id(test: &TestCodex) -> Result<String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let ids = test.thread_manager.list_thread_ids().await;
        if let Some(spawned_id) = ids
            .iter()
            .find(|id| **id != test.session_configured.thread_id)
        {
            return Ok(spawned_id.to_string());
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for spawned thread id");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_requests(
    mock: &core_test_support::responses::ResponseMock,
) -> Result<Vec<ResponsesRequest>> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let requests = mock.requests();
        if !requests.is_empty() {
            return Ok(requests);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("expected at least 1 request, got {}", requests.len());
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn wait_for_request_with_model(
    mock: &core_test_support::responses::ResponseMock,
    model: &str,
) -> Result<ResponsesRequest> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if let Some(request) = mock
            .requests()
            .into_iter()
            .find(|request| request.body_json()["model"] == model)
        {
            return Ok(request);
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for request using model {model}");
        }
        sleep(Duration::from_millis(10)).await;
    }
}

async fn setup_turn_one_with_spawned_child(
    server: &MockServer,
    child_response_timing: ChildResponseTiming,
    history_mode: ThreadHistoryMode,
) -> Result<(TestCodex, String)> {
    let (test, spawned_id, _child_request_log) = setup_turn_one_with_custom_spawned_child(
        server,
        json!({
            "message": CHILD_PROMPT,
        }),
        child_response_timing,
        /*wait_for_parent_notification*/ true,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    Ok((test, spawned_id))
}

async fn setup_turn_one_with_custom_spawned_child(
    server: &MockServer,
    spawn_args: serde_json::Value,
    child_response_timing: ChildResponseTiming,
    wait_for_parent_notification: bool,
    configure_test: impl FnOnce(
        core_test_support::test_codex::TestCodexBuilder,
    ) -> core_test_support::test_codex::TestCodexBuilder,
) -> Result<(
    TestCodex,
    String,
    core_test_support::responses::ResponseMock,
)> {
    let spawn_args = serde_json::to_string(&spawn_args)?;

    mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await?;

    let child_sse = sse(vec![
        ev_response_created("resp-child-1"),
        ev_assistant_message("msg-child-1", "child done"),
        ev_completed("resp-child-1"),
    ]);
    let wait_for_initial_notification =
        matches!(&child_response_timing, ChildResponseTiming::Immediate);
    let child_request_log = match child_response_timing {
        ChildResponseTiming::Immediate => {
            mount_sse_once_match(
                server,
                |req: &wiremock::Request| {
                    body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
                },
                child_sse,
            )
            .await
        }
        ChildResponseTiming::Delayed(delay) => {
            mount_response_once_match(
                server,
                |req: &wiremock::Request| {
                    body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
                },
                sse_response(child_sse).set_delay(delay),
            )
            .await
        }
        ChildResponseTiming::Gated(gate_rx) => {
            mount_responder_once_match(
                server,
                |req: &wiremock::Request| {
                    body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
                },
                GatedSseResponse {
                    gate_rx: Mutex::new(Some(gate_rx)),
                    response: child_sse,
                },
            )
            .await
        }
    };

    let _turn1_followup = mount_sse_once_match(
        server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let mut builder = configure_test(test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config.model = Some(INHERITED_MODEL.to_string());
        config.model_reasoning_effort = Some(INHERITED_REASONING_EFFORT);
    }));
    let test = builder.build_with_auto_env(server).await?;
    test.submit_turn(TURN_1_PROMPT).await?;
    if wait_for_initial_notification && wait_for_parent_notification {
        let _ = wait_for_requests(&child_request_log).await?;
        let rollout_path = test
            .codex
            .rollout_path()
            .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
        let deadline = Instant::now() + Duration::from_secs(6);
        loop {
            let has_notification = tokio::fs::read_to_string(&rollout_path)
                .await
                .is_ok_and(|rollout| rollout.contains("<subagent_notification>"));
            if has_notification {
                break;
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "timed out waiting for parent rollout to include subagent notification"
                );
            }
            sleep(Duration::from_millis(10)).await;
        }
    }
    let spawned_id = wait_for_spawned_thread_id(&test).await?;

    Ok((test, spawned_id, child_request_log))
}

async fn spawn_child_and_capture_snapshot(
    server: &MockServer,
    spawn_args: serde_json::Value,
    configure_test: impl FnOnce(
        core_test_support::test_codex::TestCodexBuilder,
    ) -> core_test_support::test_codex::TestCodexBuilder,
) -> Result<ThreadConfigSnapshot> {
    let (test, spawned_id, _child_request_log) = setup_turn_one_with_custom_spawned_child(
        server,
        spawn_args,
        ChildResponseTiming::Immediate,
        /*wait_for_parent_notification*/ true,
        configure_test,
    )
    .await?;
    let thread_id = ThreadId::from_string(&spawned_id)?;
    Ok(test
        .thread_manager
        .get_thread(thread_id)
        .await?
        .config_snapshot()
        .await)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_start_replaces_session_start_and_injects_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "child",
        "agent_type": "worker",
    }))?;

    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT)
                && body_contains(req, SUBAGENT_START_CONTEXT)
                && !body_contains(req, "<subagent_notification>")
                && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_pre_build_hook(|home| {
            write_subagent_lifecycle_hooks(home, /*stop_prompts*/ &[], "worker")
                .expect("failed to write subagent hook fixture");
        })
        .with_config(|config| {
            trust_discovered_hooks(config);
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    test.submit_turn(TURN_1_PROMPT).await?;
    let _ = wait_for_requests(&child_request_log).await?;

    let start_inputs = wait_for_hook_log(
        test.codex_home_path(),
        "subagent_start_hook_log.jsonl",
        /*expected_len*/ 1,
    )
    .await?;
    assert_eq!(start_inputs.len(), 1);
    assert_eq!(start_inputs[0]["agent_type"].as_str(), Some("worker"));
    let spawned_id = wait_for_spawned_thread_id(&test).await?;
    assert_eq!(
        start_inputs[0]["agent_id"].as_str(),
        Some(spawned_id.as_str())
    );

    let user_prompt_submit_inputs = wait_for_hook_log(
        test.codex_home_path(),
        "user_prompt_submit_hook_log.jsonl",
        /*expected_len*/ 2,
    )
    .await?;
    let parent_prompt_input = user_prompt_submit_inputs
        .iter()
        .find(|input| input["prompt"].as_str() == Some(TURN_1_PROMPT))
        .expect("parent prompt submit hook input should be logged");
    assert_eq!(parent_prompt_input.get("agent_id"), None);
    assert_eq!(parent_prompt_input.get("agent_type"), None);

    let child_prompt_input = user_prompt_submit_inputs
        .iter()
        .find(|input| input["prompt"].as_str() == Some(CHILD_PROMPT))
        .expect("child prompt submit hook input should be logged");
    assert_eq!(
        child_prompt_input["agent_id"].as_str(),
        Some(spawned_id.as_str())
    );
    assert_eq!(child_prompt_input["agent_type"].as_str(), Some("worker"));

    let session_start_inputs = wait_for_hook_log(
        test.codex_home_path(),
        "session_start_hook_log.jsonl",
        /*expected_len*/ 1,
    )
    .await?;
    assert_eq!(session_start_inputs.len(), 1);
    assert_eq!(session_start_inputs[0]["source"].as_str(), Some("startup"));
    assert_ne!(
        session_start_inputs[0]["session_id"].as_str(),
        Some(spawned_id.as_str())
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_stop_replaces_stop_and_skips_internal_subagents() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_name": "child",
        "agent_type": "worker",
    }))?;

    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let first_child_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done first"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    let second_child_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SUBAGENT_STOP_CONTINUATION) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-2"),
            ev_assistant_message("msg-child-2", "child done final"),
            ev_completed("resp-child-2"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;
    let internal_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, INTERNAL_SUBAGENT_PROMPT),
        sse(vec![
            ev_response_created("resp-internal-1"),
            ev_assistant_message("msg-internal-1", "internal subagent done"),
            ev_completed("resp-internal-1"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_pre_build_hook(|home| {
            write_subagent_lifecycle_hooks(
                home,
                /*stop_prompts*/ &[SUBAGENT_STOP_CONTINUATION],
                "",
            )
            .expect("failed to write subagent hook fixture");
        })
        .with_config(|config| {
            trust_discovered_hooks(config);
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        })
        .build(&server)
        .await?;

    test.submit_turn(TURN_1_PROMPT).await?;
    let _ = wait_for_requests(&first_child_request).await?;
    let _ = wait_for_requests(&second_child_request).await?;

    let subagent_stop_inputs = wait_for_hook_log(
        test.codex_home_path(),
        "subagent_stop_hook_log.jsonl",
        /*expected_len*/ 2,
    )
    .await?;
    assert_eq!(subagent_stop_inputs.len(), 2);
    assert_eq!(
        subagent_stop_inputs
            .iter()
            .map(|input| input["stop_hook_active"].as_bool())
            .collect::<Vec<_>>(),
        vec![Some(false), Some(true)]
    );
    assert_eq!(
        subagent_stop_inputs[0]["agent_type"].as_str(),
        Some("worker")
    );
    let parent_transcript_path = subagent_stop_inputs[0]["transcript_path"]
        .as_str()
        .expect("SubagentStop should include parent transcript_path");
    let agent_transcript_path = subagent_stop_inputs[0]["agent_transcript_path"]
        .as_str()
        .expect("SubagentStop should include agent_transcript_path");
    assert_ne!(parent_transcript_path, agent_transcript_path);
    assert_eq!(
        subagent_stop_inputs[1]["transcript_path"].as_str(),
        Some(parent_transcript_path)
    );
    assert_eq!(
        subagent_stop_inputs[1]["agent_transcript_path"].as_str(),
        Some(agent_transcript_path)
    );
    assert_eq!(
        subagent_stop_inputs[0]["last_assistant_message"].as_str(),
        Some("child done first")
    );

    let stop_inputs = read_hook_log(test.codex_home_path(), "stop_hook_log.jsonl")?;
    assert!(
        stop_inputs
            .iter()
            .all(|input| input["last_assistant_message"].as_str() != Some("child done first")),
        "child completion should not invoke the normal Stop hook"
    );
    let stop_input_count = stop_inputs.len();

    // This matcher would catch the old synthetic "review" SubagentStop target
    // because the SubagentStop hook above intentionally matches all agent types.
    let internal_thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::SubAgent(SubAgentSource::Review)),
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;

    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.cwd_path());
    internal_thread
        .thread
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: INTERNAL_SUBAGENT_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(local_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                model: Some(internal_thread.session_configured.model.clone()),
                ..Default::default()
            },
        })
        .await?;
    let turn_id = wait_for_event_match(internal_thread.thread.as_ref(), |event| match event {
        EventMsg::TurnStarted(event) => Some(event.turn_id.clone()),
        _ => None,
    })
    .await;
    wait_for_event_match(internal_thread.thread.as_ref(), |event| match event {
        EventMsg::TurnComplete(event) if event.turn_id == turn_id => Some(()),
        _ => None,
    })
    .await;
    let requests = wait_for_requests(&internal_request).await?;
    assert_eq!(requests.len(), 1);

    let subagent_stop_inputs_after_internal =
        read_hook_log(test.codex_home_path(), "subagent_stop_hook_log.jsonl")?;
    assert_eq!(subagent_stop_inputs_after_internal, subagent_stop_inputs);

    let stop_inputs_after_internal = read_hook_log(test.codex_home_path(), "stop_hook_log.jsonl")?;
    assert_eq!(stop_inputs_after_internal.len(), stop_input_count);

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_completion_waits_for_pending_rollback_and_survives_cold_resume(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "start child",
        "task_message": "audit child",
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-spawn-during-rollback"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-spawn-during-rollback"),
        ]),
    )
    .await;
    let child_request = mount_response_once_match(
        &server,
        |req: &wiremock::Request| request_has_agent_message_route(req, "/root", "/root/worker"),
        sse_response(sse(vec![
            ev_response_created("resp-child-during-rollback"),
            ev_assistant_message("msg-child-during-rollback", "child done"),
            ev_completed("resp-child-during-rollback"),
        ]))
        .set_delay(Duration::from_secs(/*secs*/ 5)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID)
                && body_contains(req, "\"type\":\"function_call_output\"")
        },
        sse(vec![
            ev_response_created("resp-parent-finished-before-rollback"),
            ev_assistant_message("msg-parent-finished-before-rollback", "parent done"),
            ev_completed("resp-parent-finished-before-rollback"),
        ]),
    )
    .await;
    let notification =
        "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\nchild done";
    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-v2-racing-rollback-resume"),
            ev_assistant_message("msg-v2-racing-rollback-resume", "completion retained"),
            ev_completed("resp-v2-racing-rollback-resume"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_model("koffing")
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        });
    let initial = builder.build(&server).await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;

    initial.submit_turn(TURN_1_PROMPT).await?;
    let child_thread_id = ThreadId::from_string(&wait_for_spawned_thread_id(&initial).await?)?;
    let child_thread = initial.thread_manager.get_thread(child_thread_id).await?;
    let _ = wait_for_requests(&child_request).await?;
    let durable_context_permit = initial.codex.acquire_durable_context_permit().await?;
    initial
        .codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    assert_eq!(
        wait_for_terminal_status(child_thread.as_ref()).await?,
        AgentStatus::Completed(Some("child done".to_string()))
    );
    drop(durable_context_permit);

    let completion = wait_for_completion_after_rollback(&initial.codex).await;
    assert_eq!(
        completion,
        (
            completion.0.clone(),
            SubAgentCompletionStatus::Completed,
            "/root/worker".to_string(),
            "child done".to_string(),
        )
    );
    initial.codex.flush_rollout().await?;
    let rollout_items = read_test_rollout_items(&initial)?;
    let effective_history = rollout_without_exact_rollback_ranges(&rollout_items);
    assert_eq!(
        effective_history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::ResponseItem(ResponseItem::AgentMessage { id: Some(id), .. })
                        if is_sub_agent_completion_context_response_item_id(id)
                )
            })
            .count(),
        1
    );
    assert_eq!(
        effective_history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
                )
            })
            .count(),
        1
    );

    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    builder = builder
        .with_model("koffing")
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        });
    let resumed = builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn(TURN_AFTER_RESUME_PROMPT).await?;

    let expected_agent_messages = vec![json!({
        "type": "agent_message",
        "author": "/root/worker",
        "recipient": "/root",
        "content": [{
            "type": "input_text",
            "text": notification,
        }],
    })];
    let request = wait_for_agent_messages(
        &resumed_request,
        &expected_agent_messages,
        "expected one v2 completion after pending rollback and cold resume",
    )
    .await?;
    assert_eq!(
        normalize_agent_messages(request.inputs_of_type("agent_message")),
        normalize_agent_messages(expected_agent_messages)
    );
    assert_input_item_ids_are_provider_compatible(&request);

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subagent_notification_is_included_without_wait(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (child_gate_tx, child_gate_rx) = mpsc::channel();
    let (test, spawned_id) = setup_turn_one_with_spawned_child(
        &server,
        ChildResponseTiming::Gated(child_gate_rx),
        history_mode,
    )
    .await?;

    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    let started_id = wait_for_event_match(&test.codex, sub_agent_completion_started_id).await;
    let expected_completion = (
        started_id,
        SubAgentCompletionStatus::Completed,
        spawned_id,
        "child done".to_string(),
    );
    assert_eq!(
        wait_for_event_match(&test.codex, sub_agent_completion_event).await,
        expected_completion
    );
    test.codex.flush_rollout().await?;
    let history = read_test_rollout_items(&test)?;
    let completion_index = history
        .iter()
        .position(|item| {
            matches!(
                item,
                RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
            )
        })
        .expect("persisted completion");
    let RolloutItem::EventMsg(EventMsg::ItemCompleted(completed)) = &history[completion_index]
    else {
        unreachable!("located completion item");
    };
    assert!(matches!(
        history.get(completion_index.wrapping_sub(1)),
        Some(RolloutItem::EventMsg(EventMsg::TurnStarted(started)))
            if started.turn_id == completed.turn_id
    ));
    assert!(matches!(
        history.get(completion_index + 1),
        Some(RolloutItem::EventMsg(EventMsg::TurnComplete(finished)))
            if finished.turn_id == completed.turn_id
    ));
    let persisted_completions = history
        .iter()
        .filter_map(|item| match item {
            RolloutItem::EventMsg(event) => sub_agent_completion_event(event),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(persisted_completions, vec![expected_completion]);

    let turn2 = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_2_NO_WAIT_PROMPT),
        sse(vec![
            ev_response_created("resp-turn2-1"),
            ev_assistant_message("msg-turn2-1", "no wait path"),
            ev_completed("resp-turn2-1"),
        ]),
    )
    .await;
    test.submit_turn(TURN_2_NO_WAIT_PROMPT).await?;

    let turn2_requests = wait_for_requests(&turn2).await?;
    assert!(turn2_requests.iter().any(has_subagent_notification));
    turn2_requests
        .iter()
        .for_each(assert_input_item_ids_are_provider_compatible);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_subagent_notification_survives_an_active_parent_turn_abort() -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-spawn"),
        ]),
    )
    .await;
    let blocked_parent = mount_response_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse_response(sse(vec![
            ev_response_created("resp-parent-blocked"),
            ev_assistant_message("msg-parent-blocked", "should be interrupted"),
            ev_completed("resp-parent-blocked"),
        ]))
        .set_delay(Duration::from_secs(60)),
    )
    .await;
    let child_request = mount_response_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse_response(sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", "child done"),
            ev_completed("resp-child"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    let follow_up = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_ABORT_PROMPT),
        sse(vec![
            ev_response_created("resp-after-abort"),
            ev_assistant_message("msg-after-abort", "notification retained"),
            ev_completed("resp-after-abort"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, test.config.cwd.as_path());

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: TURN_1_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: codex_protocol::protocol::ThreadSettingsOverrides {
                environments: Some(test.default_environment_selections(test.config.cwd.clone())),
                approval_policy: Some(AskForApproval::Never),
                sandbox_policy: Some(sandbox_policy),
                permission_profile,
                model: Some(test.session_configured.model.clone()),
                ..Default::default()
            },
        })
        .await?;
    let _ = wait_for_requests(&blocked_parent).await?;
    let _ = wait_for_requests(&child_request).await?;
    let _ = wait_for_event_match(&test.codex, sub_agent_completion_event).await;

    test.codex.submit(Op::Interrupt).await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnAborted(_)).then_some(())
    })
    .await;
    test.submit_turn(TURN_AFTER_ABORT_PROMPT).await?;

    let follow_up_request =
        wait_for_request_containing_text(&follow_up, TURN_AFTER_ABORT_PROMPT).await?;
    assert!(
        has_subagent_notification(&follow_up_request),
        "expected the child notification in the first request after abort"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_terminal_published_before_shutdown_survives_resume() -> Result<()> {
    let server = start_mock_server().await;
    let notification_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-after-resume"),
            ev_assistant_message("msg-after-resume", "completion retained"),
            ev_completed("resp-after-resume"),
        ]),
    )
    .await;
    let (child_gate_tx, child_gate_rx) = mpsc::channel();
    let (initial, spawned_id) = setup_turn_one_with_spawned_child(
        &server,
        ChildResponseTiming::Gated(child_gate_rx),
        ThreadHistoryMode::Legacy,
    )
    .await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    let child_thread = initial
        .thread_manager
        .get_thread(ThreadId::from_string(&spawned_id)?)
        .await?;
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            child_thread.agent_status().await,
            codex_protocol::protocol::AgentStatus::Completed(_)
                | codex_protocol::protocol::AgentStatus::Errored(_)
                | codex_protocol::protocol::AgentStatus::Shutdown
                | codex_protocol::protocol::AgentStatus::NotFound
        ) {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for child terminal publication");
        }
        tokio::task::yield_now().await;
    }

    initial.codex.submit(Op::Shutdown).await?;
    let started_id = wait_for_event_match(&initial.codex, sub_agent_completion_started_id).await;
    assert_eq!(
        wait_for_event_match(&initial.codex, sub_agent_completion_event).await,
        (
            started_id,
            SubAgentCompletionStatus::Completed,
            spawned_id,
            "child done".to_string(),
        )
    );
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    let mut resume_builder = test_codex()
        .with_model(INHERITED_MODEL)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn(TURN_AFTER_RESUME_PROMPT).await?;

    let notification_request =
        wait_for_request_containing_text(&notification_request, TURN_AFTER_RESUME_PROMPT).await?;
    assert!(
        has_subagent_notification(&notification_request),
        "expected the pre-shutdown v1 completion in resumed model context"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_watcher_retries_canonical_completion_before_shutdown_and_replay() -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let configured_store_id = store_id.clone();
    let (child_gate_tx, child_gate_rx) = mpsc::channel();
    let (initial, spawned_id, _child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
        }),
        ChildResponseTiming::Gated(child_gate_rx),
        /*wait_for_parent_notification*/ false,
        move |builder| {
            builder.with_config(move |config| {
                config.experimental_thread_store = ThreadStoreConfig::InMemory {
                    id: configured_store_id,
                };
            })
        },
    )
    .await?;
    let child_thread = initial
        .thread_manager
        .get_thread(ThreadId::from_string(&spawned_id)?)
        .await?;
    store.fail_next_sub_agent_completion_append().await;
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;

    assert_eq!(
        wait_for_terminal_status(child_thread.as_ref()).await?,
        AgentStatus::Completed(Some("child done".to_string()))
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while store.sub_agent_completion_append_attempt_ids().await.len() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("canonical completion retry should reach the persistence gate");

    let attempted_ids = store.sub_agent_completion_append_attempt_ids().await;
    assert_eq!(attempted_ids.len(), 2);
    assert_eq!(attempted_ids[0], attempted_ids[1]);
    while let Some(event) = initial.codex.try_next_event()? {
        assert!(sub_agent_completion_event(&event.msg).is_none());
    }
    let history_after_failure = initial
        .codex
        .load_history(/*include_archived*/ false)
        .await?;
    assert_eq!(
        history_after_failure
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
                )
            })
            .count(),
        0
    );

    let shutdown_codex = initial.codex.clone();
    let mut shutdown = tokio::spawn(async move { shutdown_codex.submit(Op::Shutdown).await });
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut shutdown)
            .await
            .is_err(),
        "shutdown should wait for the accepted completion retry"
    );
    store.release_sub_agent_completion_retry().await;

    let started_id = wait_for_event_match(&initial.codex, sub_agent_completion_started_id).await;
    assert_eq!(
        wait_for_event_match(&initial.codex, sub_agent_completion_event).await,
        (
            started_id.clone(),
            SubAgentCompletionStatus::Completed,
            spawned_id,
            "child done".to_string(),
        )
    );
    shutdown.await??;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    assert_eq!(
        store.sub_agent_completion_append_attempt_ids().await,
        vec![started_id.clone(), started_id]
    );
    let durable_history = initial
        .codex
        .load_history(/*include_archived*/ false)
        .await?;
    assert_eq!(
        durable_history
            .items
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
                )
            })
            .count(),
        1
    );

    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-after-retry-resume"),
            ev_assistant_message("msg-after-retry-resume", "completion retained"),
            ev_completed("resp-after-retry-resume"),
        ]),
    )
    .await;
    let resumed = resume_in_memory_thread_from_store(&initial).await?;
    submit_turn_on_thread(resumed.as_ref(), TURN_AFTER_RESUME_PROMPT).await?;
    let resumed_request =
        wait_for_request_containing_text(&resumed_request, TURN_AFTER_RESUME_PROMPT).await?;
    assert_eq!(
        resumed_request
            .message_input_texts("user")
            .iter()
            .filter(|text| text.contains("<subagent_notification>"))
            .count(),
        1
    );
    InMemoryThreadStore::remove_id(&store_id);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_watcher_releases_completion_when_rollback_requires_reload() -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let configured_store_id = store_id.clone();
    let (child_gate_tx, child_gate_rx) = mpsc::channel();
    let (initial, spawned_id, _child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "x",
        }),
        ChildResponseTiming::Gated(child_gate_rx),
        /*wait_for_parent_notification*/ false,
        move |builder| {
            builder.with_config(move |config| {
                config.experimental_thread_store = ThreadStoreConfig::InMemory {
                    id: configured_store_id,
                };
            })
        },
    )
    .await?;
    let child_thread = initial
        .thread_manager
        .get_thread(ThreadId::from_string(&spawned_id)?)
        .await?;
    let durable_context_permit = initial.codex.acquire_durable_context_permit().await?;
    store
        .fail_next_operation(InMemoryThreadStoreFailure::ThreadRollbackFlush)
        .await;

    initial
        .codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    assert_eq!(
        wait_for_terminal_status(child_thread.as_ref()).await?,
        AgentStatus::Completed(Some("child done".to_string()))
    );
    drop(durable_context_permit);

    wait_for_event_match(&initial.codex, |event| match event {
        EventMsg::Error(error)
            if error.codex_error_info == Some(CodexErrorInfo::ThreadRollbackCommitUnknown) =>
        {
            Some(())
        }
        _ => None,
    })
    .await;
    tokio::time::timeout(
        Duration::from_secs(5),
        initial.codex.wait_until_terminated(),
    )
    .await
    .expect("reload-required session should terminate after rejecting the completion");

    assert!(
        store
            .sub_agent_completion_append_attempt_ids()
            .await
            .is_empty()
    );
    let durable_history = initial
        .codex
        .load_history(/*include_archived*/ false)
        .await?;
    let durable_history_json = serde_json::to_string(&durable_history.items)?;
    assert!(!durable_history_json.contains("<subagent_notification>"));
    assert!(!durable_history.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
        )
    }));

    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-after-reload-required-resume"),
            ev_assistant_message("msg-after-reload-required-resume", "completion rejected"),
            ev_completed("resp-after-reload-required-resume"),
        ]),
    )
    .await;
    let resumed = resume_in_memory_thread_from_store(&initial).await?;
    submit_turn_on_thread(resumed.as_ref(), TURN_AFTER_RESUME_PROMPT).await?;
    let resumed_request =
        wait_for_request_containing_text(&resumed_request, TURN_AFTER_RESUME_PROMPT).await?;
    assert_eq!(
        resumed_request
            .message_input_texts("user")
            .iter()
            .filter(|text| text.contains("<subagent_notification>"))
            .count(),
        0
    );
    InMemoryThreadStore::remove_id(&store_id);

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_accepted_completion_survives_exact_rollback_and_cold_resume(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-after-rollback-resume"),
            ev_assistant_message("msg-after-rollback-resume", "completion retained"),
            ev_completed("resp-after-rollback-resume"),
        ]),
    )
    .await;
    let (child_gate_tx, child_gate_rx) = mpsc::channel();
    let (initial, spawned_id) = setup_turn_one_with_spawned_child(
        &server,
        ChildResponseTiming::Gated(child_gate_rx),
        history_mode,
    )
    .await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    let started_id = wait_for_event_match(&initial.codex, sub_agent_completion_started_id).await;
    let completion = wait_for_event_match(&initial.codex, sub_agent_completion_event).await;
    let expected_completion = (
        started_id,
        SubAgentCompletionStatus::Completed,
        spawned_id,
        "child done".to_string(),
    );
    assert_eq!(
        completion, expected_completion,
        "completion item should carry its generated canonical identity"
    );

    initial
        .codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ThreadRolledBack(_)).then_some(())
    })
    .await;
    initial.codex.flush_rollout().await?;

    let rollout_items = read_test_rollout_items(&initial)?;
    let effective_history = rollout_without_exact_rollback_ranges(&rollout_items);
    assert!(effective_history.iter().any(|item| {
        matches!(
            item,
            RolloutItem::ResponseItem(ResponseItem::Message { id: Some(id), .. })
                if is_sub_agent_completion_context_response_item_id(id)
        )
    }));
    assert_eq!(
        effective_history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
                )
            })
            .count(),
        1
    );

    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    let mut resume_builder = test_codex()
        .with_model(INHERITED_MODEL)
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn(TURN_AFTER_RESUME_PROMPT).await?;

    let resumed_request =
        wait_for_request_containing_text(&resumed_request, TURN_AFTER_RESUME_PROMPT).await?;
    assert!(
        has_subagent_notification(&resumed_request),
        "expected the accepted v1 completion after exact rollback and cold resume"
    );

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_completion_waits_for_pending_rollback_and_survives_cold_resume(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-after-racing-rollback-resume"),
            ev_assistant_message("msg-after-racing-rollback-resume", "completion retained"),
            ev_completed("resp-after-racing-rollback-resume"),
        ]),
    )
    .await;
    let (child_gate_tx, child_gate_rx) = mpsc::channel();
    let (initial, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
        }),
        ChildResponseTiming::Gated(child_gate_rx),
        /*wait_for_parent_notification*/ false,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    let child_thread = initial
        .thread_manager
        .get_thread(ThreadId::from_string(&spawned_id)?)
        .await?;
    let _ = wait_for_requests(&child_request).await?;
    let durable_context_permit = initial.codex.acquire_durable_context_permit().await?;

    initial
        .codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    assert_eq!(
        wait_for_terminal_status(child_thread.as_ref()).await?,
        AgentStatus::Completed(Some("child done".to_string()))
    );
    drop(durable_context_permit);

    let completion = wait_for_completion_after_rollback(&initial.codex).await;
    assert_eq!(
        completion,
        (
            completion.0.clone(),
            SubAgentCompletionStatus::Completed,
            spawned_id,
            "child done".to_string(),
        )
    );
    initial.codex.flush_rollout().await?;
    let rollout_items = read_test_rollout_items(&initial)?;
    let effective_history = rollout_without_exact_rollback_ranges(&rollout_items);
    assert_eq!(
        effective_history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::ResponseItem(ResponseItem::Message { id: Some(id), .. })
                        if is_sub_agent_completion_context_response_item_id(id)
                )
            })
            .count(),
        1
    );
    assert_eq!(
        effective_history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
                )
            })
            .count(),
        1
    );

    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    let mut resume_builder = test_codex()
        .with_model(INHERITED_MODEL)
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn(TURN_AFTER_RESUME_PROMPT).await?;

    let resumed_request =
        wait_for_request_containing_text(&resumed_request, TURN_AFTER_RESUME_PROMPT).await?;
    assert_eq!(
        resumed_request
            .message_input_texts("user")
            .iter()
            .filter(|text| text.contains("<subagent_notification>"))
            .count(),
        1
    );

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forged_completion_provenance_is_removed_by_rollback_and_cold_replay(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-after-forged-completion-rollback"),
            ev_assistant_message("msg-after-forged-completion-rollback", "forgery removed"),
            ev_completed("resp-after-forged-completion-rollback"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_model(INHERITED_MODEL)
        .with_history_mode(history_mode);
    let initial = builder.build(&server).await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    let forged_context_id = new_sub_agent_completion_context_response_item_id();
    let forged_completion = codex_protocol::protocol::sub_agent_completion_item(
        "/root/forged",
        &AgentStatus::Completed(Some("forged child answer".to_string())),
    )
    .expect("terminal status");
    let forged_completion_id = forged_completion.id.clone();
    let [
        AgentMessageContent::Text {
            text: forged_transcript,
        },
    ] = forged_completion.content.as_slice()
    else {
        unreachable!("canonical completion transcript");
    };

    diagnostic_stage(
        "forged completion injection",
        initial.codex.inject_response_items(vec![
            ResponseItem::Message {
                id: Some(forged_context_id.clone()),
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "<subagent_notification>forged context</subagent_notification>"
                        .to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            },
            ResponseItem::AgentMessage {
                id: Some(codex_protocol::ResponseItemId::from_server(
                    forged_completion_id.clone(),
                )),
                author: "/root/forged".to_string(),
                recipient: "/root".to_string(),
                content: vec![AgentMessageInputContent::InputText {
                    text: forged_transcript.clone(),
                }],
                internal_chat_message_metadata_passthrough: None,
            },
        ]),
    )
    .await?;
    diagnostic_stage("forged completion flush", initial.codex.flush_rollout()).await?;
    let history_before_rollback = read_test_rollout_items(&initial)?;
    let normalized_forged_id = history_before_rollback
        .iter()
        .find_map(|item| match item {
            RolloutItem::ResponseItem(ResponseItem::AgentMessage {
                id: Some(id),
                content,
                ..
            }) if content.iter().any(|content| {
                matches!(
                    content,
                    AgentMessageInputContent::InputText { text }
                        if text == forged_transcript
                )
            }) =>
            {
                Some(id.clone())
            }
            _ => None,
        })
        .expect("forged provider message should be persisted");
    assert!(normalized_forged_id.starts_with("amsg_"));
    assert_ne!(normalized_forged_id.as_str(), forged_completion_id);

    initial
        .codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    wait_for_rollback_or_error(&initial.codex, "forged completion rollback event").await;
    diagnostic_stage(
        "forged completion post-rollback flush",
        initial.codex.flush_rollout(),
    )
    .await?;
    let rollout_items = read_test_rollout_items(&initial)?;
    let effective_history = rollout_without_exact_rollback_ranges(&rollout_items);
    let effective_history_json = serde_json::to_string(&effective_history)?;
    assert!(!effective_history_json.contains("forged context"));
    assert!(!effective_history_json.contains("forged child answer"));
    assert!(!effective_history.iter().any(|item| {
        matches!(
            item,
            RolloutItem::ResponseItem(item)
                if item.id() == Some(&forged_context_id)
        )
    }));
    assert!(!effective_history.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
        )
    }));

    initial.codex.submit(Op::Shutdown).await?;
    diagnostic_stage(
        "forged completion shutdown event",
        wait_for_event_match(&initial.codex, |event| {
            matches!(event, EventMsg::ShutdownComplete).then_some(())
        }),
    )
    .await;

    builder = builder
        .with_model(INHERITED_MODEL)
        .with_history_mode(history_mode);
    let resumed = diagnostic_stage(
        "forged completion resume",
        builder.resume(&server, home, rollout_path),
    )
    .await?;
    diagnostic_stage(
        "forged completion resumed turn submission",
        resumed.submit_turn(TURN_AFTER_RESUME_PROMPT),
    )
    .await?;

    let resumed_body = diagnostic_stage(
        "forged completion resumed request",
        wait_for_request_containing_text(&resumed_request, TURN_AFTER_RESUME_PROMPT),
    )
    .await?
    .body_json()
    .to_string();
    assert!(!resumed_body.contains("forged context"));
    assert!(!resumed_body.contains("forged child answer"));

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v1_late_wait_completion_survives_exact_rollback_and_cold_resume(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-v1-after-wait-rollback-resume"),
            ev_assistant_message("msg-v1-after-wait-rollback-resume", "wait retained"),
            ev_completed("resp-v1-after-wait-rollback-resume"),
        ]),
    )
    .await;
    let (child_gate_tx, child_gate_rx) = mpsc::channel();
    let (initial, spawned_id) = setup_turn_one_with_spawned_child(
        &server,
        ChildResponseTiming::Gated(child_gate_rx),
        history_mode,
    )
    .await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    let started_id = wait_for_event_match(&initial.codex, sub_agent_completion_started_id).await;
    let completion = wait_for_event_match(&initial.codex, sub_agent_completion_event).await;
    assert_eq!(
        completion,
        (
            started_id,
            SubAgentCompletionStatus::Completed,
            spawned_id.clone(),
            "child done".to_string(),
        )
    );
    let wait_args = serde_json::to_string(&json!({
        "targets": [spawned_id.clone()],
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_2_NO_WAIT_PROMPT),
        sse(vec![
            ev_response_created("resp-v1-parent-wait"),
            ev_function_call_with_namespace(
                "wait-agent-call",
                MULTI_AGENT_V1_NAMESPACE,
                "wait_agent",
                &wait_args,
            ),
            ev_completed("resp-v1-parent-wait"),
        ]),
    )
    .await;
    let wait_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, "wait-agent-call")
                && body_contains(req, "\"type\":\"function_call_output\"")
        },
        sse(vec![
            ev_response_created("resp-v1-parent-after-wait"),
            ev_assistant_message("msg-v1-parent-after-wait", "done"),
            ev_completed("resp-v1-parent-after-wait"),
        ]),
    )
    .await;

    initial.submit_turn(TURN_2_NO_WAIT_PROMPT).await?;
    let wait_followup =
        wait_for_request_containing_text(&wait_followup, TURN_2_NO_WAIT_PROMPT).await?;
    assert!(has_subagent_notification(&wait_followup));
    initial.codex.flush_rollout().await?;
    let history = read_test_rollout_items(&initial)?;
    let wait_agent_states = history
        .iter()
        .find_map(|item| match item {
            RolloutItem::EventMsg(event) => completed_wait_agent_states(event),
            _ => None,
        })
        .expect("persisted completed wait");
    assert_eq!(
        wait_agent_states,
        [(
            ThreadId::from_string(&spawned_id)?,
            AgentStatus::Completed(Some("child done".to_string())),
        )]
        .into_iter()
        .collect()
    );
    assert_eq!(
        history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
                )
            })
            .count(),
        1
    );

    initial
        .codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ThreadRolledBack(_)).then_some(())
    })
    .await;
    initial.codex.flush_rollout().await?;
    let rollout_items = read_test_rollout_items(&initial)?;
    let effective_history = rollout_without_exact_rollback_ranges(&rollout_items);
    assert_eq!(
        effective_history.iter().find_map(|item| match item {
            RolloutItem::EventMsg(event) => completed_wait_agent_states(event),
            _ => None,
        }),
        Some(wait_agent_states)
    );
    assert_eq!(
        effective_history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
                )
            })
            .count(),
        1
    );

    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    let mut resume_builder = test_codex()
        .with_model(INHERITED_MODEL)
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        });
    let resumed = resume_builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn(TURN_AFTER_RESUME_PROMPT).await?;
    let resumed_request =
        wait_for_request_containing_text(&resumed_request, TURN_AFTER_RESUME_PROMPT).await?;
    assert_eq!(
        resumed_request
            .message_input_texts("user")
            .iter()
            .filter(|text| text.contains("<subagent_notification>"))
            .count(),
        1
    );

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_child_receives_forked_parent_context(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;

    let seed_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_0_FORK_PROMPT),
        sse(vec![
            ev_response_created("resp-seed-1"),
            ev_assistant_message("msg-seed-1", "seeded"),
            ev_completed("resp-seed-1"),
        ]),
    )
    .await;

    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "fork_context": true,
    }))?;
    let spawn_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config.model = Some(INHERITED_MODEL.to_string());
            config.model_reasoning_effort = Some(INHERITED_REASONING_EFFORT);
            config.agent_default_subagent_model = Some(REQUESTED_MODEL.to_string());
            config.agent_default_subagent_reasoning_effort = Some(REQUESTED_REASONING_EFFORT);
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn(TURN_0_FORK_PROMPT).await?;
    let _ = seed_turn.single_request();

    test.submit_turn(TURN_1_PROMPT).await?;
    let parent_body = spawn_turn.single_request().body_json();

    let child_request = wait_for_request_with_model(&child_request_log, REQUESTED_MODEL).await?;
    assert!(child_request.body_contains_text(TURN_0_FORK_PROMPT));
    let child_body = child_request.body_json();
    let original_parent_turn_id = parent_body["client_metadata"]["turn_id"]
        .as_str()
        .expect("legacy spawn parent turn id");
    assert_parent_turn(&parent_body, /*expected*/ None)?;
    assert_parent_turn(&child_body, Some(original_parent_turn_id))?;
    assert_eq!(
        (
            child_body["model"].clone(),
            child_body["reasoning"]["effort"].clone(),
        ),
        (
            json!(REQUESTED_MODEL),
            json!(REQUESTED_REASONING_EFFORT.to_string()),
        )
    );
    let child_thread_id = ThreadId::from_string(
        child_body["client_metadata"]["thread_id"]
            .as_str()
            .expect("legacy child thread id"),
    )?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    tokio::time::timeout(Duration::from_secs(2), async {
        while !matches!(child_thread.agent_status().await, AgentStatus::Completed(_)) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let args = serde_json::to_string(&json!({
        "target": child_thread_id.to_string(),
        "message": "legacy child follow-up",
    }))?;
    let parent = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "reuse the legacy child"),
        sse(vec![
            ev_response_created("resp-legacy-reuse"),
            ev_function_call_with_namespace(
                "legacy-reuse-call",
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &args,
            ),
            ev_completed("resp-legacy-reuse"),
        ]),
    )
    .await;
    let followup = mount_sse_sequence(
        &server,
        vec![
            sse(vec![ev_completed("resp-legacy-child-reuse")]),
            sse(vec![ev_completed("resp-legacy-reuse-complete")]),
        ],
    )
    .await;

    test.submit_turn("reuse the legacy child").await?;
    let followup_parent_body = parent.single_request().body_json();
    let reused_child_body = wait_for_request_with_model(&followup, REQUESTED_MODEL)
        .await?
        .body_json();
    let followup_parent_turn_id = followup_parent_body["client_metadata"]["turn_id"]
        .as_str()
        .expect("legacy follow-up parent turn id");
    assert_ne!(followup_parent_turn_id, original_parent_turn_id);
    let metadata = &reused_child_body["client_metadata"];
    assert_eq!(metadata["thread_id"], json!(child_thread_id));
    assert_parent_turn(&followup_parent_body, /*expected*/ None)?;
    assert_parent_turn(&reused_child_body, Some(followup_parent_turn_id))?;
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_final_wake_starts_an_idle_parent_turn(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "wake parent on final"),
        sse(vec![
            ev_response_created("resp-observation-spawn"),
            ev_function_call_with_namespace(
                "observation-spawn",
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-observation-spawn"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_PROMPT) && !body_contains(request, "observation-spawn")
        },
        sse_response(sse(vec![
            ev_response_created("resp-observation-child"),
            ev_assistant_message("msg-observation-child", "wake result"),
            ev_completed("resp-observation-child"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "observation-spawn"),
        sse(vec![
            ev_response_created("resp-observation-parent-done"),
            ev_assistant_message("msg-observation-parent-done", "parent idle"),
            ev_completed("resp-observation-parent-done"),
        ]),
    )
    .await;
    let wake_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "<subagent_notification>"),
        sse(vec![
            ev_response_created("resp-observation-wake"),
            ev_assistant_message("msg-observation-wake", "wake handled"),
            ev_completed("resp-observation-wake"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("wake parent on final").await?;

    let request =
        wait_for_request_containing_text(&wake_request, "<subagent_notification>").await?;
    assert!(request.body_contains_text("<subagent_notification>"));
    assert!(request.body_contains_text("wake result"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_final_wake_does_not_start_a_later_codex_exec_turn() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "one shot parent"),
        sse(vec![
            ev_response_created("resp-exec-boundary-spawn"),
            ev_function_call_with_namespace(
                "exec-boundary-spawn",
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-exec-boundary-spawn"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_PROMPT) && !body_contains(request, "exec-boundary-spawn")
        },
        sse_response(sse(vec![
            ev_response_created("resp-exec-boundary-child"),
            ev_assistant_message("msg-exec-boundary-child", "late exec result"),
            ev_completed("resp-exec-boundary-child"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "exec-boundary-spawn"),
        sse(vec![
            ev_response_created("resp-exec-boundary-parent-done"),
            ev_assistant_message("msg-exec-boundary-parent-done", "parent done"),
            ev_completed("resp-exec-boundary-parent-done"),
        ]),
    )
    .await;
    let unexpected_wake = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "<subagent_notification>"),
        sse(vec![
            ev_response_created("resp-unexpected-exec-boundary-wake"),
            ev_assistant_message("msg-unexpected-exec-boundary-wake", "unexpected"),
            ev_completed("resp-unexpected-exec-boundary-wake"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
    });
    let test = builder.build_with_auto_env(&server).await?;
    test.codex
        .set_app_server_client_info(
            Some("codex_exec".to_string()),
            /*app_server_client_version*/ None,
            /*mcp_elicitations_auto_deny*/ false,
        )
        .await
        .expect("set codex exec client identity");

    test.submit_turn("one shot parent").await?;

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let rollout = fs::read_to_string(
            test.codex
                .rollout_path()
                .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?,
        )?;
        if rollout.contains("<subagent_notification>") && rollout.contains("late exec result") {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for the late exec result to persist");
        }
        sleep(Duration::from_millis(10)).await;
    }
    assert!(unexpected_wake.requests().is_empty());
    assert_eq!(
        test.codex.agent_status().await,
        AgentStatus::Completed(Some("parent done".to_string()))
    );
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_commentary_wakes_once_without_delivering_final(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "w": "cx",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "observe first commentary"),
        sse(vec![
            ev_response_created("resp-commentary-spawn"),
            ev_function_call_with_namespace(
                "commentary-spawn",
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-commentary-spawn"),
        ]),
    )
    .await;
    let mut first_commentary =
        ev_assistant_message("msg-first-commentary", "useful acknowledgement");
    first_commentary["item"]["phase"] = json!("commentary");
    let mut later_commentary = ev_assistant_message("msg-later-commentary", "progress noise");
    later_commentary["item"]["phase"] = json!("commentary");
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_PROMPT) && !body_contains(request, "commentary-spawn")
        },
        sse_response(sse(vec![
            ev_response_created("resp-commentary-child"),
            first_commentary,
            later_commentary,
            ev_assistant_message("msg-commentary-final", "final result is not subscribed"),
            ev_completed("resp-commentary-child"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "commentary-spawn"),
        sse(vec![
            ev_response_created("resp-commentary-parent-done"),
            ev_assistant_message("msg-commentary-parent-done", "parent idle"),
            ev_completed("resp-commentary-parent-done"),
        ]),
    )
    .await;
    let commentary_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "<subagent_commentary>"),
        sse(vec![
            ev_response_created("resp-commentary-wake"),
            ev_assistant_message("msg-commentary-wake", "commentary handled"),
            ev_completed("resp-commentary-wake"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
        });
    let test = builder.build_with_auto_env(&server).await?;

    test.submit_turn("observe first commentary").await?;

    let request =
        wait_for_request_containing_text(&commentary_request, "<subagent_commentary>").await?;
    assert!(request.body_contains_text("useful acknowledgement"));
    assert!(!request.body_contains_text("progress noise"));
    assert!(!request.body_contains_text("final result is not subscribed"));
    let rollout_path = test
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    let rollout = tokio::fs::read_to_string(rollout_path).await?;
    assert!(!rollout.contains("<subagent_notification>"));
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_x_does_not_inject_the_child_final_response(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (test, spawned_id, _child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "x",
        }),
        ChildResponseTiming::Immediate,
        /*wait_for_parent_notification*/ false,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let child_thread = test
        .thread_manager
        .get_thread(ThreadId::from_string(&spawned_id)?)
        .await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(child_thread.agent_status().await, AgentStatus::Completed(_)) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let rollout_path = test
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    let rollout = tokio::fs::read_to_string(rollout_path).await?;
    assert!(rollout.contains("\"type\":\"agent_response_observation\""));
    assert!(rollout.contains(&spawned_id));
    let follow_up = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "inspect after fire and forget"),
        sse(vec![
            ev_response_created("resp-after-fire-and-forget"),
            ev_function_call_with_namespace(
                "wait-after-fire-and-forget",
                MULTI_AGENT_V1_NAMESPACE,
                "wait_agent",
                &serde_json::to_string(&json!({
                    "targets": [spawned_id.clone()],
                    "timeout_ms": 1_000,
                }))?,
            ),
            ev_completed("resp-after-fire-and-forget"),
        ]),
    )
    .await;
    let wait_result = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "wait-after-fire-and-forget")
                && body_contains(request, "child done")
        },
        sse(vec![
            ev_response_created("resp-wait-after-fire-and-forget"),
            ev_assistant_message("msg-wait-after-fire-and-forget", "done"),
            ev_completed("resp-wait-after-fire-and-forget"),
        ]),
    )
    .await;

    test.submit_turn("inspect after fire and forget").await?;

    let request =
        wait_for_request_containing_text(&follow_up, "inspect after fire and forget").await?;
    assert!(!request.body_contains_text("<subagent_notification>"));
    assert!(!request.body_contains_text("child done"));
    let wait_request =
        wait_for_request_containing_text(&wait_result, "wait-after-fire-and-forget").await?;
    assert!(wait_request.body_contains_text("child done"));
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_agent_revokes_v1_final_wake_before_shutdown(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "f",
        }),
        ChildResponseTiming::Delayed(Duration::from_millis(500)),
        /*wait_for_parent_notification*/ false,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let close_args = serde_json::to_string(&json!({
        "target": spawned_id,
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "close subscribed child"),
        sse(vec![
            ev_response_created("resp-close-subscribed-child"),
            ev_function_call_with_namespace(
                "close-subscribed-child",
                MULTI_AGENT_V1_NAMESPACE,
                "close_agent",
                &close_args,
            ),
            ev_completed("resp-close-subscribed-child"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "close-subscribed-child"),
        sse(vec![
            ev_response_created("resp-after-close-subscribed-child"),
            ev_assistant_message("msg-after-close-subscribed-child", "child closed"),
            ev_completed("resp-after-close-subscribed-child"),
        ]),
    )
    .await;

    test.submit_turn("close subscribed child").await?;
    sleep(Duration::from_secs(1)).await;

    let requests = server.received_requests().await.unwrap_or_default();
    assert!(!requests.iter().any(|request| {
        body_contains(request, "<subagent_notification>") && body_contains(request, "child done")
    }));
    let rollout_path = test
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;
    let rollout = tokio::fs::read_to_string(rollout_path).await?;
    assert!(!rollout.contains("<subagent_notification>"));
    assert!(!rollout.contains("child done"));
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_final_wake_survives_a_later_send_input_x(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "x",
        }),
        ChildResponseTiming::Immediate,
        /*wait_for_parent_notification*/ false,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let child_thread_id = ThreadId::from_string(&spawned_id)?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    assert!(matches!(
        wait_for_terminal_status(child_thread.as_ref()).await?,
        AgentStatus::Completed(_)
    ));

    let resume_args = serde_json::to_string(&json!({
        "id": spawned_id,
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "resume then redirect"),
        sse(vec![
            ev_response_created("resp-resume-observation"),
            ev_function_call_with_namespace(
                "resume-observation-call",
                MULTI_AGENT_V1_NAMESPACE,
                "resume_agent",
                &resume_args,
            ),
            ev_completed("resp-resume-observation"),
        ]),
    )
    .await;
    let send_args = serde_json::to_string(&json!({
        "target": child_thread_id,
        "message": "resumed child task",
        "w": "x",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "resume-observation-call"),
        sse(vec![
            ev_response_created("resp-send-after-resume"),
            ev_function_call_with_namespace(
                "send-after-resume-call",
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &send_args,
            ),
            ev_completed("resp-send-after-resume"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "resumed child task"),
        sse_response(sse(vec![
            ev_response_created("resp-resumed-child"),
            ev_assistant_message("msg-resumed-child", "resumed child result"),
            ev_completed("resp-resumed-child"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "send-after-resume-call"),
        sse(vec![
            ev_response_created("resp-parent-after-send"),
            ev_assistant_message("msg-parent-after-send", "parent idle"),
            ev_completed("resp-parent-after-send"),
        ]),
    )
    .await;
    let wake_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<subagent_notification>")
                && body_contains(request, "resumed child result")
        },
        sse(vec![
            ev_response_created("resp-resume-final-wake"),
            ev_assistant_message("msg-resume-final-wake", "wake handled"),
            ev_completed("resp-resume-final-wake"),
        ]),
    )
    .await;

    test.submit_turn("resume then redirect").await?;

    let request = wait_for_request_containing_text(&wake_request, "resumed child result").await?;
    assert!(request.body_contains_text("<subagent_notification>"));
    assert!(request.body_contains_text("resumed child result"));
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_input_commentary_binds_to_the_interrupt_replacement_turn(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (test, spawned_id, initial_child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "x",
        }),
        ChildResponseTiming::Delayed(Duration::from_secs(30)),
        /*wait_for_parent_notification*/ false,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let _ = wait_for_requests(&initial_child_request).await?;

    let send_call_id = "send-interrupt-observation";
    let send_args = serde_json::to_string(&json!({
        "target": spawned_id,
        "message": "replacement child task",
        "interrupt": true,
        "w": "cx",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "redirect the active child"),
        sse(vec![
            ev_response_created("resp-send-interrupt"),
            ev_function_call_with_namespace(
                send_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &send_args,
            ),
            ev_completed("resp-send-interrupt"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "replacement child task"),
        sse_response(sse(vec![
            ev_response_created("resp-replacement-child"),
            ev_commentary_message("msg-replacement-commentary", "replacement acknowledged"),
            ev_assistant_message("msg-replacement-final", "replacement complete"),
            ev_completed("resp-replacement-child"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| body_contains(request, send_call_id),
        sse(vec![
            ev_response_created("resp-parent-after-interrupt"),
            ev_assistant_message("msg-parent-after-interrupt", "parent idle"),
            ev_completed("resp-parent-after-interrupt"),
        ]),
    )
    .await;
    let commentary_wake = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<subagent_commentary>")
                && body_contains(request, "replacement acknowledged")
        },
        sse(vec![
            ev_response_created("resp-replacement-commentary-wake"),
            ev_assistant_message("msg-replacement-commentary-wake", "acknowledgement handled"),
            ev_completed("resp-replacement-commentary-wake"),
        ]),
    )
    .await;

    test.submit_turn("redirect the active child").await?;

    let request =
        wait_for_request_containing_text(&commentary_wake, "replacement acknowledged").await?;
    assert!(request.body_contains_text("msg-replacement-commentary"));
    assert!(!request.body_contains_text("child done"));
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_input_final_observation_wakes_an_idle_parent(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "x",
        }),
        ChildResponseTiming::Immediate,
        /*wait_for_parent_notification*/ false,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let child_thread = test
        .thread_manager
        .get_thread(ThreadId::from_string(&spawned_id)?)
        .await?;
    let _ = wait_for_terminal_status(child_thread.as_ref()).await?;

    let send_call_id = "send-final-observation";
    let send_args = serde_json::to_string(&json!({
        "target": spawned_id,
        "message": "final-observed child task",
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "observe the sent final"),
        sse(vec![
            ev_response_created("resp-send-final"),
            ev_function_call_with_namespace(
                send_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &send_args,
            ),
            ev_completed("resp-send-final"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "final-observed child task"),
        sse_response(sse(vec![
            ev_response_created("resp-final-observed-child"),
            ev_assistant_message("msg-final-observed-child", "sent final result"),
            ev_completed("resp-final-observed-child"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| body_contains(request, send_call_id),
        sse(vec![
            ev_response_created("resp-parent-after-send-final"),
            ev_assistant_message("msg-parent-after-send-final", "parent idle"),
            ev_completed("resp-parent-after-send-final"),
        ]),
    )
    .await;
    let final_wake = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<subagent_notification>")
                && body_contains(request, "sent final result")
        },
        sse(vec![
            ev_response_created("resp-send-final-wake"),
            ev_assistant_message("msg-send-final-wake", "final handled"),
            ev_completed("resp-send-final-wake"),
        ]),
    )
    .await;

    test.submit_turn("observe the sent final").await?;

    let request = wait_for_request_containing_text(&final_wake, "sent final result").await?;
    assert!(request.body_contains_text("<subagent_notification>"));
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn final_delivery_persistence_failure_restarts_the_v1_response_observer() -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let configured_store_id = store_id.clone();
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
        }),
        ChildResponseTiming::Immediate,
        /*wait_for_parent_notification*/ true,
        move |builder| {
            builder.with_config(move |config| {
                config.experimental_thread_store = ThreadStoreConfig::InMemory {
                    id: configured_store_id,
                };
            })
        },
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let child_thread_id = ThreadId::from_string(&spawned_id)?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let _ = wait_for_terminal_status(child_thread.as_ref()).await?;
    wait_for_no_pending_response_observation(
        &store,
        test.session_configured.thread_id,
        child_thread_id,
    )
    .await?;

    let send_call_id = "send-final-before-observation-flush-failure";
    let send_args = serde_json::to_string(&json!({
        "target": spawned_id,
        "message": "final result after transient observation failure",
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "observe final across persistence failure")
        },
        sse(vec![
            ev_response_created("resp-send-before-observation-flush-failure"),
            ev_function_call_with_namespace(
                send_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &send_args,
            ),
            ev_completed("resp-send-before-observation-flush-failure"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "final result after transient observation failure")
        },
        sse_response(sse(vec![
            ev_response_created("resp-child-after-observation-flush-failure"),
            ev_assistant_message(
                "msg-child-after-observation-flush-failure",
                "result survives observation failure",
            ),
            ev_completed("resp-child-after-observation-flush-failure"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| body_contains(request, send_call_id),
        sse(vec![
            ev_response_created("resp-parent-before-observation-flush-failure"),
            ev_assistant_message("msg-parent-before-observation-flush-failure", "parent idle"),
            ev_completed("resp-parent-before-observation-flush-failure"),
        ]),
    )
    .await;
    let final_wake = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<subagent_notification>")
                && body_contains(request, "result survives observation failure")
        },
        sse(vec![
            ev_response_created("resp-final-after-observation-flush-failure"),
            ev_assistant_message(
                "msg-final-after-observation-flush-failure",
                "recovered final handled",
            ),
            ev_completed("resp-final-after-observation-flush-failure"),
        ]),
    )
    .await;
    store
        .fail_agent_response_observation_flushes_after(
            /*successful_flushes*/ 2, /*failed_flushes*/ 1,
        )
        .await;

    test.submit_turn("observe final across persistence failure")
        .await?;

    let request =
        wait_for_request_containing_text(&final_wake, "result survives observation failure")
            .await?;
    assert!(request.body_contains_text("<subagent_notification>"));
    test.codex.flush_rollout().await?;
    assert_eq!(
        read_test_rollout_items(&test)?
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::ResponseItem(ResponseItem::AgentMessage { content, .. })
                        if content.iter().any(|content| {
                            matches!(
                                content,
                                AgentMessageContent::Text { text }
                                    if text.contains("result survives observation failure")
                            )
                        })
                )
            })
            .count(),
        1
    );
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "baseline response after recovery"),
        sse(vec![
            ev_response_created("resp-child-baseline-after-observation-recovery"),
            ev_assistant_message(
                "msg-child-baseline-after-observation-recovery",
                "baseline survived recovery",
            ),
            ev_completed("resp-child-baseline-after-observation-recovery"),
        ]),
    )
    .await;
    submit_turn_on_thread(child_thread.as_ref(), "baseline response after recovery").await?;
    wait_for_no_pending_response_observation(
        &store,
        test.session_configured.thread_id,
        child_thread_id,
    )
    .await?;
    let parent_follow_up = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "inspect recovered baseline"),
        sse(vec![
            ev_response_created("resp-inspect-recovered-baseline"),
            ev_assistant_message("msg-inspect-recovered-baseline", "inspection complete"),
            ev_completed("resp-inspect-recovered-baseline"),
        ]),
    )
    .await;
    test.submit_turn("inspect recovered baseline").await?;
    let request =
        wait_for_request_containing_text(&parent_follow_up, "inspect recovered baseline").await?;
    assert!(request.body_contains_text("baseline survived recovery"));

    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_wait_suppresses_passive_v1_completion_context(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
        }),
        ChildResponseTiming::Delayed(Duration::from_secs(5)),
        /*wait_for_parent_notification*/ false,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;

    let wait_call_id = "wait-owned-passive";
    let wait_args = serde_json::to_string(&json!({
        "targets": [spawned_id],
        "timeout_ms": 10_000,
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "wait before child completion"),
        sse(vec![
            ev_response_created("resp-wait-owned-passive"),
            ev_function_call_with_namespace(
                wait_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "wait_agent",
                &wait_args,
            ),
            ev_completed("resp-wait-owned-passive"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| body_contains(request, wait_call_id),
        sse(vec![
            ev_response_created("resp-after-wait-owned-passive"),
            ev_assistant_message("msg-after-wait-owned-passive", "wait handled"),
            ev_completed("resp-after-wait-owned-passive"),
        ]),
    )
    .await;

    test.submit_turn("wait before child completion").await?;

    let follow_up = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "inspect after owned wait"),
        sse(vec![
            ev_response_created("resp-inspect-after-owned-wait"),
            ev_assistant_message("msg-inspect-after-owned-wait", "inspection complete"),
            ev_completed("resp-inspect-after-owned-wait"),
        ]),
    )
    .await;
    test.submit_turn("inspect after owned wait").await?;
    let request = wait_for_request_containing_text(&follow_up, "inspect after owned wait").await?;
    assert_eq!(
        request
            .message_input_texts("user")
            .iter()
            .filter(|text| text.contains("<subagent_notification>"))
            .count(),
        0
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timed_out_wait_leaves_v1_final_wake_active() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "f",
        }),
        ChildResponseTiming::Delayed(Duration::from_secs(20)),
        /*wait_for_parent_notification*/ false,
        |builder| builder.with_history_mode(ThreadHistoryMode::Legacy),
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;

    let wait_call_id = "timed-out-final-wait";
    let wait_args = serde_json::to_string(&json!({
        "targets": [spawned_id],
        "timeout_ms": 10_000,
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "time out before final"),
        sse(vec![
            ev_response_created("resp-timed-out-final-wait"),
            ev_function_call_with_namespace(
                wait_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "wait_agent",
                &wait_args,
            ),
            ev_completed("resp-timed-out-final-wait"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| body_contains(request, wait_call_id),
        sse(vec![
            ev_response_created("resp-after-timed-out-final-wait"),
            ev_assistant_message("msg-after-timed-out-final-wait", "parent idle"),
            ev_completed("resp-after-timed-out-final-wait"),
        ]),
    )
    .await;
    let wake = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<subagent_notification>")
                && body_contains(request, "child done")
        },
        sse(vec![
            ev_response_created("resp-after-timed-out-wake"),
            ev_assistant_message("msg-after-timed-out-wake", "wake retained"),
            ev_completed("resp-after-timed-out-wake"),
        ]),
    )
    .await;

    test.submit_turn("time out before final").await?;

    let request = wait_for_request_containing_text(&wake, "child done").await?;
    assert!(request.body_contains_text("<subagent_notification>"));
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_v1_final_wake_survives_parent_compaction(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let compact_prompt = "compact while the child wake remains pending";
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "f",
        }),
        ChildResponseTiming::Delayed(Duration::from_secs(5)),
        /*wait_for_parent_notification*/ false,
        |builder| {
            builder
                .with_history_mode(history_mode)
                .with_config(move |config| {
                    config.compact_prompt = Some(compact_prompt.to_string());
                })
        },
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let child_thread = test
        .thread_manager
        .get_thread(ThreadId::from_string(&spawned_id)?)
        .await?;

    let compact = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| body_contains(request, compact_prompt),
        sse(vec![
            ev_response_created("resp-pending-wake-compact"),
            ev_assistant_message("msg-pending-wake-compact", "compacted parent context"),
            ev_completed("resp-pending-wake-compact"),
        ]),
    )
    .await;
    test.codex.submit(Op::Compact).await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_)).then_some(())
    })
    .await;
    let _ = wait_for_request_containing_text(&compact, compact_prompt).await?;

    let wake = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<subagent_notification>")
                && body_contains(request, "child done")
        },
        sse(vec![
            ev_response_created("resp-post-compact-child-wake"),
            ev_assistant_message("msg-post-compact-child-wake", "wake handled"),
            ev_completed("resp-post-compact-child-wake"),
        ]),
    )
    .await;
    assert_eq!(
        wait_for_terminal_status(child_thread.as_ref()).await?,
        AgentStatus::Completed(Some("child done".to_string()))
    );
    let wake_request = wait_for_request_containing_text(&wake, "child done").await?;
    assert!(wake_request.body_contains_text("<subagent_notification>"));

    let inspect = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "inspect compacted wake"),
        sse(vec![
            ev_response_created("resp-inspect-compacted-wake"),
            ev_assistant_message("msg-inspect-compacted-wake", "inspection complete"),
            ev_completed("resp-inspect-compacted-wake"),
        ]),
    )
    .await;
    test.submit_turn("inspect compacted wake").await?;
    let inspect_request =
        wait_for_request_containing_text(&inspect, "inspect compacted wake").await?;
    assert_eq!(
        inspect_request
            .message_input_texts("user")
            .iter()
            .filter(|text| {
                text.contains("<subagent_notification>") && text.contains("child done")
            })
            .count(),
        1
    );
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_resume_requires_explicit_agent_reconfiguration(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "w": "cf",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "prepare cold resume boundary"),
        sse(vec![
            ev_response_created("resp-cold-boundary-spawn"),
            ev_function_call_with_namespace(
                "cold-boundary-spawn",
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-cold-boundary-spawn"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_PROMPT) && !body_contains(request, "cold-boundary-spawn")
        },
        sse_response(sse(vec![
            ev_response_created("resp-cold-boundary-child"),
            ev_commentary_message("msg-cold-boundary-commentary", "durable acknowledgement"),
            ev_assistant_message("msg-cold-boundary-final", "durable child final"),
            ev_completed("resp-cold-boundary-child"),
        ]))
        .set_delay(Duration::from_secs(5)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "cold-boundary-spawn"),
        sse(vec![
            ev_response_created("resp-cold-boundary-parent-idle"),
            ev_assistant_message("msg-cold-boundary-parent-idle", "parent idle"),
            ev_completed("resp-cold-boundary-parent-idle"),
        ]),
    )
    .await;

    let configured_store_id = store_id.clone();
    let mut builder = test_codex()
        .with_history_mode(history_mode)
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config.experimental_thread_store = ThreadStoreConfig::InMemory {
                id: configured_store_id,
            };
        });
    let initial = builder.build_with_auto_env(&server).await?;
    initial.submit_turn("prepare cold resume boundary").await?;
    let spawned_id = ThreadId::from_string(&wait_for_spawned_thread_id(&initial).await?)?;
    let child_thread = initial.thread_manager.get_thread(spawned_id).await?;
    let durable_context_permit = initial.codex.acquire_durable_context_permit().await?;
    assert_eq!(
        wait_for_terminal_status(child_thread.as_ref()).await?,
        AgentStatus::Completed(Some("durable child final".to_string()))
    );

    let parent_thread_id = initial.session_configured.thread_id;
    let parent_history = store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: parent_thread_id,
            include_archived: false,
        })
        .await?;
    initial.thread_manager.remove_thread(&spawned_id).await;
    initial
        .thread_manager
        .remove_thread(&parent_thread_id)
        .await;

    let automatic_delivery = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "<subagent_commentary>")
                || body_contains(request, "<subagent_notification>")
        },
        sse(vec![
            ev_response_created("resp-unexpected-cold-delivery"),
            ev_assistant_message("msg-unexpected-cold-delivery", "unexpected"),
            ev_completed("resp-unexpected-cold-delivery"),
        ]),
    )
    .await;
    let resumed = initial
        .thread_manager
        .resume_thread_with_history(
            initial.config.clone(),
            InitialHistory::Resumed(ResumedHistory {
                conversation_id: parent_thread_id,
                history: Arc::new(parent_history.items),
                rollout_path: None,
            }),
            codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key("test")),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?;
    sleep(Duration::from_millis(100)).await;
    assert!(automatic_delivery.requests().is_empty());

    let resume_call_id = "explicit-cold-boundary-resume";
    let resume_args = serde_json::to_string(&json!({
        "id": spawned_id,
        "w": "f",
    }))?;
    let explicit_resume = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "reconfigure agents after cold resume")
        },
        sse(vec![
            ev_response_created("resp-explicit-cold-boundary-resume"),
            ev_function_call_with_namespace(
                resume_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "resume_agent",
                &resume_args,
            ),
            ev_completed("resp-explicit-cold-boundary-resume"),
        ]),
    )
    .await;
    let explicit_result = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, resume_call_id) && body_contains(request, "durable child final")
        },
        sse(vec![
            ev_response_created("resp-after-explicit-cold-boundary-resume"),
            ev_assistant_message("msg-after-explicit-cold-boundary-resume", "reconfigured"),
            ev_completed("resp-after-explicit-cold-boundary-resume"),
        ]),
    )
    .await;
    submit_turn_on_thread(
        resumed.thread.as_ref(),
        "reconfigure agents after cold resume",
    )
    .await?;
    let request =
        wait_for_request_containing_text(&explicit_resume, "reconfigure agents after cold resume")
            .await?;
    assert!(!request.body_contains_text("<subagent_commentary>"));
    assert!(!request.body_contains_text("<subagent_notification>"));
    assert!(!request.body_contains_text("durable acknowledgement"));
    assert!(!request.body_contains_text("durable child final"));
    let _ = wait_for_request_containing_text(&explicit_result, "durable child final").await?;
    assert!(automatic_delivery.requests().is_empty());

    InMemoryThreadStore::remove_id(&store_id);
    drop(durable_context_permit);
    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_requires_explicit_agent_reconfiguration(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let configured_store_id = store_id.clone();
    let (initial, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "cf",
        }),
        ChildResponseTiming::Delayed(Duration::from_secs(5)),
        /*wait_for_parent_notification*/ false,
        move |builder| {
            builder
                .with_history_mode(history_mode)
                .with_config(move |config| {
                    config.experimental_thread_store = ThreadStoreConfig::InMemory {
                        id: configured_store_id,
                    };
                })
        },
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let spawned_id = ThreadId::from_string(&spawned_id)?;
    let child_thread = initial.thread_manager.get_thread(spawned_id).await?;
    let source_parent_id = initial.session_configured.thread_id;
    let parent_history = store
        .load_rollback_history(LoadThreadHistoryParams {
            thread_id: source_parent_id,
            include_archived: false,
        })
        .await?;
    assert!(parent_history.items.iter().any(|item| {
        matches!(
            item,
            RolloutItem::AgentResponseObservation(observation)
                if observation.target_thread_id == spawned_id
                    && observation.pending_commentary
                    && observation.final_delivery == AgentResponseFinalDelivery::Wake
        )
    }));
    let forked = initial
        .thread_manager
        .resume_thread_with_history(
            initial.config.clone(),
            InitialHistory::Forked(parent_history.items),
            codex_core::test_support::auth_manager_from_auth(CodexAuth::from_api_key("test")),
            /*parent_trace*/ None,
            /*supports_openai_form_elicitation*/ false,
        )
        .await?;
    assert_ne!(forked.thread_id, source_parent_id);
    initial
        .thread_manager
        .remove_thread(&source_parent_id)
        .await;

    assert_eq!(
        wait_for_terminal_status(child_thread.as_ref()).await?,
        AgentStatus::Completed(Some("child done".to_string()))
    );
    sleep(Duration::from_millis(100)).await;
    let requests_before_explicit_resume = server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| {
            body_contains(request, "<subagent_commentary>")
                || body_contains(request, "<subagent_notification>")
        })
        .count();
    assert_eq!(requests_before_explicit_resume, 0);

    let resume_call_id = "explicit-fork-boundary-resume";
    let resume_args = serde_json::to_string(&json!({
        "id": spawned_id,
        "w": "f",
    }))?;
    let explicit_resume = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "reconfigure agents in fork"),
        sse(vec![
            ev_response_created("resp-explicit-fork-boundary-resume"),
            ev_function_call_with_namespace(
                resume_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "resume_agent",
                &resume_args,
            ),
            ev_completed("resp-explicit-fork-boundary-resume"),
        ]),
    )
    .await;
    let explicit_result = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, resume_call_id) && body_contains(request, "child done")
        },
        sse(vec![
            ev_response_created("resp-after-explicit-fork-boundary-resume"),
            ev_assistant_message("msg-after-explicit-fork-boundary-resume", "reconfigured"),
            ev_completed("resp-after-explicit-fork-boundary-resume"),
        ]),
    )
    .await;
    submit_turn_on_thread(forked.thread.as_ref(), "reconfigure agents in fork").await?;
    let request =
        wait_for_request_containing_text(&explicit_resume, "reconfigure agents in fork").await?;
    assert!(!request.body_contains_text("<subagent_commentary>"));
    assert!(!request.body_contains_text("<subagent_notification>"));
    assert!(!request.body_contains_text("child done"));
    let _ = wait_for_request_containing_text(&explicit_result, "child done").await?;

    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn send_input_persistence_failure_rolls_back_response_observation() -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let configured_store_id = store_id.clone();
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "x",
        }),
        ChildResponseTiming::Immediate,
        /*wait_for_parent_notification*/ false,
        move |builder| {
            builder.with_config(move |config| {
                config.experimental_thread_store = ThreadStoreConfig::InMemory {
                    id: configured_store_id,
                };
            })
        },
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let child_thread_id = ThreadId::from_string(&spawned_id)?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let _ = wait_for_terminal_status(child_thread.as_ref()).await?;

    let send_call_id = "send-observation-persistence-failure";
    let send_args = serde_json::to_string(&json!({
        "target": spawned_id,
        "message": "task whose observation persistence fails",
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "fail send observation persistence"),
        sse(vec![
            ev_response_created("resp-fail-send-observation"),
            ev_function_call_with_namespace(
                send_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &send_args,
            ),
            ev_completed("resp-fail-send-observation"),
        ]),
    )
    .await;
    let child_request = mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "task whose observation persistence fails")
        },
        sse_response(sse(vec![
            ev_response_created("resp-child-after-send-observation-failure"),
            ev_assistant_message(
                "msg-child-after-send-observation-failure",
                "unsubscribed send result",
            ),
            ev_completed("resp-child-after-send-observation-failure"),
        ]))
        .set_delay(Duration::from_secs(1)),
    )
    .await;
    let failure_response = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, send_call_id)
                && body_contains(request, "failed to persist response observation state")
        },
        sse(vec![
            ev_response_created("resp-send-observation-failure-handled"),
            ev_assistant_message("msg-send-observation-failure-handled", "failure handled"),
            ev_completed("resp-send-observation-failure-handled"),
        ]),
    )
    .await;
    store
        .fail_next_operation(InMemoryThreadStoreFailure::AgentResponseObservationFlush)
        .await;

    test.submit_turn("fail send observation persistence")
        .await?;

    let _ = wait_for_request_containing_text(
        &failure_response,
        "failed to persist response observation state",
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if child_thread.agent_status().await
                == AgentStatus::Completed(Some("unsubscribed send result".to_string()))
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let follow_up = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "after failed send persistence"),
        sse(vec![
            ev_response_created("resp-after-failed-send-persistence"),
            ev_assistant_message("msg-after-failed-send-persistence", "done"),
            ev_completed("resp-after-failed-send-persistence"),
        ]),
    )
    .await;
    test.submit_turn("after failed send persistence").await?;
    let request =
        wait_for_request_containing_text(&follow_up, "after failed send persistence").await?;
    assert!(!request.body_contains_text("unsubscribed send result"));
    test.codex.flush_rollout().await?;
    assert_no_pending_response_observation(
        &store,
        test.session_configured.thread_id,
        child_thread_id,
    )
    .await?;

    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn failed_response_observation_compensation_quarantines_the_parent() -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let configured_store_id = store_id.clone();
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "x",
        }),
        ChildResponseTiming::Immediate,
        /*wait_for_parent_notification*/ false,
        move |builder| {
            builder.with_config(move |config| {
                config.experimental_thread_store = ThreadStoreConfig::InMemory {
                    id: configured_store_id,
                };
            })
        },
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let child_thread = test
        .thread_manager
        .get_thread(ThreadId::from_string(&spawned_id)?)
        .await?;
    let _ = wait_for_terminal_status(child_thread.as_ref()).await?;

    let send_call_id = "send-with-unknown-observation-commit";
    let send_args = serde_json::to_string(&json!({
        "target": spawned_id,
        "message": "task with unknown observation commit",
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "fail observation compensation"),
        sse(vec![
            ev_response_created("resp-send-with-unknown-observation-commit"),
            ev_function_call_with_namespace(
                send_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &send_args,
            ),
            ev_completed("resp-send-with-unknown-observation-commit"),
        ]),
    )
    .await;
    mount_response_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "task with unknown observation commit")
        },
        sse_response(sse(vec![
            ev_response_created("resp-child-with-unknown-observation-commit"),
            ev_assistant_message(
                "msg-child-with-unknown-observation-commit",
                "child result after unknown commit",
            ),
            ev_completed("resp-child-with-unknown-observation-commit"),
        ]))
        .set_delay(Duration::from_secs(1)),
    )
    .await;
    let failure_response = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, send_call_id)
                && body_contains(request, "durable subscription outcome is unknown")
        },
        sse(vec![
            ev_response_created("resp-unknown-observation-commit-handled"),
            ev_assistant_message("msg-unknown-observation-commit-handled", "failure handled"),
            ev_completed("resp-unknown-observation-commit-handled"),
        ]),
    )
    .await;
    store
        .fail_agent_response_observation_flushes_after(
            /*successful_flushes*/ 0, /*failed_flushes*/ 2,
        )
        .await;

    test.submit_turn("fail observation compensation").await?;

    let _ = wait_for_request_containing_text(
        &failure_response,
        "durable subscription outcome is unknown",
    )
    .await?;
    let error = test
        .codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: "work must remain quarantined".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await
        .expect_err("parent should reject work after an unknown observation commit");
    assert_eq!(
        error.to_string(),
        "invalid request: thread history must be reloaded before accepting more work"
    );

    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resume_persistence_failure_does_not_subscribe_the_next_target_turn() -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let configured_store_id = store_id.clone();
    let (test, spawned_id, child_request) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "w": "x",
        }),
        ChildResponseTiming::Immediate,
        /*wait_for_parent_notification*/ false,
        move |builder| {
            builder.with_config(move |config| {
                config.experimental_thread_store = ThreadStoreConfig::InMemory {
                    id: configured_store_id,
                };
            })
        },
    )
    .await?;
    let _ = wait_for_requests(&child_request).await?;
    let child_thread_id = ThreadId::from_string(&spawned_id)?;
    let child_thread = test.thread_manager.get_thread(child_thread_id).await?;
    let _ = wait_for_terminal_status(child_thread.as_ref()).await?;

    let resume_call_id = "resume-observation-persistence-failure";
    let resume_args = serde_json::to_string(&json!({
        "id": spawned_id,
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "fail resume observation persistence"),
        sse(vec![
            ev_response_created("resp-fail-resume-observation"),
            ev_function_call_with_namespace(
                resume_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "resume_agent",
                &resume_args,
            ),
            ev_completed("resp-fail-resume-observation"),
        ]),
    )
    .await;
    let failure_response = mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| {
            body_contains(request, resume_call_id)
                && body_contains(
                    request,
                    "failed to persist initial response observation state",
                )
        },
        sse(vec![
            ev_response_created("resp-resume-observation-failure-handled"),
            ev_assistant_message("msg-resume-observation-failure-handled", "failure handled"),
            ev_completed("resp-resume-observation-failure-handled"),
        ]),
    )
    .await;
    store
        .fail_next_operation(InMemoryThreadStoreFailure::AgentResponseObservationFlush)
        .await;
    test.submit_turn("fail resume observation persistence")
        .await?;
    let _ = wait_for_request_containing_text(
        &failure_response,
        "failed to persist initial response observation state",
    )
    .await?;

    let send_call_id = "send-after-failed-resume-observation";
    let send_args = serde_json::to_string(&json!({
        "target": child_thread_id,
        "message": "fire and forget after failed resume",
        "w": "x",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "send after failed resume"),
        sse(vec![
            ev_response_created("resp-send-after-failed-resume"),
            ev_function_call_with_namespace(
                send_call_id,
                MULTI_AGENT_V1_NAMESPACE,
                "send_input",
                &send_args,
            ),
            ev_completed("resp-send-after-failed-resume"),
        ]),
    )
    .await;
    let child_request = mount_response_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "fire and forget after failed resume"),
        sse_response(sse(vec![
            ev_response_created("resp-child-after-failed-resume"),
            ev_assistant_message(
                "msg-child-after-failed-resume",
                "unsubscribed resume result",
            ),
            ev_completed("resp-child-after-failed-resume"),
        ]))
        .set_delay(Duration::from_secs(1)),
    )
    .await;
    mount_sse_once_match(
        &server,
        move |request: &wiremock::Request| body_contains(request, send_call_id),
        sse(vec![
            ev_response_created("resp-parent-after-failed-resume-send"),
            ev_assistant_message("msg-parent-after-failed-resume-send", "parent idle"),
            ev_completed("resp-parent-after-failed-resume-send"),
        ]),
    )
    .await;

    test.submit_turn("send after failed resume").await?;

    let _ = wait_for_requests(&child_request).await?;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if child_thread.agent_status().await
                == AgentStatus::Completed(Some("unsubscribed resume result".to_string()))
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await?;
    let follow_up = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "after failed resume persistence"),
        sse(vec![
            ev_response_created("resp-after-failed-resume-persistence"),
            ev_assistant_message("msg-after-failed-resume-persistence", "done"),
            ev_completed("resp-after-failed-resume-persistence"),
        ]),
    )
    .await;
    test.submit_turn("after failed resume persistence").await?;
    let request =
        wait_for_request_containing_text(&follow_up, "after failed resume persistence").await?;
    assert!(!request.body_contains_text("unsubscribed resume result"));
    test.codex.flush_rollout().await?;
    assert_no_pending_response_observation(
        &store,
        test.session_configured.thread_id,
        child_thread_id,
    )
    .await?;

    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_reports_initial_response_observation_persistence_failure() -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "w": "f",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "fail observation persistence"),
        sse(vec![
            ev_response_created("resp-failing-observation-spawn"),
            ev_function_call_with_namespace(
                "failing-observation-spawn",
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-failing-observation-spawn"),
        ]),
    )
    .await;
    let failure_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "failing-observation-spawn")
                && body_contains(
                    request,
                    "failed to persist initial response observation state",
                )
        },
        sse(vec![
            ev_response_created("resp-observation-persistence-failed"),
            ev_assistant_message("msg-observation-persistence-failed", "failure handled"),
            ev_completed("resp-observation-persistence-failed"),
        ]),
    )
    .await;

    let configured_store_id = store_id.clone();
    let test = test_codex()
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config.experimental_thread_store = ThreadStoreConfig::InMemory {
                id: configured_store_id,
            };
        })
        .build_with_auto_env(&server)
        .await?;
    store
        .fail_next_operation(InMemoryThreadStoreFailure::AgentResponseObservationFlush)
        .await;

    test.submit_turn("fail observation persistence").await?;

    let request = wait_for_request_containing_text(
        &failure_response,
        "failed to persist initial response observation state",
    )
    .await?;
    assert!(request.body_contains_text("failed to persist initial response observation state"));
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[test_case("f", 1; "final wake")]
#[test_case("x", 0; "fire and forget")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_discards_child_after_post_admission_observation_persistence_failure(
    w: &str,
    successful_flushes: usize,
) -> Result<()> {
    let server = start_mock_server().await;
    let store_id = uuid::Uuid::now_v7().to_string();
    let store = InMemoryThreadStore::for_id(store_id.clone());
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "w": w,
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, "fail post admission persistence"),
        sse(vec![
            ev_response_created("resp-post-admission-observation-spawn"),
            ev_function_call_with_namespace(
                "post-admission-observation-spawn",
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-post-admission-observation-spawn"),
        ]),
    )
    .await;
    let child_request = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, CHILD_PROMPT)
                && !body_contains(request, "post-admission-observation-spawn")
        },
        sse(vec![
            ev_response_created("resp-post-admission-observation-child"),
            ev_assistant_message("msg-post-admission-observation-child", "child ran"),
            ev_completed("resp-post-admission-observation-child"),
        ]),
    )
    .await;
    let failure_response = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            body_contains(request, "post-admission-observation-spawn")
                && body_contains(
                    request,
                    "failed to persist spawned response observation state",
                )
        },
        sse(vec![
            ev_response_created("resp-post-admission-observation-failed"),
            ev_assistant_message("msg-post-admission-observation-failed", "failure handled"),
            ev_completed("resp-post-admission-observation-failed"),
        ]),
    )
    .await;

    let configured_store_id = store_id.clone();
    let test = test_codex()
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config.experimental_thread_store = ThreadStoreConfig::InMemory {
                id: configured_store_id,
            };
        })
        .build_with_auto_env(&server)
        .await?;
    let mut thread_created = test.thread_manager.subscribe_thread_created();
    store
        .fail_agent_response_observation_flushes_after(
            successful_flushes,
            /*failed_flushes*/ 1,
        )
        .await;

    test.submit_turn("fail post admission persistence").await?;

    let _ = wait_for_requests(&child_request).await?;
    let request = wait_for_request_containing_text(
        &failure_response,
        "failed to persist spawned response observation state",
    )
    .await?;
    assert!(request.body_contains_text("failed to persist spawned response observation state"));
    assert_eq!(
        test.thread_manager.list_thread_ids().await,
        vec![test.session_configured.thread_id]
    );
    assert!(matches!(
        thread_created.try_recv(),
        Err(tokio::sync::broadcast::error::TryRecvError::Empty)
    ));
    InMemoryThreadStore::remove_id(&store_id);
    Ok(())
}

#[derive(Clone, Copy)]
enum FullHistoryV2ModelSelection {
    ConfiguredDefault,
    ExplicitOverride,
}

#[test_case(FullHistoryV2ModelSelection::ConfiguredDefault; "configured default with omitted fork_turns")]
#[test_case(FullHistoryV2ModelSelection::ExplicitOverride; "explicit override with fork_turns all")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_full_history_v2_child_uses_model_precedence_without_dropping_context(
    selection: FullHistoryV2ModelSelection,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let seed_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_0_FORK_PROMPT),
        sse(vec![
            ev_response_created("resp-seed-1"),
            ev_assistant_message("msg-seed-1", "seeded"),
            ev_completed("resp-seed-1"),
        ]),
    )
    .await;
    let (spawn_args, expected_model, expected_reasoning_effort) = match selection {
        FullHistoryV2ModelSelection::ConfiguredDefault => (
            json!({
                "message": CHILD_PROMPT,
                "task_name": "worker",
            }),
            V2_DEFAULT_MODEL,
            V2_DEFAULT_REASONING_EFFORT,
        ),
        FullHistoryV2ModelSelection::ExplicitOverride => (
            json!({
                "message": CHILD_PROMPT,
                "task_name": "worker",
                "fork_turns": "all",
                "model": V2_REQUESTED_MODEL,
                "reasoning_effort": V2_REQUESTED_REASONING_EFFORT,
            }),
            V2_REQUESTED_MODEL,
            V2_REQUESTED_REASONING_EFFORT,
        ),
    };
    let spawn_args = serde_json::to_string(&spawn_args)?;
    let spawn_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;
    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        config.multi_agent_v2.message_delivery = MultiAgentMessageDelivery::Plaintext;
        config.multi_agent_v2.default_fork_turns = "all".to_string();
        config.model = Some(INHERITED_MODEL.to_string());
        config.model_reasoning_effort = Some(INHERITED_REASONING_EFFORT);
        config.agent_default_subagent_model = Some(V2_DEFAULT_MODEL.to_string());
        config.agent_default_subagent_reasoning_effort = Some(V2_DEFAULT_REASONING_EFFORT);
    });
    let test = builder.build(&server).await?;

    test.submit_turn(TURN_0_FORK_PROMPT).await?;
    let _ = seed_turn.single_request();
    test.submit_turn(TURN_1_PROMPT).await?;
    let _ = spawn_turn.single_request();

    let child_request = wait_for_request_with_model(&child_request_log, expected_model).await?;
    assert!(child_request.body_contains_text(TURN_0_FORK_PROMPT));
    let child_body = child_request.body_json();
    assert_eq!(
        (
            child_body["model"].clone(),
            child_body["reasoning"]["effort"].clone(),
        ),
        (
            json!(expected_model),
            json!(expected_reasoning_effort.to_string()),
        )
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_requested_model_and_reasoning_override_inherited_settings_without_role()
-> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot = spawn_child_and_capture_snapshot(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "model": REQUESTED_MODEL,
            "reasoning_effort": REQUESTED_REASONING_EFFORT,
        }),
        |builder| {
            builder.with_config(|config| {
                config.agent_default_subagent_model = Some(INHERITED_MODEL.to_string());
                config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::High);
            })
        },
    )
    .await?;

    assert_eq!(child_snapshot.model, REQUESTED_MODEL);
    assert_eq!(
        child_snapshot.reasoning_effort,
        Some(REQUESTED_REASONING_EFFORT)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_uses_configured_subagent_defaults() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot =
        spawn_child_and_capture_snapshot(&server, json!({ "message": CHILD_PROMPT }), |builder| {
            builder.with_config(|config| {
                config.agent_default_subagent_model = Some(REQUESTED_MODEL.to_string());
                config.agent_default_subagent_reasoning_effort = Some(REQUESTED_REASONING_EFFORT);
            })
        })
        .await?;

    assert_eq!(
        (child_snapshot.model, child_snapshot.reasoning_effort),
        (
            REQUESTED_MODEL.to_string(),
            Some(REQUESTED_REASONING_EFFORT)
        )
    );
    Ok(())
}

#[test_case(
    Some(REQUESTED_MODEL),
    None,
    REQUESTED_MODEL,
    Some(ReasoningEffort::Medium);
    "model only"
)]
#[test_case(
    None,
    Some(REQUESTED_REASONING_EFFORT),
    INHERITED_MODEL,
    Some(REQUESTED_REASONING_EFFORT);
    "reasoning effort only"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_uses_independent_configured_subagent_defaults(
    default_model: Option<&str>,
    default_reasoning_effort: Option<ReasoningEffort>,
    expected_model: &str,
    expected_reasoning_effort: Option<ReasoningEffort>,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let default_model = default_model.map(str::to_string);
    let child_snapshot =
        spawn_child_and_capture_snapshot(&server, json!({ "message": CHILD_PROMPT }), |builder| {
            builder.with_config(move |config| {
                config.agent_default_subagent_model = default_model;
                config.agent_default_subagent_reasoning_effort = default_reasoning_effort;
            })
        })
        .await?;

    assert_eq!(
        (child_snapshot.model, child_snapshot.reasoning_effort),
        (expected_model.to_string(), expected_reasoning_effort)
    );
    Ok(())
}

#[test_case(true, false; "unsupported child")]
#[test_case(false, true; "supported child")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_agent_uses_summary_support_for_final_model(
    parent_supports_summary: bool,
    child_supports_summary: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mut model_catalog = bundled_models_response().expect("bundled models.json should parse");
    for (slug, supports_summary) in [
        (INHERITED_MODEL, parent_supports_summary),
        (REQUESTED_MODEL, child_supports_summary),
    ] {
        let model = model_catalog
            .models
            .iter_mut()
            .find(|model| model.slug == slug)
            .unwrap_or_else(|| panic!("{slug} should exist in bundled models.json"));
        model.supports_reasoning_summary_parameter = supports_summary;
    }

    let (_test, _spawned_id, child_request_log) = setup_turn_one_with_custom_spawned_child(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "model": REQUESTED_MODEL,
        }),
        ChildResponseTiming::Delayed(Duration::from_secs(1)),
        /*wait_for_parent_notification*/ false,
        move |builder| {
            builder.with_config(move |config| {
                config.model_catalog = Some(model_catalog);
                config.model_reasoning_summary = Some(ReasoningSummary::Detailed);
                config
                    .features
                    .enable(Feature::ConcurrentReasoningSummaries)
                    .expect("test config should allow feature update");
            })
        },
    )
    .await?;

    let deadline = Instant::now() + Duration::from_secs(2);
    let child_body = loop {
        if let Some(body) = child_request_log
            .requests()
            .iter()
            .map(ResponsesRequest::body_json)
            .find(|body| body["model"] == REQUESTED_MODEL)
        {
            break body;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for the child request");
        }
        sleep(Duration::from_millis(10)).await;
    };
    assert_eq!(child_body["model"], json!(REQUESTED_MODEL));
    let expected_reasoning = if child_supports_summary {
        json!({"effort": "medium", "summary": "detailed"})
    } else {
        json!({"effort": "medium"})
    };
    assert_eq!(child_body["reasoning"], expected_reasoning);
    assert_eq!(
        child_body.get("stream_options").is_some(),
        child_supports_summary
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_multi_agent_v2_child_inherits_parent_developer_context() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_message": CHILD_PROMPT,
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
        config.developer_instructions = Some("Parent developer instructions.".to_string());
    });
    let test = builder.build(&server).await?;

    test.submit_turn(TURN_1_PROMPT).await?;

    let child_requests = wait_for_requests(&child_request_log).await?;
    let child_request = child_requests
        .last()
        .expect("child request log should capture at least one request");
    assert!(child_request.body_contains_text("Parent developer instructions."));
    assert!(child_request.body_contains_text(CHILD_PROMPT));

    Ok(())
}

#[derive(Clone, Copy)]
enum EncryptedFunctionArgsMarker {
    Encrypted,
    Plaintext,
}

#[test_case(
    MultiAgentMessageDelivery::Encrypted,
    None,
    EncryptedFunctionArgsMarker::Plaintext;
    "encrypted_config_overrides_plaintext_marker"
)]
#[test_case(
    MultiAgentMessageDelivery::EncryptedWithAudit,
    Some("audit-visible child task"),
    EncryptedFunctionArgsMarker::Plaintext;
    "audited_config_overrides_plaintext_marker"
)]
#[test_case(
    MultiAgentMessageDelivery::Plaintext,
    None,
    EncryptedFunctionArgsMarker::Encrypted;
    "plaintext_config_overrides_encrypted_marker"
)]
#[tokio::test]
async fn multi_agent_v2_spawn_uses_configured_delivery_over_response_marker(
    message_delivery: MultiAgentMessageDelivery,
    task_message: Option<&str>,
    encrypted_function_args_marker: EncryptedFunctionArgsMarker,
) -> Result<()> {
    let output: &'static Mutex<Vec<u8>> = Box::leak(Box::new(Mutex::new(Vec::new())));
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(Level::INFO)
        .with_writer(MockWriter::new(output))
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let server = start_mock_server().await;
    let message = match message_delivery {
        MultiAgentMessageDelivery::Encrypted | MultiAgentMessageDelivery::EncryptedWithAudit => {
            "opaque-encrypted-message"
        }
        MultiAgentMessageDelivery::Plaintext => "plaintext delegated task",
    };
    let mut spawn_args_value = json!({
        "message": message,
        "task_name": "worker",
    });
    if let Some(task_message) = task_message {
        spawn_args_value["task_message"] = json!(task_message);
    }
    let spawn_args = serde_json::to_string(&spawn_args_value)?;
    let mut spawn_event = ev_function_call_with_namespace(
        SPAWN_CALL_ID,
        MULTI_AGENT_V2_NAMESPACE,
        "spawn_agent",
        &spawn_args,
    );
    let encrypted_function_args = match encrypted_function_args_marker {
        EncryptedFunctionArgsMarker::Encrypted => json!(["message"]),
        EncryptedFunctionArgsMarker::Plaintext => json!([]),
    };
    spawn_event["item"]["encrypted_function_args"] = encrypted_function_args.clone();
    let parent_turn_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-1"),
            spawn_event,
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, "\"type\":\"agent_message\"") && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;
    let parent_replay_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID) && !request_has_input_type(req, "agent_message")
        },
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-2", "done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_model("koffing")
        .with_config(move |config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.multi_agent_v2.message_delivery = message_delivery;
        });
    let test = builder.build(&server).await?;
    let root_thread_id = test.session_configured.thread_id;

    test.submit_turn(TURN_1_PROMPT).await?;

    // The response mock records candidate requests before its request matcher runs, so wait for
    // the child request instead of assuming the latest recorded request is already it.
    let deadline = Instant::now() + Duration::from_secs(2);
    let child_request = loop {
        if let Some(request) = child_request_log
            .requests()
            .into_iter()
            .find(|request| !request.inputs_of_type("agent_message").is_empty())
        {
            break request;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for child agent message request");
        }
        sleep(Duration::from_millis(10)).await;
    };
    let content = match message_delivery {
        MultiAgentMessageDelivery::Encrypted | MultiAgentMessageDelivery::EncryptedWithAudit => {
            vec![
                json!({
                    "type": "input_text",
                    "text": "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\n",
                }),
                json!({
                    "type": "encrypted_content",
                    "encrypted_content": message,
                }),
            ]
        }
        MultiAgentMessageDelivery::Plaintext => vec![json!({
            "type": "input_text",
            "text": format!(
                "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\n{message}"
            ),
        })],
    };
    if let Some(task_message) = task_message {
        assert!(!child_request.body_contains_text(task_message));
    }
    assert_eq!(
        strip_response_item_ids_from_json(strip_metadata_from_json(Value::Array(
            child_request.inputs_of_type("agent_message"),
        ))),
        Value::Array(vec![json!({
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/worker",
            "content": content,
        })])
    );
    let parent_body = parent_turn_request_log.single_request().body_json();
    let parent_turn_id = parent_body["client_metadata"]["turn_id"]
        .as_str()
        .expect("spawn parent turn id");
    assert_parent_turn(&parent_body, /*expected*/ None)?;
    assert_parent_turn(&child_request.body_json(), Some(parent_turn_id))?;

    let replayed_parent_request = parent_replay_request_log
        .requests()
        .into_iter()
        .find(|request| {
            request
                .input()
                .iter()
                .any(|item| item["call_id"].as_str() == Some(SPAWN_CALL_ID))
        })
        .expect("parent request should replay the spawn call");
    let replayed_input = replayed_parent_request.input();
    let replayed_spawn = replayed_input
        .iter()
        .find(|item| item["call_id"].as_str() == Some(SPAWN_CALL_ID))
        .expect("replayed spawn call");
    assert_eq!(
        replayed_spawn["encrypted_function_args"],
        encrypted_function_args
    );
    let replayed_args: Value = serde_json::from_str(
        replayed_spawn["arguments"]
            .as_str()
            .expect("spawn arguments should be a JSON string"),
    )?;
    assert_eq!(replayed_args, spawn_args_value);

    let child_thread_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|thread_id| *thread_id != root_thread_id)
        .expect("child thread ID");
    let logs = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let logs = String::from_utf8(output.lock().expect("buffer lock").clone())
                .expect("logs should be UTF-8");
            if logs.contains("kind=\"spawn\"") && logs.contains("state=\"receive\"") {
                break logs;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("spawn communication logs should be emitted");
    let send = logs
        .lines()
        .find(|line| line.contains("kind=\"spawn\"") && line.contains("state=\"send\""))
        .expect("spawn send event");
    assert!(send.contains(&format!("sender_thread_id={root_thread_id}")));
    assert!(send.contains(&format!("receiver_thread_id={child_thread_id}")));
    match message_delivery {
        MultiAgentMessageDelivery::Encrypted => {
            assert_eq!(log_field(send, "content"), Some(""));
            assert!(!logs.contains(message));
            assert!(send.contains("encrypted_content_present=true"));
        }
        MultiAgentMessageDelivery::EncryptedWithAudit => {
            let task_message = task_message.expect("audited delivery should include task_message");
            assert!(send.contains(task_message));
            assert!(!send.contains(message));
            assert!(send.contains("encrypted_content_present=true"));
        }
        MultiAgentMessageDelivery::Plaintext => {
            assert!(send.contains(message));
            assert!(send.contains("encrypted_content_present=false"));
        }
    }

    let communication_id = log_field(send, "communication_id").expect("communication ID");
    logs.lines()
        .find(|line| {
            line.contains("state=\"receive\"")
                && log_field(line, "communication_id") == Some(communication_id)
        })
        .expect("correlated receive event");

    Ok(())
}

#[derive(Clone, Copy)]
enum CompletionScenario {
    Completed,
    TerminalError,
}

#[test_case(CompletionScenario::Completed ; "completed")]
#[test_case(CompletionScenario::TerminalError ; "terminal_error")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plaintext_multi_agent_v2_completion_sends_agent_message(
    scenario: CompletionScenario,
) -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "opaque-encrypted-message",
        "task_message": "audit-visible child task",
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_1_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-parent-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let child_events = match scenario {
        CompletionScenario::Completed => vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ],
        CompletionScenario::TerminalError => vec![ev_response_created("resp-child-1")],
    };
    let (child_gate_tx, child_gate_rx) = mpsc::channel();
    let child_request = mount_responder_once_match(
        &server,
        |req: &wiremock::Request| request_has_agent_message_route(req, "/root", "/root/worker"),
        GatedSseResponse {
            gate_rx: Mutex::new(Some(child_gate_rx)),
            response: sse(child_events),
        },
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID)
                && body_contains(req, "\"type\":\"function_call_output\"")
                && !body_contains(req, "Message Type: FINAL_ANSWER")
        },
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-2", "parent done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;
    let error = "stream disconnected before completion: stream closed before response.completed";
    let payload = match scenario {
        CompletionScenario::Completed => "child done".to_string(),
        CompletionScenario::TerminalError => {
            format!(
                "Agent errored: {error}\n\nThis agent's turn failed. If you still need this agent, use the available collaboration tools to give it another task."
            )
        }
    };
    let notification = format!(
        "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\n{payload}"
    );
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_2_NO_WAIT_PROMPT)
                && !body_contains(req, "Message Type: FINAL_ANSWER")
        },
        sse(vec![
            ev_response_created("resp-parent-3"),
            ev_function_call_with_namespace(
                "wait-agent-call",
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("resp-parent-3"),
        ]),
    )
    .await;
    let notification_for_request = notification.clone();
    let agent_request = mount_sse_once_match(
        &server,
        move |req: &wiremock::Request| {
            body_contains(req, TURN_2_NO_WAIT_PROMPT)
                && request_has_agent_message_text(req, &notification_for_request)
        },
        sse(vec![
            ev_response_created("resp-parent-4"),
            ev_assistant_message("msg-parent-4", "done"),
            ev_completed("resp-parent-4"),
        ]),
    )
    .await;
    let test = test_codex()
        .with_model("koffing")
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
            config.model_provider.supports_websockets = false;
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: TURN_1_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let expected_child_agent_messages = vec![json!({
        "type": "agent_message",
        "author": "/root",
        "recipient": "/root/worker",
        "content": [
            {
                "type": "input_text",
                "text": "Message Type: NEW_TASK\nTask name: /root/worker\nSender: /root\nPayload:\n",
            },
            {
                "type": "encrypted_content",
                "encrypted_content": "opaque-encrypted-message",
            },
        ],
    })];
    let _ = diagnostic_stage(
        "plaintext v2 child request",
        wait_for_agent_messages(
            &child_request,
            &expected_child_agent_messages,
            "expected child agent message request",
        ),
    )
    .await?;
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    let expected_completion = match scenario {
        CompletionScenario::Completed => (
            diagnostic_stage(
                "plaintext v2 completed presentation start",
                wait_for_event_match(&test.codex, sub_agent_completion_started_id),
            )
            .await,
            SubAgentCompletionStatus::Completed,
            "/root/worker".to_string(),
            "child done".to_string(),
        ),
        CompletionScenario::TerminalError => (
            diagnostic_stage(
                "plaintext v2 errored presentation start",
                wait_for_event_match(&test.codex, sub_agent_completion_started_id),
            )
            .await,
            SubAgentCompletionStatus::Errored,
            "/root/worker".to_string(),
            error.to_string(),
        ),
    };
    assert_eq!(
        diagnostic_stage(
            "plaintext v2 presentation completion",
            wait_for_event_match(&test.codex, sub_agent_completion_event),
        )
        .await,
        expected_completion
    );
    diagnostic_stage(
        "plaintext v2 late wait turn",
        test.submit_turn(TURN_2_NO_WAIT_PROMPT),
    )
    .await?;

    let expected_agent_messages = vec![json!({
        "type": "agent_message",
        "author": "/root/worker",
        "recipient": "/root",
        "content": [{
            "type": "input_text",
            "text": notification,
        }],
    })];
    let request = diagnostic_stage(
        "plaintext v2 parent completion context request",
        wait_for_agent_messages(
            &agent_request,
            &expected_agent_messages,
            "expected parent completion agent message request",
        ),
    )
    .await?;
    assert_eq!(
        normalize_agent_messages(request.inputs_of_type("agent_message")),
        normalize_agent_messages(expected_agent_messages)
    );
    diagnostic_stage("plaintext v2 completion flush", test.codex.flush_rollout()).await?;
    let history = read_test_rollout_items(&test)?;
    let wait_agent_states = history
        .iter()
        .rev()
        .find_map(|item| match item {
            RolloutItem::EventMsg(event) => completed_wait_agent_states(event),
            _ => None,
        })
        .expect("persisted completed wait");
    let expected_wait_status = match scenario {
        CompletionScenario::Completed => {
            codex_protocol::protocol::AgentStatus::Completed(Some("child done".to_string()))
        }
        CompletionScenario::TerminalError => {
            codex_protocol::protocol::AgentStatus::Errored(error.to_string())
        }
    };
    assert_eq!(
        wait_agent_states.values().collect::<Vec<_>>(),
        vec![&expected_wait_status]
    );
    assert_eq!(
        history.iter().rev().find_map(|item| match item {
            RolloutItem::EventMsg(event) => completed_parent_turn_has_error(event),
            _ => None,
        }),
        Some(false)
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_completion_context_survives_shutdown_before_the_next_turn() -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "start child",
        "task_message": "audit child",
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-spawn"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-spawn"),
        ]),
    )
    .await;
    let child_request = mount_response_once_match(
        &server,
        |req: &wiremock::Request| request_has_agent_message_route(req, "/root", "/root/worker"),
        sse_response(sse(vec![
            ev_response_created("resp-child"),
            ev_assistant_message("msg-child", "child done"),
            ev_completed("resp-child"),
        ]))
        .set_delay(Duration::from_secs(2)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID)
                && body_contains(req, "\"type\":\"function_call_output\"")
        },
        sse(vec![
            ev_response_created("resp-parent-finished"),
            ev_assistant_message("msg-parent-finished", "parent done"),
            ev_completed("resp-parent-finished"),
        ]),
    )
    .await;
    let notification =
        "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\nchild done";
    let notification_for_request = notification.to_string();
    let resumed_request = mount_sse_once_match(
        &server,
        move |req: &wiremock::Request| {
            body_contains(req, TURN_AFTER_RESUME_PROMPT)
                && request_has_agent_message_text(req, &notification_for_request)
        },
        sse(vec![
            ev_response_created("resp-after-resume"),
            ev_assistant_message("msg-after-resume", "completion retained"),
            ev_completed("resp-after-resume"),
        ]),
    )
    .await;
    let mut builder = test_codex().with_model("koffing").with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
    });
    let initial = builder.build(&server).await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;

    initial.submit_turn(TURN_1_PROMPT).await?;
    let child_thread_id = ThreadId::from_string(&wait_for_spawned_thread_id(&initial).await?)?;
    let child_thread = initial.thread_manager.get_thread(child_thread_id).await?;
    let _ = wait_for_requests(&child_request).await?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if matches!(
            child_thread.agent_status().await,
            codex_protocol::protocol::AgentStatus::Completed(_)
                | codex_protocol::protocol::AgentStatus::Errored(_)
                | codex_protocol::protocol::AgentStatus::Shutdown
                | codex_protocol::protocol::AgentStatus::NotFound
        ) {
            break;
        }
        if Instant::now() >= deadline {
            anyhow::bail!("timed out waiting for child terminal publication");
        }
        tokio::task::yield_now().await;
    }
    initial.codex.submit(Op::Shutdown).await?;
    let started_id = wait_for_event_match(&initial.codex, sub_agent_completion_started_id).await;
    assert_eq!(
        wait_for_event_match(&initial.codex, sub_agent_completion_event).await,
        (
            started_id,
            SubAgentCompletionStatus::Completed,
            "/root/worker".to_string(),
            "child done".to_string(),
        )
    );
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    builder = builder.with_model("koffing").with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config
            .features
            .enable(Feature::MultiAgentV2)
            .expect("test config should allow feature update");
    });
    let resumed = builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn(TURN_AFTER_RESUME_PROMPT).await?;

    let expected_agent_messages = vec![json!({
        "type": "agent_message",
        "author": "/root/worker",
        "recipient": "/root",
        "content": [{
            "type": "input_text",
            "text": notification,
        }],
    })];
    let request = wait_for_agent_messages(
        &resumed_request,
        &expected_agent_messages,
        "expected persisted completion context after shutdown and resume",
    )
    .await?;
    assert_eq!(
        normalize_agent_messages(request.inputs_of_type("agent_message")),
        normalize_agent_messages(expected_agent_messages)
    );

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn v2_accepted_completion_survives_exact_rollback_and_cold_resume(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "start child",
        "task_message": "audit child",
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-spawn-before-rollback"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-spawn-before-rollback"),
        ]),
    )
    .await;
    let child_request = mount_response_once_match(
        &server,
        |req: &wiremock::Request| request_has_agent_message_route(req, "/root", "/root/worker"),
        sse_response(sse(vec![
            ev_response_created("resp-child-before-rollback"),
            ev_assistant_message("msg-child-before-rollback", "child done"),
            ev_completed("resp-child-before-rollback"),
        ]))
        .set_delay(Duration::from_secs(/*secs*/ 5)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID)
                && body_contains(req, "\"type\":\"function_call_output\"")
        },
        sse(vec![
            ev_response_created("resp-parent-before-rollback"),
            ev_assistant_message("msg-parent-before-rollback", "parent done"),
            ev_completed("resp-parent-before-rollback"),
        ]),
    )
    .await;
    let notification =
        "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\nchild done";
    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-v2-after-rollback-resume"),
            ev_assistant_message("msg-v2-after-rollback-resume", "completion retained"),
            ev_completed("resp-v2-after-rollback-resume"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_model("koffing")
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        });
    let initial = builder.build(&server).await?;
    let home = initial.home.clone();
    let rollout_path = initial
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;

    initial.submit_turn(TURN_1_PROMPT).await?;
    let _ = wait_for_requests(&child_request).await?;
    let started_id = wait_for_event_match(&initial.codex, sub_agent_completion_started_id).await;
    let completion = wait_for_event_match(&initial.codex, sub_agent_completion_event).await;
    let expected_completion = (
        started_id,
        SubAgentCompletionStatus::Completed,
        "/root/worker".to_string(),
        "child done".to_string(),
    );
    assert_eq!(completion, expected_completion);

    initial
        .codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ThreadRolledBack(_)).then_some(())
    })
    .await;
    initial.codex.flush_rollout().await?;

    let rollout_items = read_test_rollout_items(&initial)?;
    let effective_history = rollout_without_exact_rollback_ranges(&rollout_items);
    assert!(effective_history.iter().any(|item| {
        matches!(
            item,
            RolloutItem::ResponseItem(ResponseItem::AgentMessage { id: Some(id), .. })
                if is_sub_agent_completion_context_response_item_id(id)
        )
    }));
    assert_eq!(
        effective_history
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
                )
            })
            .count(),
        1
    );

    initial.codex.submit(Op::Shutdown).await?;
    wait_for_event_match(&initial.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    builder = builder
        .with_model("koffing")
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        });
    let resumed = builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn(TURN_AFTER_RESUME_PROMPT).await?;

    let expected_agent_messages = vec![json!({
        "type": "agent_message",
        "author": "/root/worker",
        "recipient": "/root",
        "content": [{
            "type": "input_text",
            "text": notification,
        }],
    })];
    let request = wait_for_agent_messages(
        &resumed_request,
        &expected_agent_messages,
        "expected the accepted v2 completion after exact rollback and cold resume",
    )
    .await?;
    assert_eq!(
        normalize_agent_messages(request.inputs_of_type("agent_message")),
        normalize_agent_messages(expected_agent_messages)
    );

    Ok(())
}

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn active_multi_agent_v2_wait_suppresses_background_completion_item(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    let server = start_mock_server().await;
    let resumed_request = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_AFTER_RESUME_PROMPT),
        sse(vec![
            ev_response_created("resp-after-wait-rollback-resume"),
            ev_assistant_message("msg-after-wait-rollback-resume", "wait retained"),
            ev_completed("resp-after-wait-rollback-resume"),
        ]),
    )
    .await;
    let spawn_args = serde_json::to_string(&json!({
        "message": "start child",
        "task_message": "audit child",
        "task_name": "worker",
    }))?;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-parent-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-parent-1"),
        ]),
    )
    .await;
    let child_request = mount_response_once_match(
        &server,
        |req: &wiremock::Request| request_has_agent_message_route(req, "/root", "/root/worker"),
        sse_response(sse(vec![
            ev_response_created("resp-child-1"),
            ev_assistant_message("msg-child-1", "child done"),
            ev_completed("resp-child-1"),
        ]))
        .set_delay(Duration::from_secs(5)),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, SPAWN_CALL_ID)
                && body_contains(req, "\"type\":\"function_call_output\"")
        },
        sse(vec![
            ev_response_created("resp-parent-2"),
            ev_assistant_message("msg-parent-2", "parent done"),
            ev_completed("resp-parent-2"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, TURN_2_NO_WAIT_PROMPT)
                && !body_contains(req, "Message Type: FINAL_ANSWER")
        },
        sse(vec![
            ev_response_created("resp-parent-3"),
            ev_function_call_with_namespace(
                "wait-agent-call",
                MULTI_AGENT_V2_NAMESPACE,
                "wait_agent",
                "{}",
            ),
            ev_completed("resp-parent-3"),
        ]),
    )
    .await;
    let notification =
        "Message Type: FINAL_ANSWER\nTask name: /root\nSender: /root/worker\nPayload:\nchild done";
    let notification_for_request = notification.to_string();
    let agent_request = mount_sse_once_match(
        &server,
        move |req: &wiremock::Request| {
            request_has_agent_message_text(req, &notification_for_request)
        },
        sse(vec![
            ev_response_created("resp-parent-4"),
            ev_assistant_message("msg-parent-4", "done"),
            ev_completed("resp-parent-4"),
        ]),
    )
    .await;
    let mut builder = test_codex()
        .with_model("koffing")
        .with_history_mode(history_mode)
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
        });
    let test = builder.build(&server).await?;
    let home = test.home.clone();
    let rollout_path = test
        .codex
        .rollout_path()
        .ok_or_else(|| anyhow::anyhow!("expected parent rollout path"))?;

    test.submit_turn(TURN_1_PROMPT).await?;
    let _ = wait_for_requests(&child_request).await?;
    test.codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: TURN_2_NO_WAIT_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        })
        .await?;
    let mut turn_id = None;
    let mut wait_started = false;
    wait_for_event_with_timeout(
        &test.codex,
        |event| {
            if turn_id.is_none()
                && let EventMsg::TurnStarted(event) = event
            {
                turn_id = Some(event.turn_id.clone());
            }
            if matches!(
                event,
                EventMsg::ItemStarted(event)
                    if matches!(
                        &event.item,
                        TurnItem::CollabAgentToolCall(item)
                            if item.tool == CollabAgentTool::Wait
                    )
            ) {
                wait_started = true;
            }
            matches!(
                event,
                EventMsg::TurnComplete(event)
                    if turn_id.as_deref() == Some(event.turn_id.as_str())
            )
        },
        Duration::from_secs(/*secs*/ 30),
    )
    .await;
    if !wait_started {
        anyhow::bail!("parent turn completed before wait_agent started");
    }
    let expected_agent_messages = vec![json!({
        "type": "agent_message",
        "author": "/root/worker",
        "recipient": "/root",
        "content": [{
            "type": "input_text",
            "text": notification,
        }],
    })];
    let request = wait_for_agent_messages(
        &agent_request,
        &expected_agent_messages,
        "expected completion delivery through active wait",
    )
    .await?;
    assert_input_item_ids_are_provider_compatible(&request);
    test.codex.flush_rollout().await?;
    let history = read_test_rollout_items(&test)?;
    let wait_agent_states = history
        .iter()
        .find_map(|item| match item {
            RolloutItem::EventMsg(event) => completed_wait_agent_states(event),
            _ => None,
        })
        .expect("persisted completed wait");
    assert_eq!(wait_agent_states.len(), 1);
    assert_eq!(
        wait_agent_states.values().next(),
        Some(&codex_protocol::protocol::AgentStatus::Completed(Some(
            "child done".to_string()
        )))
    );
    assert!(!history.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
        )
    }));
    assert_eq!(
        history.iter().find_map(|item| match item {
            RolloutItem::EventMsg(event) => completed_wait_agent_states(event),
            _ => None,
        }),
        Some(wait_agent_states.clone())
    );

    test.codex
        .submit(Op::ThreadRollback { num_turns: 1 })
        .await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::ThreadRolledBack(_)).then_some(())
    })
    .await;
    test.codex.flush_rollout().await?;
    let rollout_items = read_test_rollout_items(&test)?;
    let effective_history = rollout_without_exact_rollback_ranges(&rollout_items);
    assert_eq!(
        effective_history.iter().find_map(|item| match item {
            RolloutItem::EventMsg(event) => completed_wait_agent_states(event),
            _ => None,
        }),
        Some(wait_agent_states.clone())
    );
    assert!(!effective_history.iter().any(|item| {
        matches!(
            item,
            RolloutItem::EventMsg(event) if sub_agent_completion_event(event).is_some()
        )
    }));

    test.codex.submit(Op::Shutdown).await?;
    wait_for_event_match(&test.codex, |event| {
        matches!(event, EventMsg::ShutdownComplete).then_some(())
    })
    .await;

    let resumed = builder.resume(&server, home, rollout_path).await?;
    resumed.submit_turn(TURN_AFTER_RESUME_PROMPT).await?;
    let request = wait_for_agent_messages(
        &resumed_request,
        &expected_agent_messages,
        "expected completion context after wait-owned exact rollback and cold resume",
    )
    .await?;
    assert_eq!(
        normalize_agent_messages(request.inputs_of_type("agent_message")),
        normalize_agent_messages(expected_agent_messages)
    );
    assert_input_item_ids_are_provider_compatible(&request);

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn skills_toggle_skips_instructions_for_parent_and_spawned_child() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "task_message": CHILD_PROMPT,
        "task_name": "worker",
    }))?;
    let spawn_turn = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V2_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;

    let child_request_log = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| {
            body_contains(req, CHILD_PROMPT) && !body_contains(req, SPAWN_CALL_ID)
        },
        sse(vec![
            ev_response_created("resp-child-1"),
            ev_completed("resp-child-1"),
        ]),
    )
    .await;

    let _turn1_followup = mount_sse_once_match(
        &server,
        |req: &wiremock::Request| body_contains(req, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_assistant_message("msg-turn1-2", "parent done"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let mut builder = test_codex()
        .with_pre_build_hook(|home| {
            write_home_skill(home, "demo", "demo-skill", "demo skill").expect("write home skill");
        })
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            config
                .features
                .enable(Feature::MultiAgentV2)
                .expect("test config should allow feature update");
            config.include_skill_instructions = false;
        });
    let test = builder.build(&server).await?;

    test.submit_turn(TURN_1_PROMPT).await?;
    let parent_request = spawn_turn.single_request();
    assert!(!parent_request.body_contains_text("<skills_instructions>"));
    assert!(!parent_request.body_contains_text("demo-skill"));

    let child_requests = wait_for_requests(&child_request_log).await?;
    let child_request = child_requests
        .last()
        .expect("child request log should capture at least one request");
    assert!(!child_request.body_contains_text("<skills_instructions>"));
    assert!(!child_request.body_contains_text("demo-skill"));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_role_overrides_requested_model_and_reasoning_settings() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot = spawn_child_and_capture_snapshot(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "agent_type": "custom",
            "model": REQUESTED_MODEL,
            "reasoning_effort": REQUESTED_REASONING_EFFORT,
        }),
        |builder| {
            builder.with_config(|config| {
                let role_path = config.codex_home.join("custom-role.toml");
                std::fs::write(
                    &role_path,
                    format!(
                        "model = \"{ROLE_MODEL}\"\nmodel_reasoning_effort = \"{ROLE_REASONING_EFFORT}\"\n",
                    ),
                )
                .expect("write role config");
                config.agent_roles.insert(
                    "custom".to_string(),
                    AgentRoleConfig {
                        description: Some("Custom role".to_string()),
                        config_file: Some(role_path.to_path_buf()),
                        nickname_candidates: None,
                    },
                );
            })
        },
    )
    .await?;

    assert_eq!(child_snapshot.model, ROLE_MODEL);
    assert_eq!(child_snapshot.reasoning_effort, Some(ROLE_REASONING_EFFORT));

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_preserves_configured_defaults_through_unrelated_role() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let child_snapshot = spawn_child_and_capture_snapshot(
        &server,
        json!({
            "message": CHILD_PROMPT,
            "agent_type": "custom",
        }),
        |builder| {
            builder.with_config(|config| {
                let role_path = config.codex_home.join("instructions-only-role.toml");
                std::fs::write(&role_path, "developer_instructions = \"Stay focused\"\n")
                    .expect("write role config");
                config.agent_roles.insert(
                    "custom".to_string(),
                    AgentRoleConfig {
                        description: Some("Custom role".to_string()),
                        config_file: Some(role_path.to_path_buf()),
                        nickname_candidates: None,
                    },
                );
                config.agent_default_subagent_model = Some(REQUESTED_MODEL.to_string());
                config.agent_default_subagent_reasoning_effort = Some(REQUESTED_REASONING_EFFORT);
            })
        },
    )
    .await?;

    assert_eq!(
        (child_snapshot.model, child_snapshot.reasoning_effort),
        (
            REQUESTED_MODEL.to_string(),
            Some(REQUESTED_REASONING_EFFORT)
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_rejects_reasoning_effort_unsupported_by_role_model() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let spawn_args = serde_json::to_string(&json!({
        "message": CHILD_PROMPT,
        "agent_type": "custom",
    }))?;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, TURN_1_PROMPT),
        sse(vec![
            ev_response_created("resp-turn1-1"),
            ev_function_call_with_namespace(
                SPAWN_CALL_ID,
                MULTI_AGENT_V1_NAMESPACE,
                "spawn_agent",
                &spawn_args,
            ),
            ev_completed("resp-turn1-1"),
        ]),
    )
    .await;
    let tool_output = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| body_contains(request, SPAWN_CALL_ID),
        sse(vec![
            ev_response_created("resp-turn1-2"),
            ev_completed("resp-turn1-2"),
        ]),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            config
                .features
                .enable(Feature::Collab)
                .expect("test config should allow feature update");
            let role_path = config.codex_home.join("model-only-role.toml");
            std::fs::write(&role_path, format!("model = \"{ROLE_MODEL}\"\n"))
                .expect("write role config");
            config.agent_roles.insert(
                "custom".to_string(),
                AgentRoleConfig {
                    description: Some("Custom role".to_string()),
                    config_file: Some(role_path.to_path_buf()),
                    nickname_candidates: None,
                },
            );
            config.agent_default_subagent_model = Some("gpt-5.6-sol".to_string());
            config.agent_default_subagent_reasoning_effort = Some(ReasoningEffort::Ultra);
        })
        .build_with_auto_env(&server)
        .await?;

    test.submit_turn(TURN_1_PROMPT).await?;

    let (output, _) = tool_output
        .single_request()
        .function_call_output_content_and_success(SPAWN_CALL_ID)
        .expect("spawn_agent output");
    assert_eq!(
        output.as_deref(),
        Some(
            "Reasoning effort `ultra` is not supported for model `gpt-5.4`. Supported reasoning efforts: low, medium, high, xhigh"
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_agent_tool_description_mentions_role_locked_settings() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "tool-search-spawn-agent";
    let resp_mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-turn1-1"),
                ev_tool_search_call(
                    call_id,
                    &json!({
                        "query": "spawn agent custom role",
                        "limit": 1,
                    }),
                ),
                ev_completed("resp-turn1-1"),
            ]),
            sse(vec![
                ev_response_created("resp-turn1-2"),
                ev_assistant_message("msg-turn1-2", "done"),
                ev_completed("resp-turn1-2"),
            ]),
        ],
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config
            .features
            .enable(Feature::Collab)
            .expect("test config should allow feature update");
        config.multi_agent_v2.hide_spawn_agent_metadata = false;
        let role_path = config.codex_home.join("custom-role.toml");
        std::fs::write(
            &role_path,
            format!(
                "developer_instructions = \"Stay focused\"\nmodel = \"{ROLE_MODEL}\"\nmodel_reasoning_effort = \"{ROLE_REASONING_EFFORT}\"\n",
            ),
        )
        .expect("write role config");
        config.agent_roles.insert(
            "custom".to_string(),
            AgentRoleConfig {
                description: Some("Custom role".to_string()),
                config_file: Some(role_path.to_path_buf()),
                nickname_candidates: None,
            },
        );
    });
    let test = builder.build(&server).await?;

    test.submit_turn(TURN_1_PROMPT).await?;

    let requests = resp_mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].tool_search_output(call_id);
    let spawn_agent = namespace_child_tool(&output, "multi_agent_v1", "spawn_agent")
        .expect("tool_search should return multi_agent_v1.spawn_agent");
    let agent_type_description = tool_parameter_description(spawn_agent, "agent_type")
        .expect("spawn_agent agent_type description");
    let custom_role_description =
        role_block(&agent_type_description, "custom").expect("custom role description");
    assert_eq!(
        custom_role_description,
        "custom: {\nCustom role\n- This role's model is set to `gpt-5.4` and its reasoning effort is set to `high`. These settings cannot be changed.\n}"
    );

    Ok(())
}
