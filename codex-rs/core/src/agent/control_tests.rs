use super::*;
use crate::CodexThread;
use crate::StateDbHandle;
use crate::ThreadManager;
use crate::agent::agent_status_from_event;
use crate::agent::control::InitialTerminalObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::config::AgentRoleConfig;
use crate::config::Config;
use crate::config::ConfigBuilder;
use crate::context::ContextualUserFragment;
use crate::context::SubagentNotification;
use crate::init_state_db;
use crate::session::TurnInput;
use crate::session::turn_context::TurnContext;
use crate::state::TaskKind;
use crate::tasks::SessionTask;
use crate::tasks::SessionTaskContext;
use crate::tasks::SessionTaskResult;
use crate::thread_manager::StartThreadOptions;
use assert_matches::assert_matches;
use codex_extension_api::ExtensionDataInit;
use codex_extension_api::empty_extension_registry;
use codex_features::Feature;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::capabilities::CapabilityRootLocation;
use codex_protocol::capabilities::SelectedCapabilityRoot;
use codex_protocol::config_types::ApprovalsReviewer;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::PermissionProfile;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::CompactedItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::RolloutItem;
use codex_protocol::protocol::RolloutLine;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSettingsAppliedEvent;
use codex_protocol::protocol::ThreadSettingsSnapshot;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnAbortReason;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_thread_store::ArchiveThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use codex_thread_store::LocalThreadStore;
use codex_thread_store::LocalThreadStoreConfig;
use codex_thread_store::ThreadStore;
use codex_utils_path_uri::PathUri;
use core_test_support::responses::strip_response_item_ids;
use pretty_assertions::assert_eq;
use tempfile::TempDir;
use tokio::time::Duration;
use tokio::time::sleep;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use toml::Value as TomlValue;

struct BlockingTask {
    kind: TaskKind,
    turn_start_gate: Option<Arc<tokio::sync::Notify>>,
    turn_start_attempted: Option<Arc<tokio::sync::Notify>>,
}

impl SessionTask for BlockingTask {
    fn kind(&self) -> TaskKind {
        self.kind
    }

    fn span_name(&self) -> &'static str {
        "agent_control_test.blocking"
    }

    async fn run(
        self: Arc<Self>,
        session: Arc<SessionTaskContext>,
        ctx: Arc<TurnContext>,
        _input: Vec<TurnInput>,
        cancellation_token: CancellationToken,
    ) -> SessionTaskResult {
        match self.kind {
            TaskKind::Regular => {
                if let Some(turn_start_gate) = self.turn_start_gate.as_ref() {
                    tokio::select! {
                        () = turn_start_gate.notified() => {}
                        () = cancellation_token.cancelled() => return Ok(None),
                    }
                }
                session
                    .clone_session()
                    .send_event(
                        ctx.as_ref(),
                        EventMsg::TurnStarted(TurnStartedEvent {
                            turn_id: ctx.sub_id.clone(),
                            trace_id: None,
                            started_at: None,
                            model_context_window: None,
                            collaboration_mode_kind: Default::default(),
                        }),
                    )
                    .await;
                if let Some(turn_start_attempted) = self.turn_start_attempted.as_ref() {
                    turn_start_attempted.notify_one();
                }
            }
            TaskKind::Review | TaskKind::Compact => {}
        }
        cancellation_token.cancelled().await;
        Ok(None)
    }
}

async fn test_config_with_cli_overrides(
    mut cli_overrides: Vec<(String, TomlValue)>,
) -> (TempDir, Config) {
    let home = TempDir::new().expect("create temp dir");
    cli_overrides.push((
        "model".to_string(),
        TomlValue::String("gpt-5.5".to_string()),
    ));
    let config = ConfigBuilder::without_managed_config_for_tests()
        .codex_home(home.path().to_path_buf())
        .cli_overrides(cli_overrides)
        .build()
        .await
        .expect("load default test config");
    (home, config)
}

async fn test_config() -> (TempDir, Config) {
    test_config_with_cli_overrides(Vec::new()).await
}

fn text_input(text: &str) -> Vec<UserInput> {
    vec![UserInput::Text {
        text: text.to_string(),
        text_elements: Vec::new(),
    }]
}

fn assistant_message(text: &str, phase: Option<MessagePhase>) -> ResponseItem {
    ResponseItem::Message {
        id: None,
        role: "assistant".to_string(),
        content: vec![ContentItem::OutputText {
            text: text.to_string(),
        }],
        phase,
        internal_chat_message_metadata_passthrough: None,
    }
}

fn is_expected_completion_communication(entry: &(ThreadId, Op), expected: &(ThreadId, Op)) -> bool {
    let (
        entry_thread_id,
        Op::InterAgentCommunication {
            communication: entry_communication,
        },
    ) = entry
    else {
        return false;
    };
    let (
        expected_thread_id,
        Op::InterAgentCommunication {
            communication: expected_communication,
        },
    ) = expected
    else {
        return false;
    };
    let Some(id) = entry_communication.id.as_ref() else {
        return false;
    };
    let mut entry_communication = entry_communication.clone();
    entry_communication.id = None;
    entry_thread_id == expected_thread_id
        && is_sub_agent_completion_context_response_item_id(id)
        && &entry_communication == expected_communication
}

#[test]
fn register_session_root_skips_threads_with_explicit_parent() {
    let control = AgentControl::default();

    control.register_session_root(ThreadId::new(), Some(ThreadId::new()));

    assert_eq!(control.state.agent_id_for_path(&AgentPath::root()), None);
}

fn spawn_agent_call(call_id: &str) -> ResponseItem {
    ResponseItem::FunctionCall {
        id: None,
        name: "spawn_agent".to_string(),
        namespace: None,
        arguments: "{}".to_string(),
        call_id: call_id.to_string(),
        encrypted_function_args: None,
        internal_chat_message_metadata_passthrough: None,
    }
}

struct AgentControlHarness {
    _home: TempDir,
    config: Config,
    state_db: Option<StateDbHandle>,
    manager: ThreadManager,
    control: AgentControl,
}

impl AgentControlHarness {
    async fn new() -> Self {
        let (home, config) = test_config().await;
        Self::new_with_config(home, config).await
    }

    async fn new_with_config(home: TempDir, config: Config) -> Self {
        let state_db = init_state_db(&config).await;
        let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
            CodexAuth::from_api_key("dummy"),
            config.model_provider.clone(),
            config.codex_home.to_path_buf(),
            std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
            state_db.clone(),
        );
        let control = manager.agent_control();
        Self {
            _home: home,
            config,
            state_db,
            manager,
            control,
        }
    }

    async fn start_thread(&self) -> (ThreadId, Arc<CodexThread>) {
        let new_thread = self
            .manager
            .start_thread(StartThreadOptions::new(self.config.clone()))
            .await
            .expect("start thread");
        (new_thread.thread_id, new_thread.thread)
    }

    async fn start_paginated_thread(&self) -> (ThreadId, Arc<CodexThread>) {
        let new_thread = self
            .manager
            .start_thread(StartThreadOptions {
                history_mode: Some(ThreadHistoryMode::Paginated),
                environments: Some(Vec::new()),
                ..StartThreadOptions::new(self.config.clone())
            })
            .await
            .expect("start paginated thread");
        (new_thread.thread_id, new_thread.thread)
    }

    async fn start_thread_with_source(
        &self,
        config: Config,
        session_source: SessionSource,
    ) -> (ThreadId, Arc<CodexThread>) {
        let state = self
            .control
            .upgrade()
            .expect("thread manager should be live");
        let parent_thread_id = session_source.parent_thread_id();
        let new_thread = state
            .spawn_new_thread_with_source(
                config,
                self.control.clone(),
                session_source,
                /*history_mode*/ None,
                parent_thread_id,
                /*forked_from_thread_id*/ None,
                /*thread_source*/ Some(ThreadSource::Subagent),
                /*metrics_service_name*/ None,
                /*inherited_environments*/ None,
                /*inherited_exec_policy*/ None,
                /*environments*/ None,
            )
            .await
            .expect("start thread with source");
        (new_thread.thread_id, new_thread.thread)
    }

    async fn spawn_anonymous_child(
        &self,
        parent_thread_id: ThreadId,
        options: SpawnAgentOptions,
    ) -> ThreadId {
        self.control
            .spawn_agent_with_metadata(
                self.config.clone(),
                text_input("child task"),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                })),
                options,
            )
            .await
            .expect("child spawn should succeed")
            .thread_id
    }
}

async fn persisted_originator(thread: &CodexThread) -> String {
    thread.ensure_rollout_materialized().await;
    thread
        .flush_rollout()
        .await
        .expect("thread rollout should flush");
    let stored_thread = thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ true,
        )
        .await
        .expect("thread should be readable");
    let history = stored_thread.history.expect("history should be loaded");
    history
        .items
        .iter()
        .find_map(|item| match item {
            RolloutItem::SessionMeta(meta_line) => Some(meta_line.meta.originator.clone()),
            RolloutItem::ResponseItem(_)
            | RolloutItem::InterAgentCommunication(_)
            | RolloutItem::InterAgentCommunicationMetadata { .. }
            | RolloutItem::AgentResponseObservation(_)
            | RolloutItem::EventMsg(_)
            | RolloutItem::Compacted(_)
            | RolloutItem::WorldState(_)
            | RolloutItem::TurnContext(_) => None,
        })
        .expect("session metadata should be persisted")
}

fn has_subagent_notification(history_items: &[ResponseItem]) -> bool {
    history_items.iter().any(|item| {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "user" {
            return false;
        }
        content.iter().any(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                SubagentNotification::matches_text(text)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => false,
        })
    })
}

/// Returns true when any message item contains `needle` in a text span.
fn history_contains_text(history_items: &[ResponseItem], needle: &str) -> bool {
    history_items.iter().any(|item| {
        let ResponseItem::Message { content, .. } = item else {
            return false;
        };
        content.iter().any(|content_item| match content_item {
            ContentItem::InputText { text } | ContentItem::OutputText { text } => {
                text.contains(needle)
            }
            ContentItem::InputImage { .. } | ContentItem::InputAudio { .. } => false,
        })
    })
}

fn history_contains_assistant_inter_agent_communication(
    history_items: &[ResponseItem],
    expected: &InterAgentCommunication,
) -> bool {
    history_items.iter().any(|item| {
        let ResponseItem::Message { role, content, .. } = item else {
            return false;
        };
        if role != "assistant" {
            return false;
        }
        content.iter().any(|content_item| match content_item {
            ContentItem::OutputText { text } => {
                serde_json::from_str::<InterAgentCommunication>(text)
                    .ok()
                    .as_ref()
                    == Some(expected)
            }
            ContentItem::InputText { .. }
            | ContentItem::InputImage { .. }
            | ContentItem::InputAudio { .. } => false,
        })
    })
}

async fn wait_for_subagent_notification(parent_thread: &Arc<CodexThread>) -> bool {
    let wait = async {
        loop {
            let history_items = parent_thread
                .session
                .clone_history()
                .await
                .raw_items()
                .to_vec();
            if has_subagent_notification(&history_items) {
                return true;
            }
            sleep(Duration::from_millis(25)).await;
        }
    };
    // CI can take several seconds to schedule the detached completion watcher,
    // especially on slower Windows runners.
    timeout(Duration::from_secs(10), wait).await.is_ok()
}

async fn wait_for_subagent_completion_item(parent_thread: &Arc<CodexThread>) -> bool {
    timeout(Duration::from_secs(10), async {
        loop {
            let event = parent_thread.next_event().await.expect("parent event");
            if let EventMsg::ItemCompleted(event) = event.msg
                && let TurnItem::AgentMessage(item) = event.item
                && item.has_sub_agent_completion_identity()
            {
                break;
            }
        }
    })
    .await
    .is_ok()
}

async fn inject_user_message_without_turn(thread: &Arc<CodexThread>, message: &str) {
    let item = ResponseItem::Message {
        id: None,
        role: "user".to_string(),
        content: vec![ContentItem::InputText {
            text: message.to_string(),
        }],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    thread
        .session
        .inject_no_new_turn(vec![item], /*current_turn_context*/ None)
        .await;
}

async fn persist_thread_for_tree_resume(thread: &Arc<CodexThread>, message: &str) {
    let turn_context = thread.session.new_default_turn().await;
    thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: message.to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;
    thread.session.ensure_rollout_materialized().await;
    thread
        .session
        .flush_rollout()
        .await
        .expect("test thread rollout should flush");
}

async fn wait_for_live_thread_spawn_children(
    control: &AgentControl,
    parent_thread_id: ThreadId,
    expected_children: &[ThreadId],
) {
    let mut expected_children = expected_children.to_vec();
    expected_children.sort_by_key(std::string::ToString::to_string);

    timeout(Duration::from_secs(5), async {
        loop {
            let mut child_ids = control
                .open_thread_spawn_children(parent_thread_id)
                .await
                .expect("live child list should load")
                .into_iter()
                .map(|(thread_id, _)| thread_id)
                .collect::<Vec<_>>();
            child_ids.sort_by_key(std::string::ToString::to_string);
            if child_ids == expected_children {
                break;
            }
            sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("expected persisted child tree");
}

async fn assert_thread_not_loaded(manager: &ThreadManager, thread_id: ThreadId) {
    match manager.get_thread(thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(id) => assert_eq!(*id, thread_id),
            _ => panic!("expected ThreadNotFound, got {err:?}"),
        },
        Ok(_) => panic!("expected thread not to be loaded"),
    }
}

#[tokio::test]
async fn send_input_errors_when_manager_dropped() {
    let control = AgentControl::default();
    let err = control
        .send_input(
            ThreadId::new(),
            vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            /*parent_turn_id*/ None,
        )
        .await
        .expect_err("send_input should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn get_status_returns_not_found_without_manager() {
    let control = AgentControl::default();
    let got = control.get_status(ThreadId::new()).await;
    assert_eq!(got, AgentStatus::NotFound);
}

#[tokio::test]
async fn on_event_updates_status_from_task_started() {
    let status = agent_status_from_event(&EventMsg::TurnStarted(TurnStartedEvent {
        turn_id: "turn-1".to_string(),
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }));
    assert_eq!(status, Some(AgentStatus::Running));
}

#[tokio::test]
async fn on_event_updates_status_from_task_complete() {
    let status = agent_status_from_event(&EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-1".to_string(),
        started_at: None,
        last_agent_message: Some("done".to_string()),
        error: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }));
    let expected = AgentStatus::Completed(Some("done".to_string()));
    assert_eq!(status, Some(expected));
}

#[tokio::test]
async fn on_event_preserves_error_from_task_complete() {
    let status = agent_status_from_event(&EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: "turn-1".to_string(),
        started_at: None,
        last_agent_message: Some("partial".to_string()),
        error: Some(ErrorEvent {
            message: "boom".to_string(),
            codex_error_info: Some(CodexErrorInfo::BadRequest),
        }),
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }));

    assert_eq!(status, Some(AgentStatus::Errored("boom".to_string())));
}

#[tokio::test]
async fn on_event_updates_status_from_error() {
    let status = agent_status_from_event(&EventMsg::Error(ErrorEvent {
        message: "boom".to_string(),
        codex_error_info: None,
    }));

    let expected = AgentStatus::Errored("boom".to_string());
    assert_eq!(status, Some(expected));
}

#[tokio::test]
async fn on_event_ignores_non_terminal_error() {
    let status = agent_status_from_event(&EventMsg::Error(ErrorEvent {
        message: "turn is not steerable".to_string(),
        codex_error_info: Some(CodexErrorInfo::ActiveTurnNotSteerable {
            turn_kind: codex_protocol::protocol::NonSteerableTurnKind::Review,
        }),
    }));

    assert_eq!(status, None);
}

#[tokio::test]
async fn on_event_updates_status_from_turn_aborted() {
    let status = agent_status_from_event(&EventMsg::TurnAborted(TurnAbortedEvent {
        turn_id: Some("turn-1".to_string()),
        started_at: None,
        reason: TurnAbortReason::Interrupted,
        completed_at: None,
        duration_ms: None,
    }));

    let expected = AgentStatus::Interrupted;
    assert_eq!(status, Some(expected));
}

#[tokio::test]
async fn on_event_updates_status_from_shutdown_complete() {
    let status = agent_status_from_event(&EventMsg::ShutdownComplete);
    assert_eq!(status, Some(AgentStatus::Shutdown));
}

#[tokio::test]
async fn spawn_agent_errors_when_manager_dropped() {
    let control = AgentControl::default();
    let (_home, config) = test_config().await;
    let err = control
        .spawn_agent(config, text_input("hello"), /*session_source*/ None)
        .await
        .expect_err("spawn_agent should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn resume_agent_errors_when_manager_dropped() {
    let control = AgentControl::default();
    let (_home, config) = test_config().await;
    let err = control
        .resume_agent_from_rollout(
            config,
            ThreadId::new(),
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect_err("resume_agent should fail without a manager");
    assert_eq!(
        err.to_string(),
        "unsupported operation: thread manager dropped"
    );
}

#[tokio::test]
async fn send_input_errors_when_thread_missing() {
    let harness = AgentControlHarness::new().await;
    let thread_id = ThreadId::new();
    let err = harness
        .control
        .send_input(
            thread_id,
            vec![UserInput::Text {
                text: "hello".to_string(),
                text_elements: Vec::new(),
            }],
            /*parent_turn_id*/ None,
        )
        .await
        .expect_err("send_input should fail for missing thread");
    assert_matches!(
        err.details(),
        CodexErrorDetails::ThreadNotFound(id) if *id == thread_id
    );
}

#[tokio::test]
async fn rejected_send_input_does_not_remove_a_non_steerable_agent() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, thread) = harness.start_thread().await;
    let turn_context = thread
        .session
        .new_default_turn_with_sub_id("review-turn".to_string())
        .await;
    thread
        .session
        .spawn_task(
            turn_context,
            Vec::new(),
            BlockingTask {
                kind: TaskKind::Review,
                turn_start_gate: None,
                turn_start_attempted: None,
            },
        )
        .await;

    let error = harness
        .control
        .send_input(
            thread_id,
            text_input("cannot steer this review"),
            /*parent_turn_id*/ None,
        )
        .await
        .expect_err("review turn should reject steering");

    assert_matches!(
        error.details(),
        CodexErrorDetails::InvalidRequest(message)
            if message == "cannot steer a review turn"
    );
    let retained = harness
        .manager
        .get_thread(thread_id)
        .await
        .expect("healthy review agent should remain loaded");
    assert!(Arc::ptr_eq(&retained, &thread));
    thread
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
}

#[tokio::test]
async fn get_status_returns_not_found_for_missing_thread() {
    let harness = AgentControlHarness::new().await;
    let status = harness.control.get_status(ThreadId::new()).await;
    assert_eq!(status, AgentStatus::NotFound);
}

#[tokio::test]
async fn get_status_returns_pending_init_for_new_thread() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, _) = harness.start_thread().await;
    let status = harness.control.get_status(thread_id).await;
    assert_eq!(status, AgentStatus::PendingInit);
}

#[tokio::test]
async fn subscribe_status_errors_for_missing_thread() {
    let harness = AgentControlHarness::new().await;
    let thread_id = ThreadId::new();
    let err = harness
        .control
        .subscribe_status(thread_id)
        .await
        .expect_err("subscribe_status should fail for missing thread");
    assert_matches!(
        err.details(),
        CodexErrorDetails::ThreadNotFound(id) if *id == thread_id
    );
}

#[tokio::test]
async fn subscribe_status_updates_on_shutdown() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, thread) = harness.start_thread().await;
    let mut status_rx = harness
        .control
        .subscribe_status(thread_id)
        .await
        .expect("subscribe_status should succeed");
    assert_eq!(status_rx.borrow().clone(), AgentStatus::PendingInit);

    let _ = thread
        .submit(Op::Shutdown {})
        .await
        .expect("shutdown should submit");

    let _ = status_rx.changed().await;
    assert_eq!(status_rx.borrow().clone(), AgentStatus::Shutdown);
}

#[tokio::test]
async fn send_input_submits_user_message() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, _thread) = harness.start_thread().await;

    let submission_id = harness
        .control
        .send_input(
            thread_id,
            vec![UserInput::Text {
                text: "hello from tests".to_string(),
                text_elements: Vec::new(),
            }],
            /*parent_turn_id*/ None,
        )
        .await
        .expect("send_input should succeed");
    assert!(!submission_id.is_empty());
    let expected = (
        thread_id,
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "hello from tests".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));
}

#[tokio::test]
async fn send_inter_agent_communication_without_turn_queues_message_without_triggering_turn() {
    let harness = AgentControlHarness::new().await;
    let (thread_id, thread) = harness.start_thread().await;
    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "hello from tests".to_string(),
        /*trigger_turn*/ false,
    );

    let submission_id = harness
        .control
        .send_inter_agent_communication(
            thread_id,
            communication.clone(),
            AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new()),
            /*parent_turn_id*/ None,
        )
        .await
        .expect("send_inter_agent_communication should succeed");
    assert!(!submission_id.is_empty());

    let expected = (
        thread_id,
        Op::InterAgentCommunication {
            communication: communication.clone(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));

    timeout(Duration::from_secs(5), async {
        loop {
            if thread
                .session
                .input_queue
                .has_pending_input(&thread.session.active_turn)
                .await
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("inter-agent communication should stay pending");

    let history_items = thread.session.clone_history().await.raw_items().to_vec();
    assert!(!history_contains_assistant_inter_agent_communication(
        &history_items,
        &communication
    ));
}

#[tokio::test]
async fn ensure_v2_agent_loaded_reloads_registered_unloaded_agent() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, _parent_thread) = harness.start_paginated_thread().await;
    let agent_path = AgentPath::try_from("/root/worker").expect("agent path");
    let spawned_agent = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(agent_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("spawn_agent should succeed");
    let child_thread = harness
        .manager
        .get_thread(spawned_agent.thread_id)
        .await
        .expect("child thread should exist");
    child_thread
        .inject_response_items(vec![assistant_message(
            "child persisted",
            Some(MessagePhase::FinalAnswer),
        )])
        .await
        .expect("child rollout should persist with v2 metadata");
    child_thread
        .shutdown_and_wait()
        .await
        .expect("child thread should shut down");
    let stored_child = child_thread
        .read_thread(
            /*include_archived*/ true, /*include_history*/ false,
        )
        .await
        .expect("child metadata should be readable");
    assert_eq!(stored_child.history_mode, ThreadHistoryMode::Paginated);

    assert!(
        harness
            .manager
            .remove_thread(&spawned_agent.thread_id)
            .await
            .is_some()
    );
    match harness.manager.get_thread(spawned_agent.thread_id).await {
        Err(err) => match err.details() {
            CodexErrorDetails::ThreadNotFound(id) => assert_eq!(*id, spawned_agent.thread_id),
            _ => panic!("expected ThreadNotFound, got {err:?}"),
        },
        Ok(_) => panic!("expected thread to be removed"),
    }

    let canonical_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: Some(agent_path.clone()),
        agent_nickname: Some("canonical-worker".to_string()),
        agent_role: None,
    });
    harness
        .control
        .ensure_v2_agent_loaded_from_source(
            harness.config.clone(),
            spawned_agent.thread_id,
            canonical_source.clone(),
        )
        .await
        .expect("known v2 agent should reload");
    let reloaded_child = harness
        .manager
        .get_thread(spawned_agent.thread_id)
        .await
        .expect("reloaded child thread should exist");
    assert_eq!(reloaded_child.session_source, canonical_source);
    assert_eq!(
        harness
            .control
            .get_agent_metadata(spawned_agent.thread_id)
            .map(|metadata| (
                metadata.agent_path,
                metadata.agent_nickname,
                metadata.agent_role,
            )),
        Some((
            Some(agent_path.clone()),
            Some("canonical-worker".to_string()),
            None,
        ))
    );

    let communication = InterAgentCommunication::new(
        AgentPath::root(),
        agent_path,
        Vec::new(),
        "hello after reload".to_string(),
        /*trigger_turn*/ false,
    );
    harness
        .control
        .send_inter_agent_communication(
            spawned_agent.thread_id,
            communication.clone(),
            AgentCommunicationContext::new(AgentCommunicationKind::Message, ThreadId::new()),
            /*parent_turn_id*/ None,
        )
        .await
        .expect("send_inter_agent_communication should succeed after reload");
    let expected = (
        spawned_agent.thread_id,
        Op::InterAgentCommunication { communication },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));
}

#[tokio::test]
async fn resume_agent_from_rollout_does_not_reopen_v2_descendants() {
    let (home, mut config) = test_config().await;
    let _ = config.features.enable(Feature::MultiAgentV2);
    let _ = config.features.enable(Feature::Sqlite);
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let worker_path = AgentPath::root().join("worker").expect("worker path");
    let worker_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let reviewer_path = worker_path.join("reviewer").expect("reviewer path");
    let reviewer_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello reviewer"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(reviewer_path.clone()),
                agent_nickname: None,
                agent_role: Some("reviewer".to_string()),
            })),
        )
        .await
        .expect("reviewer spawn should succeed");
    let sibling_thread_id = harness
        .spawn_anonymous_child(parent_thread_id, SpawnAgentOptions::default())
        .await;

    let worker_thread = harness
        .manager
        .get_thread(worker_thread_id)
        .await
        .expect("worker thread should exist");
    let reviewer_thread = harness
        .manager
        .get_thread(reviewer_thread_id)
        .await
        .expect("reviewer thread should exist");
    let sibling_thread = harness
        .manager
        .get_thread(sibling_thread_id)
        .await
        .expect("sibling thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&worker_thread, "worker persisted").await;
    persist_thread_for_tree_resume(&reviewer_thread, "reviewer persisted").await;
    persist_thread_for_tree_resume(&sibling_thread, "sibling persisted").await;
    wait_for_live_thread_spawn_children(
        &harness.control,
        parent_thread_id,
        &[worker_thread_id, sibling_thread_id],
    )
    .await;
    wait_for_live_thread_spawn_children(&harness.control, worker_thread_id, &[reviewer_thread_id])
        .await;

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        harness.config.model_provider.clone(),
        harness.config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        harness.state_db.clone(),
    );
    let resumed_control = resumed_manager.agent_control();
    let resumed_parent_thread_id = resumed_control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("v2 root resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        resumed_control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_thread_not_loaded(&resumed_manager, worker_thread_id).await;
    assert_thread_not_loaded(&resumed_manager, reviewer_thread_id).await;
    assert_thread_not_loaded(&resumed_manager, sibling_thread_id).await;
    resumed_control
        .restore_v2_agent_metadata(&harness.config, parent_thread_id)
        .await;
    for thread_id in [worker_thread_id, sibling_thread_id] {
        assert!(resumed_control.ensure_agent_known(thread_id).is_ok());
    }

    resumed_control
        .close_agent(worker_thread_id)
        .await
        .expect("closing a restored sibling should succeed");

    let closed_worker = resumed_control.ensure_agent_known(worker_thread_id);
    let surviving_sibling = resumed_control.ensure_agent_known(sibling_thread_id);
    assert!(closed_worker.is_err());
    assert!(surviving_sibling.is_ok());
    assert_thread_not_loaded(&resumed_manager, sibling_thread_id).await;
}

#[tokio::test]
async fn encrypted_inter_agent_communication_clears_existing_last_task_message() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _) = harness.start_thread().await;
    let agent_path = AgentPath::try_from("/root/worker").expect("agent path");
    let spawned_agent = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("old plaintext task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(agent_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("spawn_agent should succeed");
    assert_eq!(
        harness
            .control
            .state
            .agent_metadata_for_thread(spawned_agent.thread_id)
            .and_then(|metadata| metadata.last_task_message),
        Some("old plaintext task".to_string())
    );

    let communication = InterAgentCommunication::new_encrypted(
        AgentPath::root(),
        agent_path,
        Vec::new(),
        "encrypted-task".to_string(),
        /*trigger_turn*/ true,
    );
    harness
        .control
        .send_inter_agent_communication(
            spawned_agent.thread_id,
            communication,
            AgentCommunicationContext::new(AgentCommunicationKind::Followup, ThreadId::new()),
            /*parent_turn_id*/ None,
        )
        .await
        .expect("send_inter_agent_communication should succeed");

    assert_eq!(
        harness
            .control
            .state
            .agent_metadata_for_thread(spawned_agent.thread_id)
            .and_then(|metadata| metadata.last_task_message),
        None
    );
}

#[tokio::test]
async fn encrypted_inter_agent_communication_uses_audit_and_ignores_results_for_last_task() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _) = harness.start_thread().await;
    let agent_path = AgentPath::try_from("/root/worker").expect("agent path");
    let spawned_agent = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("old plaintext task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(agent_path.clone()),
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("spawn_agent should succeed");

    let mut communication = InterAgentCommunication::new_encrypted(
        AgentPath::root(),
        agent_path.clone(),
        Vec::new(),
        "encrypted-task".to_string(),
        /*trigger_turn*/ true,
    );
    communication.content = "audit-visible task".to_string();
    harness
        .control
        .send_inter_agent_communication(
            spawned_agent.thread_id,
            communication,
            AgentCommunicationContext::new(AgentCommunicationKind::Followup, parent_thread_id),
            /*parent_turn_id*/ None,
        )
        .await
        .expect("send_inter_agent_communication should succeed");

    assert_eq!(
        harness
            .control
            .state
            .agent_metadata_for_thread(spawned_agent.thread_id)
            .and_then(|metadata| metadata.last_task_message),
        Some("audit-visible task".to_string())
    );

    harness
        .control
        .send_inter_agent_communication(
            spawned_agent.thread_id,
            InterAgentCommunication::new(
                AgentPath::root(),
                agent_path,
                Vec::new(),
                "final result".to_string(),
                /*trigger_turn*/ false,
            ),
            AgentCommunicationContext::new(AgentCommunicationKind::Result, parent_thread_id),
            /*parent_turn_id*/ None,
        )
        .await
        .expect("result communication should succeed");

    assert_eq!(
        harness
            .control
            .state
            .agent_metadata_for_thread(spawned_agent.thread_id)
            .and_then(|metadata| metadata.last_task_message),
        Some("audit-visible task".to_string())
    );
}

#[tokio::test]
async fn spawn_agent_creates_thread_and_sends_prompt() {
    let harness = AgentControlHarness::new().await;
    let thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("spawned"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let _thread = harness
        .manager
        .get_thread(thread_id)
        .await
        .expect("thread should be registered");
    let expected = (
        thread_id,
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "spawned".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));
}

#[tokio::test]
async fn ephemeral_spawn_does_not_persist_agent_graph_edge() {
    let (home, mut config) = test_config().await;
    config.ephemeral = true;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("spawned"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
        )
        .await
        .expect("ephemeral agent spawn should succeed");

    let persisted_children = harness
        .state_db
        .as_ref()
        .expect("manager should retain state db")
        .list_thread_spawn_children(parent_thread_id)
        .await
        .expect("persisted child list should load");
    assert_eq!(persisted_children, Vec::<ThreadId>::new());
    assert!(
        harness.manager.get_thread(child_thread_id).await.is_ok(),
        "ephemeral child should remain live"
    );
}

#[tokio::test]
async fn spawn_agent_fork_from_paginated_parent_uses_model_context_prefix() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    inject_user_message_without_turn(&parent_thread, "paginated parent context").await;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-paginated".to_string();
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[spawn_agent_call(&parent_spawn_call_id)],
        )
        .await;
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "id-less inherited context".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
                thread_id: parent_thread_id,
                turn_id: "parent-turn".to_string(),
                item: TurnItem::UserMessage(UserMessageItem {
                    id: "parent-user".to_string(),
                    client_id: None,
                    content: Vec::new(),
                }),
                started_at_ms: Some(0),
                completed_at_ms: 1,
            })),
            RolloutItem::EventMsg(EventMsg::ThreadSettingsApplied(
                ThreadSettingsAppliedEvent {
                    thread_settings: ThreadSettingsSnapshot {
                        model: "parent-only-model".to_string(),
                        model_provider_id: "parent-only-provider".to_string(),
                        service_tier: None,
                        approval_policy: AskForApproval::Never,
                        approvals_reviewer: ApprovalsReviewer::User,
                        permission_profile: PermissionProfile::workspace_write(),
                        active_permission_profile: None,
                        cwd: harness.config.cwd.clone(),
                        reasoning_effort: None,
                        reasoning_summary: None,
                        personality: None,
                        collaboration_mode: CollaborationMode {
                            mode: ModeKind::Default,
                            settings: Settings {
                                model: "parent-only-model".to_string(),
                                reasoning_effort: None,
                                developer_instructions: None,
                            },
                        },
                    },
                },
            )),
        ])
        .await;

    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    assert!(
        history_contains_text(
            child_thread.session.clone_history().await.raw_items(),
            "paginated parent context",
        ),
        "bounded parent context should remain model-visible to the child"
    );
    child_thread.ensure_rollout_materialized().await;
    child_thread
        .flush_rollout()
        .await
        .expect("child rollout should flush");
    let rollout_path = child_thread
        .rollout_path()
        .expect("child rollout should exist");
    let lines = std::fs::read_to_string(&rollout_path)
        .expect("read child rollout")
        .lines()
        .map(|line| serde_json::from_str::<RolloutLine>(line).expect("parse rollout line"))
        .collect::<Vec<_>>();
    let RolloutItem::SessionMeta(meta_line) = &lines[0].item else {
        panic!("child rollout should start with session metadata");
    };
    assert_eq!(meta_line.meta.history_mode, ThreadHistoryMode::Paginated);
    assert_eq!(meta_line.meta.parent_thread_id, Some(parent_thread_id));
    assert_eq!(meta_line.meta.forked_from_id, Some(parent_thread_id));
    let prefix_end = usize::try_from(
        meta_line
            .meta
            .subagent_history_start_ordinal
            .expect("paginated child should mark its local history boundary"),
    )
    .expect("history boundary should fit in usize");
    let copied_prefix = &lines[1..prefix_end];
    let copied_idless_context = copied_prefix
        .iter()
        .find_map(|line| match &line.item {
            RolloutItem::ResponseItem(response_item)
                if serde_json::to_string(response_item)
                    .expect("serialize response item")
                    .contains("id-less inherited context") =>
            {
                Some(response_item)
            }
            _ => None,
        })
        .expect("copied prefix should contain inherited response item");
    assert!(
        copied_idless_context.id().is_some_and(|id| !id.is_empty()),
        "copied model context should receive response item ids before persistence"
    );
    let copied_parent_context_count = lines
        .iter()
        .filter(|line| {
            serde_json::to_string(&line.item)
                .expect("serialize rollout item")
                .contains("paginated parent context")
        })
        .count();
    assert_eq!(
        copied_parent_context_count, 1,
        "copied model context should be persisted once"
    );
    assert!(
        !copied_prefix.iter().any(|line| {
            matches!(
                &line.item,
                RolloutItem::EventMsg(
                    EventMsg::ItemCompleted(_) | EventMsg::ThreadSettingsApplied(_)
                )
            )
        }),
        "copied non-structural presentation and metadata records should not enter the child rollout"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_without_fork_from_paginated_parent_stays_fresh_and_paginated() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    inject_user_message_without_turn(&parent_thread, "parent-only context").await;

    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    assert!(
        !history_contains_text(
            child_thread.session.clone_history().await.raw_items(),
            "parent-only context",
        ),
        "fork_turns=none should not copy parent context"
    );
    child_thread.ensure_rollout_materialized().await;
    child_thread
        .flush_rollout()
        .await
        .expect("child rollout should flush");
    let meta = codex_rollout::read_session_meta_line(
        &child_thread
            .rollout_path()
            .expect("child rollout should exist"),
    )
    .await
    .expect("read child session metadata");
    assert_eq!(meta.meta.history_mode, ThreadHistoryMode::Paginated);
    assert_eq!(meta.meta.subagent_history_start_ordinal, None);

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_numeric_fork_from_compacted_paginated_parent_clamps_to_provable_turns() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    let parent_spawn_call_id = "spawn-call-paginated-numeric".to_string();
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(vec![ResponseItem::Message {
                    id: None,
                    role: "user".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "compacted summary".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                }]),
                compaction_summary_tokens: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                ..Default::default()
            }),
            RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "recent parent turn".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            RolloutItem::ResponseItem(spawn_agent_call(&parent_spawn_call_id)),
        ])
        .await;

    let clamped_child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(2)),
                ..Default::default()
            },
        )
        .await;
    let clamped_child_thread = harness
        .manager
        .get_thread(clamped_child_thread_id)
        .await
        .expect("clamped child thread should be registered");
    let clamped_history = clamped_child_thread.session.clone_history().await;
    assert!(
        history_contains_text(clamped_history.raw_items(), "recent parent turn"),
        "clamped numeric fork should keep the provable recent turn"
    );
    assert!(
        !history_contains_text(clamped_history.raw_items(), "compacted summary"),
        "clamped numeric fork should not expand into compacted parent context"
    );

    let _ = harness
        .control
        .shutdown_live_agent(clamped_child_thread_id)
        .await
        .expect("clamped child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_can_fork_parent_thread_history_with_sanitized_items() {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.developer_instructions = Some("Parent developer instructions.".to_string());
    parent_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Parent root guidance.".to_string());
    parent_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Parent subagent guidance.".to_string());
    let mut child_config = harness.config.clone();
    let _ = child_config.features.enable(Feature::MultiAgentV2);
    child_config.developer_instructions = Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.subagent_developer_instructions =
        Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Child root guidance.".to_string());
    child_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Child subagent guidance.".to_string());
    let new_thread = harness
        .manager
        .start_thread(StartThreadOptions::new(parent_config.clone()))
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    inject_user_message_without_turn(&parent_thread, "parent seed context").await;
    let expected_parent_seed = parent_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .first()
        .cloned()
        .expect("parent seed should be recorded");
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-history".to_string();
    let trigger_message = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "parent trigger message".to_string(),
        /*trigger_turn*/ true,
    );
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "Parent root guidance.".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "Parent subagent guidance.".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![
                        ContentItem::InputText {
                            text: "Developer context before.\nParent developer instructions.\nDeveloper context after."
                                .to_string(),
                        },
                        ContentItem::InputText {
                            text: "Preserved developer context.".to_string(),
                        },
                    ],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                assistant_message("parent commentary", Some(MessagePhase::Commentary)),
                assistant_message("parent final answer", Some(MessagePhase::FinalAnswer)),
                assistant_message("parent unknown phase", /*phase*/ None),
                ResponseItem::Reasoning {
                    id: Some(ResponseItemId::with_suffix("rs", "parent-reasoning")),
                    summary: Vec::new(),
                    content: None,
                    encrypted_content: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                trigger_message.to_response_input_item().into(),
                spawn_agent_call(&parent_spawn_call_id),
            ],
        )
        .await;
    let parent_reference_context_item = turn_context.to_turn_context_item();
    parent_thread
        .session
        .persist_rollout_items(&[RolloutItem::TurnContext(
            parent_reference_context_item.clone(),
        )])
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");
    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should succeed")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    assert_ne!(child_thread_id, parent_thread_id);
    assert_eq!(
        child_thread.config_snapshot().await.history_mode,
        ThreadHistoryMode::Legacy
    );
    let history = child_thread.session.clone_history().await;
    let mut expected_final_answer =
        assistant_message("parent final answer", Some(MessagePhase::FinalAnswer));
    expected_final_answer.set_turn_id_if_missing(&turn_context.sub_id);
    let mut expected_developer_message = ResponseItem::Message {
        id: None,
        role: "developer".to_string(),
        content: vec![
            ContentItem::InputText {
                text: "Developer context before.\nChild developer instructions.\nDeveloper context after."
                    .to_string(),
            },
            ContentItem::InputText {
                text: "Preserved developer context.".to_string(),
            },
        ],
        phase: None,
        internal_chat_message_metadata_passthrough: None,
    };
    expected_developer_message.set_turn_id_if_missing(&turn_context.sub_id);
    let expected_history = [
        expected_parent_seed,
        expected_developer_message,
        expected_final_answer,
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "Child subagent guidance.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    assert_eq!(
        strip_response_item_ids(history.raw_items()),
        strip_response_item_ids(&expected_history),
        "full-history forked child history should replace parent usage hints with the child subagent hint while filtering non-final assistant/tool chatter"
    );
    assert_eq!(
        serde_json::to_value(child_thread.session.reference_context_item().await)
            .expect("serialize child reference context item"),
        serde_json::to_value(Some(parent_reference_context_item))
            .expect("serialize expected reference context item"),
        "full-history forked child should preserve the parent diff baseline"
    );

    let mut no_hint_child_config = harness.config.clone();
    let _ = no_hint_child_config.features.enable(Feature::MultiAgentV2);
    no_hint_child_config.developer_instructions = Some(String::new());
    no_hint_child_config
        .multi_agent_v2
        .subagent_developer_instructions = Some(String::new());
    no_hint_child_config.multi_agent_v2.subagent_usage_hint_text = None;
    let no_hint_child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            no_hint_child_config,
            text_input("child task without hints"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should honor an empty subagent usage hint")
        .thread_id;
    let no_hint_child_thread = harness
        .manager
        .get_thread(no_hint_child_thread_id)
        .await
        .expect("no-hint child thread should be registered");
    let no_hint_history = no_hint_child_thread.session.clone_history().await;
    assert!(
        !history_contains_text(no_hint_history.raw_items(), "Child subagent guidance."),
        "full-history forked child should not add empty subagent guidance"
    );
    assert!(
        !history_contains_text(
            no_hint_history.raw_items(),
            "Parent developer instructions."
        ),
        "empty child developer instructions should remove parent developer instructions"
    );
    assert!(
        history_contains_text(
            no_hint_history.raw_items(),
            "Developer context before.\n\nDeveloper context after."
        ),
        "empty child developer instructions should preserve surrounding developer context"
    );
    assert!(
        history_contains_text(no_hint_history.raw_items(), "Preserved developer context."),
        "empty child developer instructions should preserve unrelated developer fragments"
    );

    let expected = (
        child_thread_id,
        Op::UserInput {
            items: vec![UserInput::Text {
                text: "child task".to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings: Default::default(),
        },
    );
    let captured = harness
        .manager
        .captured_ops()
        .into_iter()
        .find(|entry| *entry == expected);
    assert_eq!(captured, Some(expected));

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = harness
        .control
        .shutdown_live_agent(no_hint_child_thread_id)
        .await
        .expect("no-hint child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_strips_parent_usage_hints_from_compacted_history() {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.developer_instructions = Some("Parent developer instructions.".to_string());
    parent_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Parent root guidance.".to_string());
    parent_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Parent subagent guidance.".to_string());
    let mut child_config = harness.config.clone();
    let _ = child_config.features.enable(Feature::MultiAgentV2);
    child_config.developer_instructions = Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.subagent_developer_instructions =
        Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Child root guidance.".to_string());
    child_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Child subagent guidance.".to_string());
    let new_thread = harness
        .manager
        .start_thread(StartThreadOptions::new(parent_config))
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-compacted-usage-hints".to_string();
    let replacement_history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "compacted parent summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "Parent root guidance.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![
                ContentItem::InputText {
                    text: "Compacted context before.\nParent developer instructions.\nCompacted context after."
                        .to_string(),
                },
                ContentItem::InputText {
                    text: "Preserved compacted developer context.".to_string(),
                },
            ],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(replacement_history),
                compaction_summary_tokens: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                ..Default::default()
            }),
            RolloutItem::TurnContext(turn_context.to_turn_context_item()),
            RolloutItem::ResponseItem(spawn_agent_call(&parent_spawn_call_id)),
        ])
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should sanitize compacted usage hints")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "compacted parent summary"),
        "forked child history should retain compacted non-hint content"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent root guidance."),
        "forked child history should strip stale parent hints from compacted replacement history"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent developer instructions."),
        "forked child history should replace parent instructions in compacted replacement history"
    );
    assert!(
        history_contains_text(
            history.raw_items(),
            "Compacted context before.\nChild developer instructions.\nCompacted context after."
        ),
        "forked child history should replace compacted parent instructions without removing surrounding context"
    );
    assert!(
        history_contains_text(
            history.raw_items(),
            "Preserved compacted developer context."
        ),
        "forked child history should preserve unrelated compacted developer fragments"
    );
    assert!(
        history_contains_text(history.raw_items(), "Child subagent guidance."),
        "full-history forked child should add the child subagent hint after compacted-history sanitization"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

/// Full-history forks must restore child instructions when compaction discarded
/// the only matching parent instruction fragment from effective history.
#[tokio::test]
async fn spawn_agent_full_fork_restores_instructions_after_compaction_discards_parent_fragment() {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.developer_instructions = Some("Parent developer instructions.".to_string());
    let mut child_config = parent_config.clone();
    child_config.developer_instructions = Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.subagent_developer_instructions =
        Some("Child developer instructions.".to_string());

    let new_thread = harness
        .manager
        .start_thread(StartThreadOptions::new(parent_config))
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-compacted-stale-instructions".to_string();
    let replacement_history = vec![
        ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "compacted parent summary".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
        ResponseItem::Message {
            id: None,
            role: "developer".to_string(),
            content: vec![ContentItem::InputText {
                text: "Preserved compacted developer context.".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ];

    // Preserve the parent's live baseline while its durable checkpoint omits the
    // developer fragment that appeared in obsolete pre-compaction history.
    parent_thread
        .session
        .replace_history(
            replacement_history.clone(),
            Some(turn_context.to_turn_context_item()),
        )
        .await;
    parent_thread
        .session
        .persist_rollout_items(&[
            RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "Parent developer instructions.".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }),
            RolloutItem::Compacted(CompactedItem {
                message: String::new(),
                replacement_history: Some(replacement_history),
                compaction_summary_tokens: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                replacement_history_media_sanitized_prefix_len: None,
                replacement_history_media_repair: false,
            }),
            RolloutItem::TurnContext(turn_context.to_turn_context_item()),
            RolloutItem::ResponseItem(spawn_agent_call(&parent_spawn_call_id)),
        ])
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should preserve effective compacted instructions")
        .thread_id;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(
            history.raw_items(),
            "Preserved compacted developer context."
        ),
        "full-history fork should preserve unrelated compacted developer fragments"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent developer instructions."),
        "full-history fork should not restore stale pre-compaction parent instructions"
    );
    assert!(
        history_contains_text(history.raw_items(), "Child developer instructions."),
        "full-history fork should append child instructions absent from effective compacted history"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

/// A legacy compaction clears the child's baseline, so its first turn must
/// rebuild configured developer instructions exactly once.
#[tokio::test]
async fn spawn_agent_full_fork_legacy_compaction_rebuilds_child_instructions_once() {
    for (case, parent_developer_instructions) in [
        ("without parent instructions", None),
        (
            "with parent instructions",
            Some("Parent developer instructions."),
        ),
    ] {
        let harness = AgentControlHarness::new().await;
        let mut parent_config = harness.config.clone();
        let _ = parent_config.features.enable(Feature::MultiAgentV2);
        parent_config.developer_instructions = parent_developer_instructions.map(str::to_string);
        let mut child_config = parent_config.clone();
        child_config.developer_instructions = Some("Child developer instructions.".to_string());
        child_config.multi_agent_v2.subagent_developer_instructions =
            Some("Child developer instructions.".to_string());

        let new_thread = harness
            .manager
            .start_thread(StartThreadOptions::new(parent_config))
            .await
            .expect("start parent thread");
        let parent_thread_id = new_thread.thread_id;
        let parent_thread = new_thread.thread;
        let turn_context = parent_thread.session.new_default_turn().await;
        let parent_spawn_call_id = match parent_developer_instructions {
            Some(_) => "spawn-call-legacy-compact-with-parent",
            None => "spawn-call-legacy-compact-without-parent",
        };
        let parent_user_message = ResponseItem::Message {
            id: None,
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "parent task before legacy compaction".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        };

        // A live parent can reestablish its baseline after resuming a rollout
        // whose older compaction record cannot restore that baseline to a child.
        parent_thread
            .session
            .replace_history(
                vec![parent_user_message.clone()],
                Some(turn_context.to_turn_context_item()),
            )
            .await;
        let mut rollout_items = vec![
            RolloutItem::ResponseItem(parent_user_message),
            RolloutItem::Compacted(CompactedItem {
                message: "legacy compacted summary".to_string(),
                replacement_history: None,
                compaction_summary_tokens: None,
                window_number: None,
                first_window_id: None,
                previous_window_id: None,
                window_id: None,
                replacement_history_media_sanitized_prefix_len: None,
                replacement_history_media_repair: false,
            }),
        ];
        if let Some(instructions) = parent_developer_instructions {
            rollout_items.push(RolloutItem::ResponseItem(ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: instructions.to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }));
        }
        rollout_items.push(RolloutItem::TurnContext(
            turn_context.to_turn_context_item(),
        ));
        rollout_items.push(RolloutItem::ResponseItem(spawn_agent_call(
            parent_spawn_call_id,
        )));
        parent_thread
            .session
            .persist_rollout_items(&rollout_items)
            .await;
        parent_thread.session.ensure_rollout_materialized().await;
        parent_thread
            .session
            .flush_rollout()
            .await
            .expect("parent rollout should flush");

        let child_thread_id = harness
            .control
            .spawn_agent_with_metadata(
                child_config,
                text_input("child task"),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 1,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: None,
                })),
                SpawnAgentOptions {
                    fork_parent_spawn_call_id: Some(parent_spawn_call_id.to_string()),
                    fork_mode: Some(SpawnAgentForkMode::FullHistory),
                    ..Default::default()
                },
            )
            .await
            .expect("forked spawn should preserve legacy compacted history")
            .thread_id;
        let child_thread = harness
            .manager
            .get_thread(child_thread_id)
            .await
            .expect("child thread should be registered");
        while child_thread
            .session
            .reference_context_item()
            .await
            .is_none()
        {
            tokio::task::yield_now().await;
        }
        let history = child_thread.session.clone_history().await;
        let mut instruction_count = 0;
        for item in history.raw_items() {
            let ResponseItem::Message { role, content, .. } = item else {
                continue;
            };
            if role != "developer" {
                continue;
            }
            for content_item in content {
                if let ContentItem::InputText { text } = content_item
                    && text == "Child developer instructions."
                {
                    instruction_count += 1;
                }
            }
        }
        assert_eq!(
            instruction_count, 1,
            "{case}: canonical context reconstruction must not duplicate child developer instructions"
        );

        let _ = harness
            .control
            .shutdown_live_agent(child_thread_id)
            .await
            .expect("child shutdown should submit");
        let _ = parent_thread
            .submit(Op::Shutdown {})
            .await
            .expect("parent shutdown should submit");
    }
}

#[tokio::test]
async fn spawn_agent_fork_flushes_parent_rollout_before_loading_history() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-unflushed".to_string();
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[
                assistant_message("unflushed final answer", Some(MessagePhase::FinalAnswer)),
                spawn_agent_call(&parent_spawn_call_id),
            ],
        )
        .await;

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::FullHistory),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should flush parent rollout before loading history")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "unflushed final answer"),
        "forked child history should include unflushed assistant final answers after flushing the parent rollout"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_last_n_turns_keeps_only_recent_turns() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    inject_user_message_without_turn(&parent_thread, "old parent context").await;
    let queued_communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "queued message".to_string(),
        /*trigger_turn*/ false,
    );
    let queued_turn_context = parent_thread.session.new_default_turn().await;
    parent_thread
        .session
        .record_conversation_items(
            queued_turn_context.as_ref(),
            &[queued_communication.to_response_input_item().into()],
        )
        .await;

    let triggered_communication = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::try_from("/root/worker").expect("agent path"),
        Vec::new(),
        "triggered context".to_string(),
        /*trigger_turn*/ true,
    );
    let triggered_turn_context = parent_thread.session.new_default_turn().await;
    parent_thread
        .session
        .record_conversation_items(
            triggered_turn_context.as_ref(),
            &[triggered_communication.to_response_input_item().into()],
        )
        .await;
    inject_user_message_without_turn(&parent_thread, "current parent task").await;
    let spawn_turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-last-n".to_string();
    parent_thread
        .session
        .record_conversation_items(
            spawn_turn_context.as_ref(),
            &[spawn_agent_call(&parent_spawn_call_id)],
        )
        .await;
    parent_thread
        .session
        .persist_rollout_items(&[RolloutItem::TurnContext(
            spawn_turn_context.to_turn_context_item(),
        )])
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id.clone()),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(2)),
                ..Default::default()
            },
        )
        .await
        .expect("forked spawn should keep only the last two turns")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;

    assert!(
        !history_contains_text(history.raw_items(), "old parent context"),
        "forked child history should drop parent context outside the requested last-N turn window"
    );
    assert!(
        !history_contains_text(history.raw_items(), "queued message"),
        "forked child history should drop queued inter-agent messages outside the requested last-N turn window"
    );
    assert!(
        !history_contains_text(history.raw_items(), "triggered context"),
        "forked child history should filter assistant inter-agent messages even when they fall inside the requested last-N turn window"
    );
    assert!(
        history_contains_text(history.raw_items(), "current parent task"),
        "forked child history should keep the parent user message from the requested last-N turn window"
    );
    assert!(
        child_thread
            .session
            .reference_context_item()
            .await
            .is_none(),
        "last-N forked child should rebuild context after truncating the cached prefix"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_last_n_turns_drops_parent_startup_prefix_when_under_limit() {
    let harness = AgentControlHarness::new().await;
    let selected_capability_roots = vec![SelectedCapabilityRoot {
        id: "demo@1".to_string(),
        location: CapabilityRootLocation::Environment {
            environment_id: "build".to_string(),
            path: PathUri::parse("file:///plugins/demo").expect("plugin root URI"),
        },
    }];
    let mut thread_extension_init = ExtensionDataInit::new();
    thread_extension_init.insert(selected_capability_roots.clone());
    let parent = harness
        .manager
        .start_thread(StartThreadOptions {
            environments: Some(Vec::new()),
            thread_extension_init,
            ..StartThreadOptions::new(harness.config.clone())
        })
        .await
        .expect("start parent thread");
    let parent_thread_id = parent.thread_id;
    let parent_thread = parent.thread;
    let startup_turn_context = parent_thread.session.new_default_turn().await;
    parent_thread
        .session
        .record_conversation_items(
            startup_turn_context.as_ref(),
            &[ResponseItem::Message {
                id: None,
                role: "developer".to_string(),
                content: vec![ContentItem::InputText {
                    text: "parent startup developer context".to_string(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;
    inject_user_message_without_turn(&parent_thread, "current parent task").await;
    let spawn_turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-last-n-under-limit".to_string();
    parent_thread
        .session
        .record_conversation_items(
            spawn_turn_context.as_ref(),
            &[spawn_agent_call(&parent_spawn_call_id)],
        )
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(2)),
                ..Default::default()
            },
        )
        .await
        .expect("bounded forked spawn should drop startup prefix")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "current parent task"),
        "bounded fork should retain the requested recent parent turn"
    );
    assert!(
        !history_contains_text(history.raw_items(), "parent startup developer context"),
        "bounded fork should drop parent startup context even when fewer turns exist than requested"
    );
    assert_eq!(
        &child_thread.session.services.selected_capability_roots,
        &selected_capability_roots
    );
    assert!(
        child_thread
            .session
            .reference_context_item()
            .await
            .is_none(),
        "bounded forked child should still rebuild context after truncating the cached prefix"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_fork_last_n_turns_strips_parent_usage_hints() {
    let harness = AgentControlHarness::new().await;
    let mut parent_config = harness.config.clone();
    let _ = parent_config.features.enable(Feature::MultiAgentV2);
    parent_config.developer_instructions = Some("Parent developer instructions.".to_string());
    parent_config.multi_agent_v2.root_agent_usage_hint_text =
        Some("Parent root guidance.".to_string());
    let mut child_config = harness.config.clone();
    let _ = child_config.features.enable(Feature::MultiAgentV2);
    child_config.developer_instructions = Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.subagent_developer_instructions =
        Some("Child developer instructions.".to_string());
    child_config.multi_agent_v2.subagent_usage_hint_text =
        Some("Child subagent guidance.".to_string());
    let new_thread = harness
        .manager
        .start_thread(StartThreadOptions::new(parent_config))
        .await
        .expect("start parent thread");
    let parent_thread_id = new_thread.thread_id;
    let parent_thread = new_thread.thread;
    inject_user_message_without_turn(&parent_thread, "parent task").await;
    let turn_context = parent_thread.session.new_default_turn().await;
    let parent_spawn_call_id = "spawn-call-last-n-usage-hints".to_string();
    parent_thread
        .session
        .record_conversation_items(
            turn_context.as_ref(),
            &[
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![ContentItem::InputText {
                        text: "Parent root guidance.".to_string(),
                    }],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                ResponseItem::Message {
                    id: None,
                    role: "developer".to_string(),
                    content: vec![
                        ContentItem::InputText {
                            text: "Parent developer instructions.".to_string(),
                        },
                        ContentItem::InputText {
                            text: "Preserved bounded developer context.".to_string(),
                        },
                    ],
                    phase: None,
                    internal_chat_message_metadata_passthrough: None,
                },
                spawn_agent_call(&parent_spawn_call_id),
            ],
        )
        .await;
    parent_thread.session.ensure_rollout_materialized().await;
    parent_thread
        .session
        .flush_rollout()
        .await
        .expect("parent rollout should flush");

    let child_thread_id = harness
        .control
        .spawn_agent_with_metadata(
            child_config,
            text_input("child task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some(parent_spawn_call_id),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(2)),
                ..Default::default()
            },
        )
        .await
        .expect("bounded forked spawn should sanitize parent usage hints")
        .thread_id;

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let history = child_thread.session.clone_history().await;
    assert!(
        history_contains_text(history.raw_items(), "parent task"),
        "bounded fork should retain the requested recent parent turn"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent root guidance."),
        "bounded fork should strip stale parent root hints before the child rebuilds startup context"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Parent developer instructions."),
        "bounded fork should remove parent instructions before the child rebuilds startup context"
    );
    assert!(
        !history_contains_text(history.raw_items(), "Child developer instructions."),
        "bounded fork should not inject child instructions before its canonical context rebuild"
    );
    assert!(
        history_contains_text(history.raw_items(), "Preserved bounded developer context."),
        "bounded fork should preserve unrelated developer fragments"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn spawn_agent_respects_legacy_max_threads_alias() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_threads".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let _ = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("start thread");

    let first_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");

    let err = control
        .spawn_agent(
            config,
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect_err("spawn_agent should respect max threads");
    let CodexErrorDetails::AgentLimitReached {
        max_threads: seen_max_threads,
    } = err.details()
    else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*seen_max_threads, max_threads);

    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn spawn_agent_releases_slot_after_shutdown() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let first_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");

    let second_agent_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed after shutdown");
    let _ = control
        .shutdown_live_agent(second_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn spawn_agent_limit_shared_across_clones() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();
    let cloned = control.clone();

    let first_agent_id = cloned
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");

    let err = control
        .spawn_agent(
            config,
            text_input("hello again"),
            /*session_source*/ None,
        )
        .await
        .expect_err("spawn_agent should respect shared guard");
    let CodexErrorDetails::AgentLimitReached { max_threads } = err.details() else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*max_threads, 1);

    let _ = control
        .shutdown_live_agent(first_agent_id)
        .await
        .expect("shutdown agent");
}

#[tokio::test]
async fn resume_agent_respects_max_threads_limit() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let resumable_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed");
    let _ = control
        .shutdown_live_agent(resumable_id)
        .await
        .expect("shutdown resumable thread");

    let active_id = control
        .spawn_agent(
            config.clone(),
            text_input("occupy"),
            /*session_source*/ None,
        )
        .await
        .expect("spawn_agent should succeed for active slot");

    let err = control
        .resume_agent_from_rollout(
            config,
            resumable_id,
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect_err("resume should respect max threads");
    let CodexErrorDetails::AgentLimitReached {
        max_threads: seen_max_threads,
    } = err.details()
    else {
        panic!("expected AgentLimitReached");
    };
    assert_eq!(*seen_max_threads, max_threads);

    let _ = control
        .shutdown_live_agent(active_id)
        .await
        .expect("shutdown active thread");
}

#[tokio::test]
async fn resume_agent_releases_slot_after_resume_failure() {
    let max_threads = 1usize;
    let (_home, config) = test_config_with_cli_overrides(vec![(
        "agents.max_concurrent_threads_per_session".to_string(),
        TomlValue::Integer(max_threads as i64),
    )])
    .await;
    let manager = ThreadManager::with_models_provider_and_home_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
    );
    let control = manager.agent_control();

    let _ = control
        .resume_agent_from_rollout(
            config.clone(),
            ThreadId::new(),
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect_err("resume should fail for missing rollout path");

    let resumed_id = control
        .spawn_agent(config, text_input("hello"), /*session_source*/ None)
        .await
        .expect("spawn should succeed after failed resume");
    let _ = control
        .shutdown_live_agent(resumed_id)
        .await
        .expect("shutdown resumed thread");
}

#[tokio::test]
async fn failed_concurrent_resume_setup_preserves_the_adopted_runtime() {
    let harness = AgentControlHarness::new().await;
    let missing_parent_thread_id = ThreadId::new();
    let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: missing_parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let (thread_id, running_thread) = harness
        .start_thread_with_source(harness.config.clone(), session_source.clone())
        .await;
    persist_thread_for_tree_resume(&running_thread, "running before concurrent resume").await;

    let error = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            thread_id,
            session_source,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect_err("adopted resume setup should fail for a missing parent");

    assert!(matches!(
        error.details(),
        CodexErrorDetails::ThreadNotFound(thread_id) if *thread_id == missing_parent_thread_id
    ));
    let retained_thread = harness
        .manager
        .get_thread(thread_id)
        .await
        .expect("failed adopting resume must preserve the running thread");
    assert!(Arc::ptr_eq(&retained_thread, &running_thread));
    assert!(
        harness.control.get_agent_metadata(thread_id).is_none(),
        "metadata registered only by the failed adopting attempt must roll back"
    );
}

#[tokio::test]
async fn spawn_child_completion_notifies_parent_history() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let _ = child_thread
        .submit(Op::Shutdown {})
        .await
        .expect("child shutdown should submit");

    assert_eq!(wait_for_subagent_notification(&parent_thread).await, true);
}

#[tokio::test]
async fn multi_agent_v2_completion_ignores_dead_direct_parent() {
    let harness = AgentControlHarness::new().await;
    let mut config = harness.config.clone();
    let _ = config.features.enable(Feature::MultiAgentV2);
    let root = harness
        .manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("root thread should start");
    let root_thread_id = root.thread_id;
    let root_thread = root.thread;
    let worker_path = AgentPath::root().join("worker_a").expect("worker path");
    let worker_thread_id = harness
        .control
        .spawn_agent(
            config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let tester_path = worker_path.join("tester").expect("tester path");
    let tester_thread_id = harness
        .control
        .spawn_agent(
            config,
            text_input("hello tester"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(tester_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("tester spawn should succeed");
    harness
        .control
        .shutdown_live_agent(worker_thread_id)
        .await
        .expect("worker shutdown should succeed");

    let tester_thread = harness
        .manager
        .get_thread(tester_thread_id)
        .await
        .expect("tester thread should exist");
    let tester_turn = tester_thread.session.new_default_turn().await;
    tester_thread
        .session
        .send_event(
            tester_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: tester_turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    sleep(Duration::from_millis(100)).await;

    assert!(
        !harness
            .manager
            .captured_ops()
            .into_iter()
            .any(|(thread_id, op)| {
                thread_id == worker_thread_id
                    && matches!(
                        op,
                        Op::InterAgentCommunication { communication }
                            if communication.author == tester_path
                                && communication.recipient == worker_path
                                && communication.content == "done"
                    )
            })
    );

    let root_history_items = root_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .to_vec();
    assert!(!history_contains_assistant_inter_agent_communication(
        &root_history_items,
        &InterAgentCommunication::new(
            tester_path,
            AgentPath::root(),
            Vec::new(),
            "done".to_string(),
            /*trigger_turn*/ true,
        )
    ));
    assert!(!has_subagent_notification(&root_history_items));
}

#[tokio::test]
async fn v1_observer_of_v2_shutdown_queues_notification_for_direct_parent() {
    let harness = AgentControlHarness::new().await;
    let (_root_thread_id, root_thread) = harness.start_thread().await;
    let (worker_thread_id, worker_thread) = harness.start_thread().await;
    let mut tester_config = harness.config.clone();
    let _ = tester_config.features.enable(Feature::MultiAgentV2);
    let worker_path = AgentPath::root().join("worker_a").expect("worker path");
    let tester_path = worker_path.join("tester").expect("tester path");
    let tester_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: worker_thread_id,
        depth: 2,
        agent_path: Some(tester_path.clone()),
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (tester_thread_id, tester_thread) = harness
        .start_thread_with_source(tester_config.clone(), tester_source.clone())
        .await;
    harness
        .control
        .maybe_start_completion_watcher(
            &tester_thread,
            Some(tester_source),
            tester_path.to_string(),
            Some(tester_path.clone()),
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");
    let tester_turn = tester_thread.session.new_default_turn().await;
    tester_thread
        .session
        .send_event(
            tester_turn.as_ref(),
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: tester_turn.sub_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        )
        .await;
    tester_thread
        .session
        .send_event(tester_turn.as_ref(), EventMsg::ShutdownComplete)
        .await;

    let expected_message = crate::session_prefix::format_subagent_notification_message(
        tester_path.as_str(),
        tester_thread_id,
        &AgentStatus::Shutdown,
    );
    assert!(wait_for_subagent_notification(&worker_thread).await);
    let worker_history_items = worker_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .to_vec();
    assert!(history_contains_text(
        &worker_history_items,
        &expected_message
    ));

    let root_history_items = root_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .to_vec();
    assert!(!history_contains_text(
        &root_history_items,
        &expected_message
    ));
    assert!(!has_subagent_notification(&root_history_items));
}

#[tokio::test]
async fn multi_agent_v2_target_wake_cleanup_rechecks_idle_v1_observer() {
    struct IdleRecorder(tokio::sync::mpsc::UnboundedSender<()>);

    impl codex_extension_api::ThreadLifecycleContributor<Config> for IdleRecorder {
        fn on_thread_idle<'a>(
            &'a self,
            _input: codex_extension_api::ThreadIdleInput<'a>,
        ) -> codex_extension_api::ExtensionFuture<'a, ()> {
            Box::pin(async move {
                let _ = self.0.send(());
            })
        }
    }

    let (_home, mut config) = test_config().await;
    config
        .features
        .enable(Feature::Collab)
        .expect("test config should enable multi-agent v1");
    let mut child_config = config.clone();
    child_config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("test child config should enable multi-agent v2");
    let (idle_tx, mut idle_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(IdleRecorder(idle_tx)));
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        Arc::clone(&auth_manager),
        crate::thread_manager::build_models_manager(&config, auth_manager),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Arc::new(extensions.build()),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        crate::thread_manager::thread_store_from_config(&config, /*state_db*/ None),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let state = control.upgrade().expect("thread manager should be live");
    let parent = state
        .spawn_new_thread_with_source(
            config,
            control.clone(),
            SessionSource::Exec,
            /*history_mode*/ None,
            /*parent_thread_id*/ None,
            /*forked_from_thread_id*/ None,
            /*thread_source*/ None,
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("start v1 observer");
    control.register_session_root(parent.thread_id, /*current_parent_thread_id*/ None);
    let parent_presentation = parent.thread.session.presentation_id();
    let child_path = AgentPath::root().join("worker").expect("child path");
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: parent.thread_id,
        depth: 1,
        agent_path: Some(child_path.clone()),
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let child = state
        .spawn_new_thread_with_source(
            child_config,
            control.clone(),
            child_source.clone(),
            /*history_mode*/ None,
            Some(parent.thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("start v2 target");
    let child_presentation = child.thread.session.presentation_id();
    control
        .maybe_start_completion_watcher(
            &child.thread,
            Some(child_source),
            child_path.to_string(),
            Some(child_path.clone()),
            ResponseObservationPolicy::from_parts(
                /*commentary*/ false,
                FinalResponseObservation::Wake,
            ),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start v1 observation of v2 target");

    let child_turn = child.thread.session.new_default_turn().await;
    child
        .thread
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: child_turn.sub_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        )
        .await;
    timeout(Duration::from_secs(5), async {
        while !control.has_bound_final_response_wake(parent_presentation) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("v2 target turn should bind the final wake");
    assert!(
        idle_rx.try_recv().is_err(),
        "bound wake should keep the v1 observer's idle lifecycle deferred"
    );

    child
        .thread
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                started_at: None,
                last_agent_message: Some("v2 target done".to_string()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;

    let expected_message = crate::session_prefix::format_subagent_notification_message(
        child_path.as_str(),
        child.thread_id,
        &AgentStatus::Completed(Some("v2 target done".to_string())),
    );
    let communication = timeout(Duration::from_secs(5), async {
        loop {
            if let Some(communication) =
                manager
                    .captured_ops()
                    .into_iter()
                    .find_map(|(thread_id, op)| match op {
                        Op::InterAgentCommunication { communication }
                            if thread_id == parent.thread_id =>
                        {
                            Some(communication)
                        }
                        _ => None,
                    })
            {
                break communication;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("V1 observer should receive a waking notification from the V2 target");
    assert!(communication.trigger_turn);
    assert_eq!(communication.content, expected_message);
    timeout(Duration::from_secs(5), async {
        while control.has_bound_final_response_wake(parent_presentation) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("delivered cross-version wake should clear its observation");
    parent
        .thread
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
    timeout(Duration::from_secs(5), idle_rx.recv())
        .await
        .expect("idle lifecycle should resume after the cross-version wake turn ends")
        .expect("idle lifecycle recorder should remain available");
    assert!(!control.has_bound_final_response_wake(parent_presentation));
    assert!(
        control
            .response_observation_relationship_snapshot(parent_presentation, child_presentation)
            .is_some(),
        "watcher relationship should remain available for later target turns"
    );

    let shutdown = manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert!(shutdown.timed_out.is_empty());
}

#[tokio::test]
async fn v1_observer_of_v2_raw_error_queues_notification_for_direct_parent() {
    let harness = AgentControlHarness::new().await;
    let (worker_thread_id, worker_thread) = harness.start_thread().await;
    let worker_path = AgentPath::root().join("worker_a").expect("worker path");
    let tester_path = worker_path.join("tester").expect("tester path");
    let tester_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: worker_thread_id,
        depth: 2,
        agent_path: Some(tester_path.clone()),
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let mut tester_config = harness.config.clone();
    let _ = tester_config.features.enable(Feature::MultiAgentV2);
    let (tester_thread_id, tester_thread) = harness
        .start_thread_with_source(tester_config, tester_source.clone())
        .await;
    harness
        .control
        .maybe_start_completion_watcher(
            &tester_thread,
            Some(tester_source),
            tester_path.to_string(),
            Some(tester_path.clone()),
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");

    let error = "invalid thread settings";
    let turn_id = uuid::Uuid::now_v7().to_string();
    tester_thread
        .session
        .send_event_raw(Event {
            id: turn_id.clone(),
            msg: EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: turn_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        })
        .await;
    tester_thread
        .session
        .send_event_raw(Event {
            id: turn_id,
            msg: EventMsg::Error(ErrorEvent {
                message: error.to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;

    let expected_message = crate::session_prefix::format_subagent_notification_message(
        tester_path.as_str(),
        tester_thread_id,
        &AgentStatus::Errored(error.to_string()),
    );
    assert!(wait_for_subagent_notification(&worker_thread).await);
    let worker_history_items = worker_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .to_vec();
    assert!(history_contains_text(
        &worker_history_items,
        &expected_message
    ));
}

#[tokio::test]
async fn completion_watcher_notifies_parent_when_child_is_missing() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source.clone())
        .await;
    harness
        .control
        .maybe_start_completion_watcher(
            &child_thread,
            /*session_source*/ Some(child_source),
            child_thread_id.to_string(),
            /*child_agent_path*/ None,
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");
    harness.manager.remove_thread(&child_thread_id).await;

    assert_eq!(wait_for_subagent_notification(&parent_thread).await, true);

    let history_items = parent_thread
        .session
        .clone_history()
        .await
        .raw_items()
        .to_vec();
    assert_eq!(
        history_contains_text(
            &history_items,
            &format!("\"agent_path\":\"{child_thread_id}\"")
        ),
        true
    );
    assert_eq!(
        history_contains_text(&history_items, "\"status\":\"not_found\""),
        true
    );
}

#[tokio::test]
async fn removing_child_notifies_parent_while_another_thread_arc_is_retained() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source.clone())
        .await;
    harness
        .control
        .maybe_start_completion_watcher(
            &child_thread,
            Some(child_source),
            child_thread_id.to_string(),
            /*child_agent_path*/ None,
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");
    let retained_thread = Arc::clone(&child_thread);

    let removed_thread = harness
        .manager
        .remove_thread(&child_thread_id)
        .await
        .expect("child thread should be loaded");

    assert!(Arc::ptr_eq(&removed_thread, &retained_thread));
    assert_eq!(retained_thread.agent_status().await, AgentStatus::NotFound);
    assert!(wait_for_subagent_notification(&parent_thread).await);
    assert!(wait_for_subagent_completion_item(&parent_thread).await);
}

#[tokio::test]
async fn removing_running_child_delivers_not_found_for_bound_response_observation() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source)
        .await;
    let child_turn_id = "active-child-turn".to_string();
    let child_turn = child_thread
        .session
        .new_default_turn_with_sub_id(child_turn_id.clone())
        .await;
    child_thread
        .session
        .spawn_task(
            child_turn,
            Vec::new(),
            BlockingTask {
                kind: TaskKind::Regular,
                turn_start_gate: None,
                turn_start_attempted: None,
            },
        )
        .await;
    timeout(Duration::from_secs(5), async {
        loop {
            let (snapshot, subscription) = child_thread.session.subscribe_agent_responses();
            drop(subscription);
            if snapshot.active_turn_id.as_deref() == Some(child_turn_id.as_str()) {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("blocking child should publish its active turn");

    let parent_presentation = parent_thread.session.presentation_id();
    let child_presentation = child_thread.session.presentation_id();
    harness
        .control
        .send_input_observing_response(
            child_thread_id,
            text_input("continue the active task"),
            /*parent_turn_id*/ None,
            parent_presentation,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("active child should accept observed follow-up input");
    assert!(
        harness
            .control
            .response_observation_snapshots(parent_presentation, child_presentation)
            .iter()
            .any(|observation| {
                observation.target_turn_id.as_deref() == Some(child_turn_id.as_str())
                    && observation.final_delivery
                        == codex_protocol::protocol::AgentResponseFinalDelivery::Passive
            }),
        "send_input observation should be bound to the active child turn"
    );

    let wait = harness
        .control
        .register_targeted_wait_agent_presentation(parent_presentation, &[child_thread_id]);
    let removed_thread = harness
        .manager
        .remove_thread(&child_thread_id)
        .await
        .expect("child thread should be loaded");
    let presentation_commit = wait.freeze_for_children([child_thread_id]);
    assert_eq!(
        presentation_commit
            .claimed_target_turns()
            .iter()
            .map(|target| (target.child, target.turn_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(child_presentation, child_turn_id.as_str())],
    );
    drop(presentation_commit);

    assert!(Arc::ptr_eq(&removed_thread, &child_thread));
    assert_eq!(child_thread.agent_status().await, AgentStatus::NotFound);
    assert!(wait_for_subagent_notification(&parent_thread).await);
    assert!(wait_for_subagent_completion_item(&parent_thread).await);
    assert!(
        history_contains_text(
            parent_thread.session.clone_history().await.raw_items(),
            "\"status\":\"not_found\"",
        ),
        "the released wait claim should allow automatic NotFound delivery"
    );
    child_thread
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
}

#[tokio::test]
async fn removing_child_before_turn_started_suppresses_late_running_publication() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source)
        .await;
    child_thread
        .session
        .send_event_raw(Event {
            id: "previous-turn".to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "previous-turn".to_string(),
                last_agent_message: Some("previous result".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        })
        .await;
    assert_eq!(
        child_thread.agent_status().await,
        AgentStatus::Completed(Some("previous result".to_string()))
    );
    let child_turn_id = "admitted-before-turn-start".to_string();
    let child_turn = child_thread
        .session
        .new_default_turn_with_sub_id(child_turn_id.clone())
        .await;
    let turn_start_gate = Arc::new(tokio::sync::Notify::new());
    let turn_start_attempted = Arc::new(tokio::sync::Notify::new());
    child_thread
        .session
        .spawn_task(
            child_turn,
            Vec::new(),
            BlockingTask {
                kind: TaskKind::Regular,
                turn_start_gate: Some(Arc::clone(&turn_start_gate)),
                turn_start_attempted: Some(Arc::clone(&turn_start_attempted)),
            },
        )
        .await;

    let parent_presentation = parent_thread.session.presentation_id();
    let child_presentation = child_thread.session.presentation_id();
    harness
        .control
        .send_input_observing_response(
            child_thread_id,
            text_input("observe the admitted turn"),
            /*parent_turn_id*/ None,
            parent_presentation,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("admitted child turn should accept observed follow-up input");
    let (admitted_snapshot, admitted_subscription) =
        child_thread.session.subscribe_agent_responses();
    drop(admitted_subscription);
    assert_eq!(
        (
            admitted_snapshot.active_turn_id,
            admitted_snapshot.last_terminal,
            admitted_snapshot.status,
        ),
        (Some(child_turn_id.clone()), None, AgentStatus::Running,),
        "turn admission should publish canonical identity and Running atomically"
    );

    let wait = harness
        .control
        .register_targeted_wait_agent_presentation(parent_presentation, &[child_thread_id]);
    let removed_thread = harness
        .manager
        .remove_thread(&child_thread_id)
        .await
        .expect("child thread should be loaded");
    let presentation_commit = wait.freeze_for_children([child_thread_id]);
    assert_eq!(
        presentation_commit
            .claimed_target_turns()
            .iter()
            .map(|target| (target.child, target.turn_id.as_str()))
            .collect::<Vec<_>>(),
        vec![(child_presentation, child_turn_id.as_str())],
    );
    drop(presentation_commit);

    turn_start_gate.notify_one();
    timeout(Duration::from_secs(5), turn_start_attempted.notified())
        .await
        .expect("removed child should attempt its delayed TurnStarted");
    let (removed_snapshot, removed_subscription) = child_thread.session.subscribe_agent_responses();
    drop(removed_subscription);
    assert_eq!(
        (
            removed_snapshot.active_turn_id,
            removed_snapshot.last_terminal,
            removed_snapshot.status,
            child_thread.agent_status().await,
        ),
        (
            None,
            Some((child_turn_id.clone(), AgentStatus::NotFound)),
            AgentStatus::NotFound,
            AgentStatus::NotFound,
        ),
        "late TurnStarted must not replace the removal terminal"
    );
    assert!(Arc::ptr_eq(&removed_thread, &child_thread));
    assert!(wait_for_subagent_notification(&parent_thread).await);
    assert!(wait_for_subagent_completion_item(&parent_thread).await);
    child_thread
        .session
        .abort_all_tasks(TurnAbortReason::Interrupted)
        .await;
}

#[tokio::test]
async fn removing_child_publishes_not_found_to_an_already_active_wait() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source.clone())
        .await;
    harness
        .control
        .maybe_start_completion_watcher(
            &child_thread,
            /*session_source*/ Some(child_source),
            child_thread_id.to_string(),
            /*child_agent_path*/ None,
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");
    let wait = harness.control.register_targeted_wait_agent_presentation(
        parent_thread.session.presentation_id(),
        &[child_thread_id],
    );
    let (_initial_status, mut terminal_status) = child_thread.session.subscribe_terminal_status();

    let removed_thread = harness
        .manager
        .remove_thread(&child_thread_id)
        .await
        .expect("child thread should be loaded");

    assert!(Arc::ptr_eq(&removed_thread, &child_thread));
    assert_eq!(
        terminal_status.recv().await.map(|event| event.status),
        Some(AgentStatus::NotFound)
    );
    let commit = wait.freeze_for_children([child_thread_id]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(child_thread_id, AgentStatus::NotFound)])
    );
    commit.commit();
    assert_eq!(child_thread.agent_status().await, AgentStatus::NotFound);
}

#[tokio::test]
async fn completion_watcher_starts_once_for_the_same_session() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source.clone())
        .await;
    for _ in 0..2 {
        harness
            .control
            .maybe_start_completion_watcher(
                &child_thread,
                Some(child_source.clone()),
                child_thread_id.to_string(),
                /*child_agent_path*/ None,
                ResponseObservationPolicy::default(),
                InitialTerminalObservation::FutureTurnsOnly,
            )
            .await
            .expect("start completion watcher");
    }

    child_thread
        .session
        .send_event_raw(Event {
            id: uuid::Uuid::now_v7().to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: "child failed".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;

    assert!(wait_for_subagent_notification(&parent_thread).await);
    assert!(wait_for_subagent_completion_item(&parent_thread).await);
    assert!(
        timeout(
            Duration::from_millis(100),
            wait_for_subagent_completion_item(&parent_thread)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn cancelled_wait_releases_v1_completion_to_background_watcher() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source.clone())
        .await;
    harness
        .control
        .maybe_start_completion_watcher(
            &child_thread,
            Some(child_source),
            child_thread_id.to_string(),
            /*child_agent_path*/ None,
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");
    let wait = harness.control.register_targeted_wait_agent_presentation(
        parent_thread.session.presentation_id(),
        &[child_thread_id],
    );

    child_thread
        .session
        .send_event_raw(Event {
            id: uuid::Uuid::now_v7().to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: "child failed".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
    drop(wait);

    assert!(wait_for_subagent_notification(&parent_thread).await);
    assert!(wait_for_subagent_completion_item(&parent_thread).await);
}

#[tokio::test]
async fn completed_wait_suppresses_v1_background_watcher() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source.clone())
        .await;
    harness
        .control
        .maybe_start_completion_watcher(
            &child_thread,
            Some(child_source),
            child_thread_id.to_string(),
            /*child_agent_path*/ None,
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");
    let wait = harness.control.register_targeted_wait_agent_presentation(
        parent_thread.session.presentation_id(),
        &[child_thread_id],
    );

    child_thread
        .session
        .send_event_raw(Event {
            id: uuid::Uuid::now_v7().to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: "child failed".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
    wait.freeze_for_children([child_thread_id]).commit();

    assert!(
        timeout(
            Duration::from_millis(100),
            wait_for_subagent_notification(&parent_thread)
        )
        .await
        .is_err()
    );
    assert!(
        timeout(
            Duration::from_millis(100),
            wait_for_subagent_completion_item(&parent_thread)
        )
        .await
        .is_err()
    );
}

#[tokio::test]
async fn late_wait_does_not_suppress_v1_background_watcher() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let (child_thread_id, child_thread) = harness
        .start_thread_with_source(harness.config.clone(), child_source.clone())
        .await;
    harness
        .control
        .maybe_start_completion_watcher(
            &child_thread,
            Some(child_source),
            child_thread_id.to_string(),
            /*child_agent_path*/ None,
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("start completion watcher");
    child_thread
        .session
        .send_event_raw(Event {
            id: uuid::Uuid::now_v7().to_string(),
            msg: EventMsg::Error(ErrorEvent {
                message: "child failed".to_string(),
                codex_error_info: Some(CodexErrorInfo::BadRequest),
            }),
        })
        .await;
    child_thread
        .session
        .send_event_raw(Event {
            id: uuid::Uuid::now_v7().to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "child-turn".to_string(),
                started_at: None,
                last_agent_message: Some("incorrect success".to_string()),
                error: Some(ErrorEvent {
                    message: "child failed".to_string(),
                    codex_error_info: Some(CodexErrorInfo::BadRequest),
                }),
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        })
        .await;
    assert_eq!(
        child_thread.agent_status().await,
        AgentStatus::Errored("child failed".to_string())
    );
    let wait = harness.control.register_targeted_wait_agent_presentation(
        parent_thread.session.presentation_id(),
        &[child_thread_id],
    );
    let commit = wait.freeze_for_children([child_thread_id]);
    commit.commit();

    assert!(wait_for_subagent_notification(&parent_thread).await);
    assert!(wait_for_subagent_completion_item(&parent_thread).await);
}

#[tokio::test]
async fn spawn_thread_subagent_gets_random_nickname_in_session_source() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let snapshot = child_thread.config_snapshot().await;

    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: seen_parent_thread_id,
        depth,
        agent_nickname,
        agent_role,
        ..
    }) = snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(seen_parent_thread_id, parent_thread_id);
    assert_eq!(depth, 1);
    assert!(agent_nickname.is_some());
    assert_eq!(agent_role, Some("explorer".to_string()));
}

#[tokio::test]
async fn spawn_thread_subagents_persist_parent_originator_across_new_and_truncated_fork() {
    let harness = AgentControlHarness::new().await;
    let parent = harness
        .manager
        .start_thread(StartThreadOptions {
            metrics_service_name: Some("codex_work_desktop".to_string()),
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(harness.config.clone())
        })
        .await
        .expect("parent thread should start");
    let parent_originator = persisted_originator(&parent.thread).await;
    assert_eq!(parent_originator, "codex_work_desktop");

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let child_originator = persisted_originator(&child_thread).await;
    assert_eq!(child_originator, parent_originator);

    let child = harness
        .control
        .spawn_agent_with_metadata(
            harness.config.clone(),
            text_input("hello forked child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: parent.thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
            SpawnAgentOptions {
                fork_parent_spawn_call_id: Some("spawn-call-last-n".to_string()),
                fork_mode: Some(SpawnAgentForkMode::LastNTurns(1)),
                ..Default::default()
            },
        )
        .await
        .expect("forked child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child.thread_id)
        .await
        .expect("child thread should be registered");
    let child_originator = persisted_originator(&child_thread).await;
    assert_eq!(child_originator, parent_originator);
}

#[tokio::test]
async fn spawn_thread_subagent_uses_role_specific_nickname_candidates() {
    let mut harness = AgentControlHarness::new().await;
    harness.config.agent_roles.insert(
        "researcher".to_string(),
        AgentRoleConfig {
            description: Some("Research role".to_string()),
            config_file: None,
            nickname_candidates: Some(vec!["Atlas".to_string()]),
        },
    );
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("researcher".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should be registered");
    let snapshot = child_thread.config_snapshot().await;

    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn { agent_nickname, .. }) =
        snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(agent_nickname, Some("Atlas".to_string()));
}

#[tokio::test]
async fn resume_thread_subagent_restores_stored_metadata() {
    let (home, config) = test_config().await;
    let thread_store = Arc::new(InMemoryThreadStore::default());
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth_manager.clone(),
        crate::thread_manager::build_models_manager(&config, auth_manager),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        thread_store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let agent_path = AgentPath::from_string("/root/explorer".to_string())
        .expect("test agent path should be valid");

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(agent_path.clone()),
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    child_thread.session.ensure_rollout_materialized().await;
    child_thread
        .session
        .flush_rollout()
        .await
        .expect("flush child rollout");
    let mut status_rx = harness
        .control
        .subscribe_status(child_thread_id)
        .await
        .expect("status subscription should succeed");
    if matches!(status_rx.borrow().clone(), AgentStatus::PendingInit) {
        timeout(Duration::from_secs(5), async {
            loop {
                status_rx
                    .changed()
                    .await
                    .expect("child status should advance past pending init");
                if !matches!(status_rx.borrow().clone(), AgentStatus::PendingInit) {
                    break;
                }
            }
        })
        .await
        .expect("child should initialize before shutdown");
    }
    let original_snapshot = child_thread.config_snapshot().await;
    let original_nickname = original_snapshot
        .session_source
        .get_nickname()
        .expect("spawned sub-agent should have a nickname");
    timeout(Duration::from_secs(5), async {
        loop {
            if let Ok(stored_thread) = thread_store
                .read_thread(ReadThreadParams {
                    thread_id: child_thread_id,
                    include_archived: true,
                    include_history: false,
                })
                .await
                && stored_thread.agent_nickname.is_some()
                && stored_thread.agent_role.as_deref() == Some("explorer")
                && stored_thread.agent_path.as_deref() == Some(agent_path.as_str())
            {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("child thread metadata should be persisted to sqlite before shutdown");

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should submit");

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("resume should succeed");
    assert_eq!(resumed_thread_id, child_thread_id);

    let resumed_snapshot = harness
        .manager
        .get_thread(resumed_thread_id)
        .await
        .expect("resumed child thread should exist")
        .config_snapshot()
        .await;
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: resumed_parent_thread_id,
        depth: resumed_depth,
        agent_path: resumed_agent_path,
        agent_nickname: resumed_nickname,
        agent_role: resumed_role,
        ..
    }) = resumed_snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_eq!(resumed_depth, 1);
    assert_eq!(resumed_agent_path, Some(agent_path));
    assert_eq!(resumed_nickname, Some(original_nickname));
    assert_eq!(resumed_role, Some("explorer".to_string()));

    let _ = harness
        .control
        .shutdown_live_agent(resumed_thread_id)
        .await
        .expect("resumed child shutdown should submit");
}

#[tokio::test]
async fn resume_agent_from_rollout_reads_archived_rollout_path() {
    let harness = AgentControlHarness::new().await;
    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello"),
            /*session_source*/ None,
        )
        .await
        .expect("child spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    persist_thread_for_tree_resume(&child_thread, "persist before archiving").await;
    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should succeed");
    let store = LocalThreadStore::new(
        LocalThreadStoreConfig::from_config(&harness.config),
        harness.state_db.clone(),
    );
    store
        .archive_thread(ArchiveThreadParams {
            thread_id: child_thread_id,
        })
        .await
        .expect("child thread should archive");

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("resume should find archived rollout");
    assert_eq!(resumed_thread_id, child_thread_id);

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("resumed child shutdown should succeed");
}

#[tokio::test]
async fn resume_agent_from_paginated_rollout_loads_model_context() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_paginated_thread().await;
    let child_thread_id = harness
        .spawn_anonymous_child(
            parent_thread_id,
            SpawnAgentOptions {
                parent_thread_id: Some(parent_thread_id),
                ..Default::default()
            },
        )
        .await;
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    assert_eq!(
        child_thread.config_snapshot().await.history_mode,
        ThreadHistoryMode::Paginated
    );
    persist_thread_for_tree_resume(&child_thread, "persist before resume").await;
    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("child shutdown should succeed");

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("resume should load paginated model context");
    assert_eq!(resumed_thread_id, child_thread_id);
    let resumed_thread = harness
        .manager
        .get_thread(resumed_thread_id)
        .await
        .expect("resumed child thread should exist");
    assert!(
        history_contains_text(
            resumed_thread.session.clone_history().await.raw_items(),
            "persist before resume",
        ),
        "resumed child should keep its persisted model context"
    );

    let _ = harness
        .control
        .shutdown_live_agent(child_thread_id)
        .await
        .expect("resumed child shutdown should succeed");
    let _ = parent_thread
        .submit(Op::Shutdown {})
        .await
        .expect("parent shutdown should submit");
}

#[tokio::test]
async fn list_agent_subtree_thread_ids_includes_anonymous_and_closed_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;
    let worker_path = AgentPath::root().join("worker").expect("worker path");
    let reviewer_path = AgentPath::root().join("reviewer").expect("reviewer path");

    let worker_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(worker_path.clone()),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker spawn should succeed");
    let worker_child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello worker child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: Some(
                    worker_path
                        .join("child")
                        .expect("worker child path should be valid"),
                ),
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("worker child spawn should succeed");
    let no_path_child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello anonymous child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: worker_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("no-path child spawn should succeed");
    let no_path_grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello anonymous grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: no_path_child_thread_id,
                depth: 3,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("no-path grandchild spawn should succeed");
    let _reviewer_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello reviewer"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: Some(reviewer_path),
                agent_nickname: None,
                agent_role: Some("reviewer".to_string()),
            })),
        )
        .await
        .expect("reviewer spawn should succeed");

    let _ = harness
        .control
        .shutdown_live_agent(no_path_grandchild_thread_id)
        .await
        .expect("no-path grandchild shutdown should succeed");

    let mut worker_subtree_thread_ids = harness
        .manager
        .list_agent_subtree_thread_ids(worker_thread_id)
        .await
        .expect("worker subtree thread ids should load");
    worker_subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_worker_subtree_thread_ids = vec![
        worker_thread_id,
        worker_child_thread_id,
        no_path_child_thread_id,
        no_path_grandchild_thread_id,
    ];
    expected_worker_subtree_thread_ids.sort_by_key(ToString::to_string);
    assert_eq!(
        worker_subtree_thread_ids,
        expected_worker_subtree_thread_ids
    );

    let mut no_path_child_subtree_thread_ids = harness
        .manager
        .list_agent_subtree_thread_ids(no_path_child_thread_id)
        .await
        .expect("no-path subtree thread ids should load");
    no_path_child_subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_no_path_child_subtree_thread_ids =
        vec![no_path_child_thread_id, no_path_grandchild_thread_id];
    expected_no_path_child_subtree_thread_ids.sort_by_key(ToString::to_string);
    assert_eq!(
        no_path_child_subtree_thread_ids,
        expected_no_path_child_subtree_thread_ids
    );
}

#[tokio::test]
async fn list_agent_subtree_thread_ids_finds_live_descendants_of_unloaded_root() {
    let (_home, config) = test_config().await;
    let manager = ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::from_api_key("dummy"),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        std::sync::Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        /*state_db*/ None,
    );
    let control = manager.agent_control();
    let parent_thread_id = manager
        .start_thread(StartThreadOptions::new(config.clone()))
        .await
        .expect("parent should start")
        .thread_id;

    let child_thread_id = control
        .spawn_agent(
            config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = control
        .spawn_agent(
            config,
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    manager.remove_thread(&parent_thread_id).await;

    let mut subtree_thread_ids = manager
        .list_agent_subtree_thread_ids(parent_thread_id)
        .await
        .expect("live subtree should load");
    subtree_thread_ids.sort_by_key(ToString::to_string);
    let mut expected_subtree_thread_ids =
        vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_subtree_thread_ids.sort_by_key(ToString::to_string);

    assert_eq!(subtree_thread_ids, expected_subtree_thread_ids);
}

#[tokio::test]
async fn close_agent_closes_live_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(parent_thread_id)
        .await
        .expect("tree shutdown should succeed");

    assert_eq!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let shutdown_ids = harness
        .manager
        .captured_ops()
        .into_iter()
        .filter_map(|(thread_id, op)| matches!(op, Op::Shutdown).then_some(thread_id))
        .collect::<Vec<_>>();
    let mut expected_shutdown_ids = vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_shutdown_ids.sort_by_key(std::string::ToString::to_string);
    let mut shutdown_ids = shutdown_ids;
    shutdown_ids.sort_by_key(std::string::ToString::to_string);
    assert_eq!(shutdown_ids, expected_shutdown_ids);
}

#[tokio::test]
async fn close_agent_closes_descendants_when_started_at_child() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, _parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");

    let _ = harness
        .control
        .close_agent(parent_thread_id)
        .await
        .expect("tree shutdown should succeed");

    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );

    let shutdown_ids = harness
        .manager
        .captured_ops()
        .into_iter()
        .filter_map(|(thread_id, op)| matches!(op, Op::Shutdown).then_some(thread_id))
        .collect::<Vec<_>>();
    let mut expected_shutdown_ids = vec![parent_thread_id, child_thread_id, grandchild_thread_id];
    expected_shutdown_ids.sort_by_key(std::string::ToString::to_string);
    let mut shutdown_ids = shutdown_ids;
    shutdown_ids.sort_by_key(std::string::ToString::to_string);
    assert_eq!(shutdown_ids, expected_shutdown_ids);
}

async fn harness_for_multi_agent_version(
    multi_agent_version: MultiAgentVersion,
) -> AgentControlHarness {
    let (home, mut config) = test_config().await;
    match multi_agent_version {
        MultiAgentVersion::Disabled => panic!("test requires an enabled multi-agent version"),
        MultiAgentVersion::V1 => {}
        MultiAgentVersion::V2 => {
            let _ = config.features.enable(Feature::MultiAgentV2);
        }
    }
    AgentControlHarness::new_with_config(home, config).await
}

async fn assert_close_wins_lifecycle_race_and_revokes_observation(
    multi_agent_version: MultiAgentVersion,
) {
    let harness = harness_for_multi_agent_version(multi_agent_version).await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let agent_path = AgentPath::try_from("/root/worker").expect("agent path");
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: Some(agent_path.clone()),
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(child_source.clone()),
        )
        .await
        .expect("child spawn should succeed");
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    persist_thread_for_tree_resume(&child_thread, "child persisted before close").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    let parent_presentation = parent_thread.session.presentation_id();
    let child_presentation = child_thread.session.presentation_id();
    assert!(
        harness
            .control
            .has_completion_watcher(parent_presentation, child_presentation)
    );
    let secondary_listener = harness.manager.agent_control();
    secondary_listener
        .maybe_start_completion_watcher(
            &child_thread,
            Some(child_source.clone()),
            agent_path.to_string(),
            Some(agent_path),
            ResponseObservationPolicy::default(),
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await
        .expect("secondary listener should subscribe");
    assert!(secondary_listener.has_completion_watcher(parent_presentation, child_presentation));

    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let initial_lifecycle_generation = state.agent_lifecycle_generation(child_thread_id);
    let lifecycle_lock = state.agent_lifecycle_lock(child_thread_id);
    let lifecycle_guard = lifecycle_lock.lock_owned().await;
    let (close_started_tx, close_started_rx) = tokio::sync::oneshot::channel();
    let close_control = harness.control.clone();
    let close_task = tokio::spawn(async move {
        let _ = close_started_tx.send(());
        close_control.close_agent(child_thread_id).await
    });
    close_started_rx
        .await
        .expect("close task should reach the lifecycle boundary");
    tokio::task::yield_now().await;
    assert!(
        !close_task.is_finished(),
        "close must wait for the in-flight resume/send lifecycle owner"
    );
    let late_adoption = if multi_agent_version == MultiAgentVersion::V1 {
        let adoption_control = harness.manager.agent_control();
        let adoption_source = child_source.clone();
        let observed_status = child_thread.agent_status().await;
        Some((
            adoption_control.clone(),
            tokio::spawn(async move {
                adoption_control
                    .ensure_v1_completion_watcher(
                        child_thread_id,
                        adoption_source,
                        ResponseObservationPolicy::default(),
                        observed_status,
                    )
                    .await
            }),
        ))
    } else {
        None
    };
    drop(lifecycle_guard);
    timeout(Duration::from_secs(5), close_task)
        .await
        .expect("close should finish after lifecycle owner releases")
        .expect("close task should not panic")
        .expect("close should succeed");
    if let Some((adoption_control, late_adoption)) = late_adoption {
        assert_matches!(
            timeout(Duration::from_secs(5), late_adoption)
                .await
                .expect("late adoption should finish after close")
                .expect("late adoption task should not panic"),
            Err(err)
                if matches!(
                    err.details(),
                    CodexErrorDetails::ThreadNotFound(thread_id)
                        if *thread_id == child_thread_id
                )
        );
        assert!(
            !adoption_control.has_completion_watcher(parent_presentation, child_presentation),
            "an adoption queued behind close must not install a new watcher"
        );
    }

    assert_eq!(
        state.agent_lifecycle_generation(child_thread_id),
        initial_lifecycle_generation.wrapping_add(1)
    );
    assert!(
        !harness
            .control
            .has_completion_watcher(parent_presentation, child_presentation)
    );
    assert!(
        harness
            .control
            .response_observation_snapshots(parent_presentation, child_presentation)
            .is_empty()
    );
    timeout(Duration::from_secs(5), async {
        while secondary_listener.has_completion_watcher(parent_presentation, child_presentation) {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("close should invalidate subscriptions owned by another control plane");
    let closed_children = state
        .agent_graph_store()
        .expect("agent graph store")
        .list_thread_spawn_children(
            parent_thread_id,
            Some(codex_agent_graph_store::ThreadSpawnEdgeStatus::Closed),
        )
        .await
        .expect("closed child query should succeed");
    assert!(closed_children.contains(&child_thread_id));

    let resumed_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            child_source,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("explicit resume should reopen the closed child");
    assert_eq!(resumed_thread_id, child_thread_id);
    let resumed_child = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("resumed child should be live");
    assert_eq!(
        state.agent_lifecycle_generation(child_thread_id),
        initial_lifecycle_generation.wrapping_add(1)
    );
    assert!(
        harness
            .control
            .has_completion_watcher(parent_presentation, resumed_child.session.presentation_id())
    );
    assert!(
        !secondary_listener
            .has_completion_watcher(parent_presentation, resumed_child.session.presentation_id()),
        "an explicitly resumed caller must not restore another listener's pre-close subscription"
    );

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("resumed child close should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

#[tokio::test]
async fn v1_close_wins_lifecycle_race_and_revokes_wake_observation() {
    assert_close_wins_lifecycle_race_and_revokes_observation(MultiAgentVersion::V1).await;
}

#[tokio::test]
async fn v2_close_wins_lifecycle_race_and_revokes_wake_observation() {
    assert_close_wins_lifecycle_race_and_revokes_observation(MultiAgentVersion::V2).await;
}

#[tokio::test]
async fn adopted_v1_child_records_foreign_wait_presentation_before_final_status() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let child_control = harness.manager.agent_control();
    let child = state
        .spawn_new_thread_with_source(
            harness.config.clone(),
            child_control,
            child_source.clone(),
            /*history_mode*/ None,
            /*parent_thread_id*/ Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            /*thread_source*/ Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("independently controlled child should start");
    assert!(
        !Arc::ptr_eq(
            &harness.control.wait_agent_presentations,
            &child
                .thread
                .session
                .services
                .agent_control
                .wait_agent_presentations,
        ),
        "test requires distinct AgentControl presentation registries"
    );
    harness
        .control
        .ensure_v1_completion_watcher(
            child.thread_id,
            child_source,
            ResponseObservationPolicy::default(),
            child.thread.agent_status().await,
        )
        .await
        .expect("foreign V1 watcher should attach");
    let parent_presentation = parent_thread.session.presentation_id();
    let wait = harness
        .control
        .register_targeted_wait_agent_presentation(parent_presentation, &[child.thread_id]);

    child
        .thread
        .session
        .send_event_raw(Event {
            id: "turn-1".to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "turn-1".to_string(),
                last_agent_message: Some("foreign child done".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        })
        .await;

    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(
        (commit.agent_states(), child.thread.agent_status().await,),
        (
            HashMap::from([(
                child.thread_id,
                AgentStatus::Completed(Some("foreign child done".to_string())),
            )]),
            AgentStatus::Completed(Some("foreign child done".to_string())),
        ),
    );
    commit.commit();

    let _ = harness
        .control
        .shutdown_live_agent(child.thread_id)
        .await
        .expect("child shutdown should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

#[tokio::test]
async fn closing_completed_child_wakes_foreign_watcher_with_retained_runtime() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let child_owner = harness.manager.agent_control();
    let child = state
        .spawn_new_thread_with_source(
            harness.config.clone(),
            child_owner.clone(),
            child_source.clone(),
            /*history_mode*/ None,
            /*parent_thread_id*/ Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            /*thread_source*/ Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("independently controlled child should start");
    let retained_child = Arc::clone(&child.thread);
    let parent_presentation = parent_thread.session.presentation_id();
    let child_presentation = child.thread.session.presentation_id();
    harness
        .control
        .ensure_v1_completion_watcher(
            child.thread_id,
            child_source,
            ResponseObservationPolicy::default(),
            child.thread.agent_status().await,
        )
        .await
        .expect("foreign V1 watcher should attach");
    assert!(
        !Arc::ptr_eq(
            &harness.control.wait_agent_presentations,
            &child_owner.wait_agent_presentations,
        ),
        "test requires distinct owner and observer presentation registries"
    );

    child
        .thread
        .session
        .send_event_raw(Event {
            id: "completed-before-close".to_string(),
            msg: EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: "completed-before-close".to_string(),
                last_agent_message: Some("completed before close".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        })
        .await;
    assert!(wait_for_subagent_notification(&parent_thread).await);
    assert!(wait_for_subagent_completion_item(&parent_thread).await);
    assert!(
        harness
            .control
            .has_completion_watcher(parent_presentation, child_presentation)
    );

    child_owner
        .close_agent(child.thread_id)
        .await
        .expect("owner should close its completed child");
    timeout(Duration::from_secs(5), async {
        while harness
            .control
            .has_completion_watcher(parent_presentation, child_presentation)
        {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("lifecycle notification should stop the foreign watcher");
    assert_eq!(
        retained_child.agent_status().await,
        AgentStatus::Completed(Some("completed before close".to_string())),
        "explicit close must not rewrite the retained runtime's completed status"
    );

    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

#[tokio::test]
async fn stale_recovery_generation_revokes_only_its_obsolete_presentation() {
    let harness = AgentControlHarness::new().await;
    let (_parent_thread_id, parent_thread) = harness.start_thread().await;
    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let parent = parent_thread.session.presentation_id();
    let child_thread_id = ThreadId::new();
    let obsolete_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let fresh_child = SessionPresentationId::new(child_thread_id, uuid::Uuid::now_v7());
    let obsolete_generation = harness.control.agent_lifecycle_generation(child_thread_id);
    let mut obsolete_registration = harness
        .control
        .register_response_watcher_with_admission(
            obsolete_child,
            parent,
            &parent_thread.session.submission_admission,
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ true,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("obsolete watcher registration");
    let obsolete_observations = harness
        .control
        .response_observation_snapshots(parent, obsolete_child);
    obsolete_registration.preserve_state_for_replacement_on_drop();
    drop(obsolete_registration);
    state.advance_agent_lifecycle_generation(child_thread_id);
    let _fresh_registration = harness
        .control
        .register_response_watcher_with_admission(
            fresh_child,
            parent,
            &parent_thread.session.submission_admission,
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ true,
            /*target_turn_id*/ None,
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("fresh watcher registration");

    harness
        .control
        .restore_v1_response_observer(
            parent,
            child_thread_id,
            obsolete_generation,
            /*previous_child*/ Some(obsolete_child),
            obsolete_observations,
        )
        .await;

    assert!(
        harness
            .control
            .response_observation_snapshots(parent, obsolete_child)
            .is_empty()
    );
    assert!(
        !harness
            .control
            .response_observation_snapshots(parent, fresh_child)
            .is_empty(),
        "stale cleanup must preserve a fresh post-close presentation for the same thread UUID"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mutually_observing_live_agents_do_not_deadlock_lifecycle_setup() {
    let harness = AgentControlHarness::new().await;
    let (first_thread_id, first_thread) = harness.start_thread().await;
    let (second_thread_id, second_thread) = harness.start_thread().await;
    let first_control = first_thread.session.services.agent_control.clone();
    let second_control = second_thread.session.services.agent_control.clone();
    let first_presentation = first_thread.session.presentation_id();
    let second_presentation = second_thread.session.presentation_id();
    let first_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: first_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("observer".to_string()),
    });
    let second_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: second_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("observer".to_string()),
    });
    let start = Arc::new(tokio::sync::Barrier::new(/*n*/ 3));
    let first_start = Arc::clone(&start);
    let first_status = second_thread.agent_status().await;
    let first_adoption = tokio::spawn({
        let first_control = first_control.clone();
        async move {
            first_start.wait().await;
            first_control
                .ensure_v1_completion_watcher(
                    second_thread_id,
                    first_source,
                    ResponseObservationPolicy::default(),
                    first_status,
                )
                .await
        }
    });
    let second_start = Arc::clone(&start);
    let second_status = first_thread.agent_status().await;
    let second_adoption = tokio::spawn({
        let second_control = second_control.clone();
        async move {
            second_start.wait().await;
            second_control
                .ensure_v1_completion_watcher(
                    first_thread_id,
                    second_source,
                    ResponseObservationPolicy::default(),
                    second_status,
                )
                .await
        }
    });
    start.wait().await;

    timeout(Duration::from_secs(5), async {
        first_adoption
            .await
            .expect("first adoption task should not panic")
            .expect("first adoption should succeed");
        second_adoption
            .await
            .expect("second adoption task should not panic")
            .expect("second adoption should succeed");
    })
    .await
    .expect("mutual adoption should not deadlock");
    assert!(first_control.has_completion_watcher(first_presentation, second_presentation));
    assert!(second_control.has_completion_watcher(second_presentation, first_presentation));
}

#[tokio::test]
async fn v1_observer_honors_response_policy_for_a_v2_target() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let mut child_config = harness.config.clone();
    child_config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("enable multi-agent V2");
    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let child = state
        .spawn_new_thread_with_source(
            child_config,
            harness.manager.agent_control(),
            child_source.clone(),
            /*history_mode*/ None,
            /*parent_thread_id*/ Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            /*thread_source*/ Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("independently controlled V2 child should start");
    let parent_presentation = parent_thread.session.presentation_id();
    let child_presentation = child.thread.session.presentation_id();
    harness
        .control
        .ensure_v1_completion_watcher(
            child.thread_id,
            child_source,
            ResponseObservationPolicy::default(),
            child.thread.agent_status().await,
        )
        .await
        .expect("V1 observer should attach to a V2 target");
    assert!(
        harness
            .control
            .has_completion_watcher(parent_presentation, child_presentation)
    );

    let observation_transaction = harness
        .control
        .acquire_response_observation_transaction(parent_presentation)
        .await;
    let wait = harness
        .control
        .register_targeted_wait_agent_presentation(parent_presentation, &[child.thread_id]);
    let child_turn = child.thread.session.new_default_turn().await;
    child
        .thread
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some("mixed-version child done".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(
            child.thread_id,
            AgentStatus::Completed(Some("mixed-version child done".to_string())),
        )])
    );
    commit.commit();
    drop(observation_transaction);

    let _ = harness
        .control
        .shutdown_live_agent(child.thread_id)
        .await
        .expect("child shutdown should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

#[tokio::test]
async fn restored_foreign_v1_observer_records_v2_terminal_before_async_delivery() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let mut child_config = harness.config.clone();
    child_config
        .features
        .enable(Feature::MultiAgentV2)
        .expect("enable multi-agent V2");
    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let child = state
        .spawn_new_thread_with_source(
            child_config,
            harness.manager.agent_control(),
            child_source,
            /*history_mode*/ None,
            /*parent_thread_id*/ Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            /*thread_source*/ Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("independently controlled V2 child should start");
    let parent_presentation = parent_thread.session.presentation_id();
    let child_presentation = child.thread.session.presentation_id();
    let child_turn = child.thread.session.new_default_turn().await;
    let mut replaced_registration = harness
        .control
        .register_response_watcher_with_admission(
            child_presentation,
            parent_presentation,
            &parent_thread.session.submission_admission,
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ true,
            /*target_turn_id*/ Some(child_turn.sub_id.clone()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("initial watcher registration");
    let observations = harness
        .control
        .response_observation_snapshots(parent_presentation, child_presentation);
    replaced_registration.preserve_state_for_replacement_on_drop();
    drop(replaced_registration);
    harness
        .control
        .restore_v1_response_observer(
            parent_presentation,
            child.thread_id,
            harness.control.agent_lifecycle_generation(child.thread_id),
            /*previous_child*/ None,
            observations,
        )
        .await;
    assert!(
        harness
            .control
            .has_completion_watcher(parent_presentation, child_presentation)
    );

    let observation_transaction = harness
        .control
        .acquire_response_observation_transaction(parent_presentation)
        .await;
    let wait = harness
        .control
        .register_targeted_wait_agent_presentation(parent_presentation, &[child.thread_id]);
    child
        .thread
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some("restored mixed-version child done".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    let commit = wait.freeze_for_children([child.thread_id]);
    assert_eq!(
        commit.agent_states(),
        HashMap::from([(
            child.thread_id,
            AgentStatus::Completed(Some("restored mixed-version child done".to_string())),
        )])
    );
    commit.commit();
    drop(observation_transaction);

    let _ = harness
        .control
        .shutdown_live_agent(child.thread_id)
        .await
        .expect("child shutdown should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

#[tokio::test]
async fn response_observer_retries_a_transient_canonical_history_read_failure() {
    let (home, config) = test_config().await;
    let thread_store = Arc::new(InMemoryThreadStore::default());
    let auth_manager = AuthManager::from_auth_for_testing(CodexAuth::from_api_key("dummy"));
    let manager = ThreadManager::new(
        &config,
        auth_manager.clone(),
        crate::thread_manager::build_models_manager(&config, auth_manager),
        crate::CodexAppsToolsCache::default(),
        SessionSource::Exec,
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        empty_extension_registry(),
        Arc::new(crate::test_support::EmptyUserInstructionsProvider),
        /*analytics_events_client*/ None,
        thread_store.clone(),
        /*agent_graph_store*/ None,
        uuid::Uuid::new_v4().to_string(),
        /*attestation_provider*/ None,
        /*external_time_provider*/ None,
    );
    let control = manager.agent_control();
    let harness = AgentControlHarness {
        _home: home,
        config,
        state_db: None,
        manager,
        control,
    };
    let (parent_thread_id, parent_thread) = harness.start_thread().await;
    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let child = state
        .spawn_new_thread_with_source(
            harness.config.clone(),
            harness.manager.agent_control(),
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            }),
            /*history_mode*/ None,
            Some(parent_thread_id),
            /*forked_from_thread_id*/ None,
            Some(ThreadSource::Subagent),
            /*metrics_service_name*/ None,
            /*inherited_environments*/ None,
            /*inherited_exec_policy*/ None,
            /*environments*/ None,
        )
        .await
        .expect("start child thread");
    child.thread.ensure_rollout_materialized().await;
    let child_thread_id = child.thread_id;
    let parent_presentation = parent_thread.session.presentation_id();
    let child_presentation = child.thread.session.presentation_id();
    let child_turn = child.thread.session.new_default_turn().await;
    let mut replaced_registration = harness
        .control
        .register_response_watcher_with_admission(
            child_presentation,
            parent_presentation,
            &parent_thread.session.submission_admission,
            ResponseObservationPolicy::default(),
            /*retain_passive_completion_relationship*/ true,
            /*target_turn_id*/ Some(child_turn.sub_id.clone()),
            ResponseObservationBinding::NextTurn,
            ResponseObservationPersistence::Durable,
        )
        .expect("initial watcher registration");
    let observations = harness
        .control
        .response_observation_snapshots(parent_presentation, child_presentation);
    replaced_registration.preserve_state_for_replacement_on_drop();
    drop(replaced_registration);
    child
        .thread
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnStarted(TurnStartedEvent {
                turn_id: child_turn.sub_id.clone(),
                trace_id: None,
                started_at: None,
                model_context_window: None,
                collaboration_mode_kind: Default::default(),
            }),
        )
        .await;
    child
        .thread
        .session
        .send_event(
            child_turn.as_ref(),
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: child_turn.sub_id.clone(),
                last_agent_message: Some("recovered after transient read failure".to_string()),
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
            }),
        )
        .await;
    child
        .thread
        .flush_rollout()
        .await
        .expect("flush completed child history");
    let removed = harness
        .manager
        .remove_thread(&child.thread_id)
        .await
        .expect("remove completed child runtime");
    assert!(Arc::ptr_eq(&removed, &child.thread));
    thread_store
        .fail_next_operation(InMemoryThreadStoreFailure::AgentResponseObservationHistoryRead)
        .await;

    let restore = tokio::spawn({
        let control = harness.control.clone();
        async move {
            control
                .restore_v1_response_observer(
                    parent_presentation,
                    child_thread_id,
                    control.agent_lifecycle_generation(child_thread_id),
                    /*previous_child*/ Some(child_presentation),
                    observations,
                )
                .await;
        }
    });
    assert!(
        wait_for_subagent_notification(&parent_thread).await,
        "observer should retry the canonical read and recover the completion"
    );
    state.advance_agent_lifecycle_generation(child_thread_id);
    timeout(Duration::from_secs(5), restore)
        .await
        .expect("restored observer should stop after lifecycle invalidation")
        .expect("restored observer task should not panic");

    harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

async fn assert_close_wins_queued_child_spawn(multi_agent_version: MultiAgentVersion) {
    let harness = harness_for_multi_agent_version(multi_agent_version).await;
    let (root_thread_id, _) = harness.start_thread().await;
    let parent_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("parent task"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: root_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("parent agent spawn should succeed");
    wait_for_live_thread_spawn_children(&harness.control, root_thread_id, &[parent_thread_id])
        .await;

    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let parent_guard = state
        .agent_lifecycle_lock(parent_thread_id)
        .lock_owned()
        .await;
    let (close_started_tx, close_started_rx) = tokio::sync::oneshot::channel();
    let close_control = harness.control.clone();
    let close_task = tokio::spawn(async move {
        let _ = close_started_tx.send(());
        close_control.close_agent(parent_thread_id).await
    });
    close_started_rx
        .await
        .expect("close task should reach the parent lifecycle boundary");
    tokio::task::yield_now().await;

    let spawn_control = harness.control.clone();
    let spawn_config = harness.config.clone();
    let spawn_task = tokio::spawn(async move {
        spawn_control
            .spawn_agent(
                spawn_config,
                text_input("late child task"),
                Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                    parent_thread_id,
                    depth: 2,
                    agent_path: None,
                    agent_nickname: None,
                    agent_role: Some("worker".to_string()),
                })),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(!close_task.is_finished());
    assert!(!spawn_task.is_finished());

    drop(parent_guard);
    timeout(Duration::from_secs(5), close_task)
        .await
        .expect("parent close should finish")
        .expect("parent close task should not panic")
        .expect("parent close should succeed");
    assert_matches!(
        timeout(Duration::from_secs(5), spawn_task)
            .await
            .expect("late child spawn should finish")
            .expect("late child spawn task should not panic"),
        Err(err)
            if matches!(
                err.details(),
                CodexErrorDetails::ThreadNotFound(thread_id)
                    if *thread_id == parent_thread_id
            )
    );
    assert_eq!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness
            .control
            .list_live_agent_subtree_thread_ids(root_thread_id)
            .await
            .expect("root subtree should remain queryable"),
        vec![root_thread_id],
        "close must not leave the parent or a late child live",
    );

    let _ = harness
        .control
        .shutdown_live_agent(root_thread_id)
        .await
        .expect("root shutdown should succeed");
}

#[tokio::test]
async fn v1_close_wins_queued_child_spawn() {
    assert_close_wins_queued_child_spawn(MultiAgentVersion::V1).await;
}

#[tokio::test]
async fn v2_close_wins_queued_child_spawn() {
    assert_close_wins_queued_child_spawn(MultiAgentVersion::V2).await;
}

async fn assert_close_wins_queued_child_resume(multi_agent_version: MultiAgentVersion) {
    let harness = harness_for_multi_agent_version(multi_agent_version).await;
    let (root_thread_id, _) = harness.start_thread().await;
    let parent_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: root_thread_id,
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("explorer".to_string()),
    });
    let parent_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("parent task"),
            Some(parent_source),
        )
        .await
        .expect("parent agent spawn should succeed");
    let child_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id,
        depth: 2,
        agent_path: None,
        agent_nickname: None,
        agent_role: Some("worker".to_string()),
    });
    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("child task"),
            Some(child_source.clone()),
        )
        .await
        .expect("child agent spawn should succeed");
    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    persist_thread_for_tree_resume(&child_thread, "child persisted before queued resume").await;
    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");

    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let parent_guard = state
        .agent_lifecycle_lock(parent_thread_id)
        .lock_owned()
        .await;
    let (close_started_tx, close_started_rx) = tokio::sync::oneshot::channel();
    let close_control = harness.control.clone();
    let close_task = tokio::spawn(async move {
        let _ = close_started_tx.send(());
        close_control.close_agent(parent_thread_id).await
    });
    close_started_rx
        .await
        .expect("parent close should reach the lifecycle boundary");
    tokio::task::yield_now().await;

    let resume_control = harness.control.clone();
    let resume_config = harness.config.clone();
    let resume_task = tokio::spawn(async move {
        resume_control
            .resume_agent_from_rollout(
                resume_config,
                child_thread_id,
                child_source,
                ResponseObservationPolicy::default(),
            )
            .await
    });
    tokio::task::yield_now().await;
    assert!(!close_task.is_finished());
    assert!(!resume_task.is_finished());

    drop(parent_guard);
    timeout(Duration::from_secs(5), close_task)
        .await
        .expect("parent close should finish")
        .expect("parent close task should not panic")
        .expect("parent close should succeed");
    assert_matches!(
        timeout(Duration::from_secs(5), resume_task)
            .await
            .expect("late child resume should finish")
            .expect("late child resume task should not panic"),
        Err(err)
            if matches!(
                err.details(),
                CodexErrorDetails::ThreadNotFound(thread_id)
                    if *thread_id == parent_thread_id
            )
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .shutdown_live_agent(root_thread_id)
        .await
        .expect("root shutdown should succeed");
}

#[tokio::test]
async fn v1_close_wins_queued_child_resume() {
    assert_close_wins_queued_child_resume(MultiAgentVersion::V1).await;
}

#[tokio::test]
async fn v2_close_wins_queued_child_resume() {
    assert_close_wins_queued_child_resume(MultiAgentVersion::V2).await;
}

#[tokio::test]
async fn resume_agent_from_rollout_does_not_reopen_closed_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;
    let state = harness
        .control
        .upgrade()
        .expect("thread manager should be live");
    let child_generation = state.agent_lifecycle_generation(child_thread_id);
    let grandchild_generation = state.agent_lifecycle_generation(grandchild_thread_id);

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");
    assert_eq!(
        (
            state.agent_lifecycle_generation(child_thread_id),
            state.agent_lifecycle_generation(grandchild_thread_id),
        ),
        (
            child_generation.wrapping_add(1),
            grandchild_generation.wrapping_add(1),
        ),
        "subtree close must revoke both target and descendant observations",
    );
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("single-thread resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .close_agent(parent_thread_id)
        .await
        .expect("tree shutdown after resume should succeed");
}

#[tokio::test]
async fn resume_closed_child_reopens_open_descendants() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close should succeed");

    let resumed_child_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            child_thread_id,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: None,
            }),
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("child resume should succeed");
    assert_eq!(resumed_child_thread_id, child_thread_id);
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .close_agent(child_thread_id)
        .await
        .expect("child close after resume should succeed");
    let _ = harness
        .control
        .shutdown_live_agent(parent_thread_id)
        .await
        .expect("parent shutdown should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_reopens_open_descendants_after_manager_shutdown() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("tree resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .close_agent(parent_thread_id)
        .await
        .expect("tree shutdown after subtree resume should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_uses_edge_data_when_descendant_metadata_source_is_stale() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let state_db = grandchild_thread
        .state_db()
        .expect("sqlite state db should be available");
    let mut stale_metadata = state_db
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild metadata query should succeed")
        .expect("grandchild metadata should exist");
    stale_metadata.source =
        serde_json::to_string(&SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id: ThreadId::new(),
            depth: 99,
            agent_path: None,
            agent_nickname: None,
            agent_role: Some("worker".to_string()),
        }))
        .expect("stale session source should serialize");
    state_db
        .upsert_thread(&stale_metadata)
        .await
        .expect("stale grandchild metadata should persist");

    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("tree resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_ne!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let resumed_grandchild_snapshot = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("resumed grandchild thread should exist")
        .config_snapshot()
        .await;
    let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: resumed_parent_thread_id,
        depth: resumed_depth,
        ..
    }) = resumed_grandchild_snapshot.session_source
    else {
        panic!("expected thread-spawn sub-agent source");
    };
    assert_eq!(resumed_parent_thread_id, child_thread_id);
    assert_eq!(resumed_depth, 2);

    let _ = harness
        .control
        .close_agent(parent_thread_id)
        .await
        .expect("tree shutdown after subtree resume should succeed");
}

#[tokio::test]
async fn resume_agent_from_rollout_skips_descendants_when_parent_resume_fails() {
    let harness = AgentControlHarness::new().await;
    let (parent_thread_id, parent_thread) = harness.start_thread().await;

    let child_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello child"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id,
                depth: 1,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("explorer".to_string()),
            })),
        )
        .await
        .expect("child spawn should succeed");
    let grandchild_thread_id = harness
        .control
        .spawn_agent(
            harness.config.clone(),
            text_input("hello grandchild"),
            Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: child_thread_id,
                depth: 2,
                agent_path: None,
                agent_nickname: None,
                agent_role: Some("worker".to_string()),
            })),
        )
        .await
        .expect("grandchild spawn should succeed");

    let child_thread = harness
        .manager
        .get_thread(child_thread_id)
        .await
        .expect("child thread should exist");
    let grandchild_thread = harness
        .manager
        .get_thread(grandchild_thread_id)
        .await
        .expect("grandchild thread should exist");
    persist_thread_for_tree_resume(&parent_thread, "parent persisted").await;
    persist_thread_for_tree_resume(&child_thread, "child persisted").await;
    persist_thread_for_tree_resume(&grandchild_thread, "grandchild persisted").await;
    wait_for_live_thread_spawn_children(&harness.control, parent_thread_id, &[child_thread_id])
        .await;
    wait_for_live_thread_spawn_children(&harness.control, child_thread_id, &[grandchild_thread_id])
        .await;

    let child_rollout_path = child_thread
        .rollout_path()
        .expect("child thread should have rollout path");
    let report = harness
        .manager
        .shutdown_all_threads_bounded(Duration::from_secs(5))
        .await;
    assert_eq!(report.submit_failed, Vec::<ThreadId>::new());
    assert_eq!(report.timed_out, Vec::<ThreadId>::new());
    tokio::fs::remove_file(&child_rollout_path)
        .await
        .expect("child rollout path should be removable");

    let resumed_parent_thread_id = harness
        .control
        .resume_agent_from_rollout(
            harness.config.clone(),
            parent_thread_id,
            SessionSource::Exec,
            ResponseObservationPolicy::default(),
        )
        .await
        .expect("root resume should succeed");
    assert_eq!(resumed_parent_thread_id, parent_thread_id);
    assert_ne!(
        harness.control.get_status(parent_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(child_thread_id).await,
        AgentStatus::NotFound
    );
    assert_eq!(
        harness.control.get_status(grandchild_thread_id).await,
        AgentStatus::NotFound
    );

    let _ = harness
        .control
        .close_agent(parent_thread_id)
        .await
        .expect("tree shutdown after partial subtree resume should succeed");
}
