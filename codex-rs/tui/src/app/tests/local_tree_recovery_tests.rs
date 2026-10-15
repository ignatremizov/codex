//! Exercise recovery through the TUI session facade against a genuinely crashed owner.
//! Only the test-owned subprocess is killed; canonical history and SQLite are never edited.

use super::*;
use crate::app_server_session::SpawnAgentRequest;
use app_test_support::MockResponsesConfig;
use codex_app_server_protocol::AgentControlOutcome;
use codex_app_server_protocol::AgentForkMode;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadSettingsUpdateParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::Settings;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde::Deserialize;
use serde::Serialize;
use std::process::Command;
use std::process::Stdio;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

const CHILD_HOME: &str = "CODEX_TUI_TREE_RECOVERY_TEST_HOME";
const CHILD_CWD: &str = "CODEX_TUI_TREE_RECOVERY_TEST_CWD";
const CHILD_SCENARIO: &str = "CODEX_TUI_TREE_RECOVERY_TEST_SCENARIO";
const PROMPT: &str = "Finish this child-owned task after recovery.";

#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
enum RecoveryScenario {
    UnfinishedModel,
    UnfinishedDescendant,
    ChangedSettings,
    ChangedSummary,
    UnfinishedTool,
}

#[derive(Serialize, Deserialize)]
struct DurableTree {
    root: ThreadId,
    idle: ThreadId,
    closed: ThreadId,
    ancestor: ThreadId,
    running: ThreadId,
    turn_id: String,
}

async fn recovery_config(home: &Path, cwd: &Path) -> Result<Config> {
    Ok(ConfigBuilder::default()
        .codex_home(home.to_path_buf())
        .harness_overrides(ConfigOverrides {
            cwd: Some(cwd.to_path_buf()),
            ..Default::default()
        })
        .loader_overrides(LoaderOverrides::without_managed_config_for_tests())
        .build()
        .await?)
}

// This ordinary test is inert unless invoked by the parent with its owned temporary home.
#[tokio::test]
async fn tree_owner_child() -> Result<()> {
    let Some(home) = std::env::var_os(CHILD_HOME) else {
        return Ok(());
    };
    let home = PathBuf::from(home);
    let cwd = PathBuf::from(std::env::var_os(CHILD_CWD).ok_or_else(|| {
        color_eyre::eyre::eyre!("test subprocess is missing its working directory")
    })?);
    let scenario: RecoveryScenario = serde_json::from_str(&std::env::var(CHILD_SCENARIO)?)?;
    let config = recovery_config(&home, &cwd).await?;
    let mut owner = crate::start_embedded_app_server_for_picker(&config).await?;
    let root = owner.start_thread(&config).await?.session.thread_id;
    let mut children = Vec::new();
    for task in ["idle", "closed", "ancestor", "running"] {
        let response = owner
            .spawn_agent(SpawnAgentRequest {
                source_thread_id: if task == "running" { children[2] } else { root },
                task: Some(task.into()),
                role: None,
                authored_selector: Some("new".into()),
                model: None,
                reasoning_effort: None,
                input: None,
                fork_mode: AgentForkMode::None,
                response_handling: None,
            })
            .await?;
        assert_eq!(response.audit_warning, None);
        let AgentControlOutcome::Spawned {
            target_thread_id,
            post_admission_warning,
            ..
        } = response.outcome
        else {
            color_eyre::eyre::bail!("host spawn did not create a child");
        };
        assert_eq!(post_admission_warning, None);
        let id = ThreadId::from_string(&target_thread_id)?;
        children.push(id);
        if task == "idle" {
            // Unsubscribe unloads only this idle runtime, retaining its open ownership edge.
            // Explicit thread/unload would unload the entire owning tree, including Main.
            owner.thread_unsubscribe(id).await?;
            tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
                loop {
                    if !owner
                        .thread_loaded_list(Default::default())
                        .await?
                        .data
                        .contains(&id.to_string())
                    {
                        return Ok::<_, color_eyre::Report>(());
                    }
                    tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
                }
            })
            .await??;
        } else if task == "closed" {
            owner
                .close_agent(
                    root,
                    id.to_string(),
                    id.to_string(),
                    /*response_handling*/ None,
                )
                .await?;
        }
    }
    let [idle, closed, ancestor, running] = children.as_slice() else {
        color_eyre::eyre::bail!("missing spawned children");
    };
    let mode_settings = Settings {
        model: config
            .model
            .clone()
            .ok_or_else(|| color_eyre::eyre::eyre!("test model is missing"))?,
        reasoning_effort: config.model_reasoning_effort,
        developer_instructions: None,
    };
    let _: TurnStartResponse = owner
        .request_handle()
        .request_typed(ClientRequest::TurnStart {
            request_id: RequestId::String("start-running".into()),
            params: TurnStartParams {
                thread_id: running.to_string(),
                input: vec![AppServerUserInput::Text {
                    text: PROMPT.into(),
                    text_elements: Vec::new(),
                }],
                service_tier_for_turn: Some("default".into()),
                summary: Some(ReasoningSummary::Detailed),
                collaboration_mode: Some(CollaborationMode {
                    mode: ModeKind::Plan,
                    settings: mode_settings.clone(),
                }),
                output_schema: Some(serde_json::json!({
                    "type": "object",
                    "properties": { "ok": { "type": "boolean" } },
                    "required": ["ok"],
                    "additionalProperties": false,
                })),
                ..Default::default()
            },
        })
        .await?;

    let turn_id = tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
        loop {
            let thread = owner.thread_read(*running, /*include_turns*/ true).await?;
            assert_eq!(
                thread.history_mode,
                codex_app_server_protocol::ThreadHistoryMode::Paginated
            );
            if let Some(turn) = thread.turns.last()
                && turn.status == TurnStatus::InProgress
                && let Some(path) = thread.path
            {
                let contents = std::fs::read_to_string(path)?;
                // Wait for disk evidence, not merely turn admission or an HTTP request.
                let records = contents
                    .lines()
                    .map(serde_json::from_str::<serde_json::Value>)
                    .collect::<std::result::Result<Vec<_>, _>>();
                let Ok(records) = records else {
                    // The writer may still be appending the last JSONL record.
                    tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
                    continue;
                };
                let started = records.iter().any(|record| {
                    record["type"] == "event_msg"
                        && record["payload"]["type"] == "task_started"
                        && record["payload"]["turn_id"] == turn.id
                });
                let context = records.iter().any(|record| {
                    record["type"] == "turn_context" && record["payload"]["turn_id"] == turn.id
                });
                let input = records.iter().any(|record| {
                    record["type"] == "response_item"
                        && record["payload"]["role"] == "user"
                        && record.to_string().contains(PROMPT)
                });
                let tool_pending = records.iter().any(|record| {
                    record["type"] == "response_item"
                        && record["payload"]["type"] == "function_call"
                        && record["payload"]["call_id"] == "pending-tool"
                }) && !records.iter().any(|record| {
                    record["type"] == "response_item"
                        && record["payload"]["type"] == "function_call_output"
                        && record["payload"]["call_id"] == "pending-tool"
                });
                if started
                    && context
                    && input
                    && (scenario != RecoveryScenario::UnfinishedTool || tool_pending)
                {
                    return Ok::<_, color_eyre::Report>(turn.id.clone());
                }
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
    })
    .await??;
    if matches!(
        scenario,
        RecoveryScenario::ChangedSettings | RecoveryScenario::ChangedSummary
    ) {
        assert!(
            owner
                .thread_settings_update(ThreadSettingsUpdateParams {
                    thread_id: running.to_string(),
                    collaboration_mode: (scenario == RecoveryScenario::ChangedSettings).then_some(
                        CollaborationMode {
                            mode: ModeKind::Default,
                            settings: mode_settings,
                        }
                    ),
                    summary: (scenario == RecoveryScenario::ChangedSummary)
                        .then_some(ReasoningSummary::None),
                    ..Default::default()
                })
                .await?
        );
        // This read flushes the changed settings before the parent kills the owner.
        owner.thread_read(*running, /*include_turns*/ true).await?;
    }
    std::fs::write(
        home.join("ready.json"),
        serde_json::to_vec(&DurableTree {
            root,
            idle: *idle,
            closed: *closed,
            ancestor: *ancestor,
            running: *running,
            turn_id,
        })?,
    )?;
    // Keep the process alive without graceful shutdown until the parent kills it.
    std::future::pending().await
}

struct TreeOwner(std::process::Child);

impl Drop for TreeOwner {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn crashed_child_selection_preserves_original_turn_and_output_constraints() -> Result<()> {
    crashed_child_selection_restores_tree_and_checks_original_turn(
        RecoveryScenario::UnfinishedModel,
    )
    .await
}

#[tokio::test]
async fn crashed_root_selection_restores_only_unfinished_descendants_and_required_owners()
-> Result<()> {
    crashed_child_selection_restores_tree_and_checks_original_turn(
        RecoveryScenario::UnfinishedDescendant,
    )
    .await
}

#[tokio::test]
async fn crashed_child_selection_pauses_when_settings_changed_during_original_turn() -> Result<()> {
    crashed_child_selection_restores_tree_and_checks_original_turn(
        RecoveryScenario::ChangedSettings,
    )
    .await
}

#[tokio::test]
async fn crashed_child_selection_keeps_newer_summary_instead_of_original_turn_summary() -> Result<()>
{
    crashed_child_selection_restores_tree_and_checks_original_turn(RecoveryScenario::ChangedSummary)
        .await
}

#[tokio::test]
async fn crashed_child_selection_continues_when_persisted_tool_call_has_no_outcome() -> Result<()> {
    crashed_child_selection_restores_tree_and_checks_original_turn(RecoveryScenario::UnfinishedTool)
        .await
}

async fn crashed_child_selection_restores_tree_and_checks_original_turn(
    scenario: RecoveryScenario,
) -> Result<()> {
    let home = tempdir()?;
    let cwd = tempdir()?;
    let model = responses::start_mock_server().await;
    MockResponsesConfig::new(&model.uri())
        .enable_feature(Feature::Collab)
        .disable_feature(Feature::MultiAgentV2)
        .with_root_config("thread_unload_delay_secs = 0")
        .with_extra_config(
            "[features.current_time_reminder]\nenabled = true\nsleep_tool = true\n\
             [agents]\nmax_concurrent_threads_per_session = 3\nmax_depth = 2",
        )
        .write(home.path())?;
    let original_response = if scenario == RecoveryScenario::UnfinishedTool {
        // Leave a child-authorized timer pending, with a durable call but no output.
        // Root-only interactive tools would return an error rather than reliably stay pending.
        ResponseTemplate::new(/*s*/ 200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(responses::sse(vec![
                responses::ev_response_created("pending-tool-response"),
                responses::ev_function_call_with_namespace(
                    "pending-tool",
                    "clock",
                    "sleep",
                    r#"{"duration_ms":120000}"#,
                ),
                responses::ev_completed("pending-tool-response"),
            ]))
    } else {
        ResponseTemplate::new(/*s*/ 200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(responses::sse_completed("never-completes-before-crash"))
            .set_delay(Duration::from_secs(/*secs*/ 120))
    };
    Mock::given(method("POST"))
        .and(path_regex(".*/responses$"))
        .respond_with(original_response)
        .mount(&model)
        .await;
    let mut child = TreeOwner(
        Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "app::tests::local_tree_recovery::tree_owner_child",
                "--nocapture",
            ])
            .env(CHILD_HOME, home.path())
            .env(CHILD_CWD, cwd.path())
            .env(CHILD_SCENARIO, serde_json::to_string(&scenario)?)
            .stdin(Stdio::piped())
            .spawn()?,
    );
    let tree: DurableTree = tokio::time::timeout(Duration::from_secs(/*secs*/ 45), async {
        loop {
            if let Some(status) = child.0.try_wait()? {
                color_eyre::eyre::bail!("owner exited before durable readiness: {status}");
            }
            match std::fs::read(home.path().join("ready.json")) {
                Ok(bytes) => {
                    // A concurrent write can be observed before its complete JSON is visible.
                    if let Ok(tree) = serde_json::from_slice::<DurableTree>(&bytes) {
                        return Ok(tree);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(color_eyre::Report::from(error)),
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
    })
    .await??;
    let (original_user_messages, original_format, original_tier, original_summary) =
        tokio::time::timeout(Duration::from_secs(/*secs*/ 10), async {
            loop {
                let requests = model.received_requests().await.unwrap_or_default();
                if let Some(request) = requests
                    .iter()
                    .find(|request| request.url.path().ends_with("/responses"))
                {
                    let body: serde_json::Value = serde_json::from_slice(&request.body)?;
                    let input = body["input"]
                        .as_array()
                        .ok_or_else(|| color_eyre::eyre::eyre!("missing original model input"))?;
                    return Ok::<_, color_eyre::Report>((
                        input
                            .iter()
                            .filter(|item| item["role"] == "user")
                            .cloned()
                            .collect::<Vec<_>>(),
                        body["text"]["format"].clone(),
                        body["service_tier"].clone(),
                        body["reasoning"]["summary"].clone(),
                    ));
                }
                tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
            }
        })
        .await??;
    assert!(
        original_user_messages
            .iter()
            .any(|item| item.to_string().contains(PROMPT))
    );
    assert_eq!(original_format["type"], "json_schema");
    assert_eq!(original_summary, "detailed");
    child.0.kill()?;
    assert!(!child.0.wait()?.success());
    model.reset().await;
    let response = responses::mount_sse_once(&model, responses::sse_completed("recovered")).await;
    // V1 keeps an unsubscribed agent's registration in its original owner. Recovery must
    // fit the unfinished child and required ancestor into two fresh slots, excluding it.
    let config_path = home.path().join("config.toml");
    let mut saved_config: toml::Value = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
    saved_config["agents"]["max_concurrent_threads_per_session"] = toml::Value::Integer(2);
    std::fs::write(config_path, toml::to_string(&saved_config)?)?;
    let config = recovery_config(home.path(), cwd.path()).await?;
    let mut replacement = crate::start_embedded_app_server_for_picker(&config).await?;
    let plan = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 30),
        replacement.prepare_local_tree_recovery(
            if scenario == RecoveryScenario::UnfinishedDescendant {
                tree.root
            } else {
                tree.running
            },
        ),
    )
    .await??;
    assert_eq!(plan.root_thread_id(), tree.root);
    let mut actual = plan.thread_ids().to_vec();
    actual.sort_by_key(ToString::to_string);
    let mut expected = vec![tree.root, tree.ancestor, tree.running];
    expected.sort_by_key(ToString::to_string);
    assert_eq!(actual, expected);
    let mut loaded = replacement
        .thread_loaded_list(Default::default())
        .await?
        .data;
    loaded.sort();
    assert_eq!(
        loaded,
        expected.iter().map(ToString::to_string).collect::<Vec<_>>()
    );
    if matches!(
        scenario,
        RecoveryScenario::ChangedSettings | RecoveryScenario::ChangedSummary
    ) {
        assert_eq!(plan.warnings().len(), 1, "{:?}", plan.warnings());
        assert!(plan.warnings()[0].contains(&tree.running.to_string()));
        if scenario == RecoveryScenario::ChangedSummary {
            assert!(plan.warnings()[0].contains("reasoning summary"));
        }
    } else {
        assert!(plan.warnings().is_empty(), "{:?}", plan.warnings());
    }
    assert!(
        response.requests().is_empty(),
        "preparation must not run a model"
    );
    let Err(error) = replacement.prepare_local_tree_recovery(tree.root).await else {
        color_eyre::eyre::bail!("replacement accepted a second recovery plan");
    };
    assert!(
        error
            .to_string()
            .contains("local recovery was already attempted"),
        "{error:#}"
    );

    let (mut app, _events, _operations) = make_test_app_with_channels().await;
    app.chat_widget
        .handle_thread_session(test_thread_session(tree.running, cwd.path().to_path_buf()));
    app.chat_widget
        .set_queue_autosend_suppressed(/*suppressed*/ true);
    app.chat_widget
        .apply_external_edit("queued after recovery".into());
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE));
    app.chat_widget
        .set_queue_autosend_suppressed(/*suppressed*/ false);
    let outcome = tokio::time::timeout(
        Duration::from_secs(/*secs*/ 30),
        app.continue_recovered_local_tree(&replacement, plan),
    )
    .await?
    .ok_or_else(|| color_eyre::eyre::eyre!("recovery continuation was not confirmed"))?;
    assert!(!app.chat_widget.maybe_send_next_queued_input());
    assert_eq!(
        app.chat_widget.queued_user_message_texts(),
        vec!["queued after recovery"]
    );
    if matches!(
        scenario,
        RecoveryScenario::ChangedSettings | RecoveryScenario::ChangedSummary
    ) {
        assert!(outcome.resumed_turns.is_empty());
        let changed_setting = if scenario == RecoveryScenario::ChangedSettings {
            "collaboration mode"
        } else {
            "reasoning summary"
        };
        assert_eq!(
            outcome.paused_threads,
            vec![(
                tree.running,
                format!(
                    "original turn settings differ from restored thread settings ({changed_setting}); explicit retry is required"
                ),
            )]
        );
        assert!(response.requests().is_empty());
        replacement.shutdown().await?;
        return Ok(());
    }
    assert!(
        outcome.paused_threads.is_empty(),
        "{:?}",
        outcome.paused_threads
    );
    assert_eq!(outcome.resumed_turns, vec![tree.running]);
    let completed = tokio::time::timeout(Duration::from_secs(/*secs*/ 30), async {
        loop {
            let thread = replacement
                .thread_read(tree.running, /*include_turns*/ true)
                .await?;
            if thread
                .turns
                .last()
                .is_some_and(|turn| turn.status == TurnStatus::Completed)
            {
                return Ok::<_, color_eyre::Report>(thread);
            }
            tokio::time::sleep(Duration::from_millis(/*millis*/ 20)).await;
        }
    })
    .await??;
    assert_eq!(
        completed
            .turns
            .iter()
            .map(|turn| turn.id.as_str())
            .collect::<Vec<_>>(),
        vec![tree.turn_id.as_str()]
    );
    for idle_thread in [tree.root, tree.idle, tree.ancestor] {
        let thread = replacement
            .thread_read(idle_thread, /*include_turns*/ true)
            .await?;
        let path = thread
            .path
            .ok_or_else(|| color_eyre::eyre::eyre!("restored thread has no rollout"))?;
        let records = std::fs::read_to_string(path)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        // Passive completion audits have history-only start/end records. Real model
        // turns also persist a context and identify their originating root turn.
        let model_turn_records = records
            .into_iter()
            .filter(|record| {
                record["type"] == "turn_context"
                    || (record["type"] == "event_msg"
                        && record["payload"]["type"] == "task_started"
                        && !record["payload"]["root_turn_id"].is_null())
            })
            .collect::<Vec<_>>();
        assert_eq!(
            model_turn_records,
            Vec::<serde_json::Value>::new(),
            "idle thread {idle_thread} must not acquire a model turn"
        );
    }
    replacement.shutdown().await?;
    // Include all completed recovery work when asserting that only the selected
    // unfinished child sampled, even if a passive audit reached its idle parent.
    let request = response.single_request();
    if scenario == RecoveryScenario::UnfinishedTool {
        assert_eq!(
            request.function_call_output_text("pending-tool"),
            Some("aborted".to_string())
        );
        let path = completed
            .path
            .ok_or_else(|| color_eyre::eyre::eyre!("continued thread has no rollout"))?;
        let records = std::fs::read_to_string(path)?
            .lines()
            .map(serde_json::from_str::<serde_json::Value>)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let durable_outputs = records
            .into_iter()
            .filter(|record| {
                record["type"] == "response_item"
                    && record["payload"]["type"] == "function_call_output"
                    && record["payload"]["call_id"] == "pending-tool"
            })
            .collect::<Vec<_>>();
        assert_eq!(
            durable_outputs,
            Vec::<serde_json::Value>::new(),
            "the placeholder belongs only to model context"
        );
    }
    let user_messages = request
        .input()
        .into_iter()
        .filter(|item| item["role"] == "user")
        .collect::<Vec<_>>();
    assert_eq!(
        user_messages, original_user_messages,
        "recovery must preserve user input without appending a synthetic continuation"
    );
    let body = request.body_json();
    assert_eq!(body["text"]["format"], original_format);
    assert_eq!(body["service_tier"], original_tier);
    assert_eq!(body["reasoning"]["summary"], original_summary);
    Ok(())
}
