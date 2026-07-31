use crate::TurnInputRequest;
use crate::TurnInputSubmission;
use crate::TurnStartOptions;
use crate::agent::AgentStatus;
use crate::agent::registry::AgentMetadata;
use crate::agent::registry::AgentRegistry;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::resolve_role_config;
use crate::agent::status::is_final;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::codex_thread::CodexThread;
use crate::codex_thread::ThreadConfigSnapshot;
use crate::config::Config;
use crate::config::RolloutBudgetConfig;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::rollout_budget::RolloutBudget;
use crate::session::CompletionSubmissionAdmission;
use crate::session::emit_subagent_session_started;
use crate::session::multi_agents::ResolvedMultiAgentV2UsageHints;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::session_prefix::format_subagent_context_line;
use crate::thread_manager::ResumeThreadWithHistoryOptions;
use crate::thread_manager::ThreadIdGenerator;
use crate::thread_manager::ThreadManagerState;
use crate::thread_manager::default_thread_id_generator;
use crate::thread_rollout_truncation::truncate_rollout_to_last_n_fork_turns;
use crate::turn_timing::now_unix_timestamp_ms;
use arc_swap::ArcSwapOption;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_protocol::AgentPath;
use codex_protocol::ResponseItemId;
use codex_protocol::SessionId;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::items::TurnItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::HasLegacyEvent;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::protocol::ItemStartedEvent;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_protocol::turn_input::CyberAccessProgram;
use codex_protocol::user_input::UserInput;
use codex_rollout_trace::AgentResultTracePayload;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ReadThreadParams;
use serde::Serialize;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use tokio::sync::watch;
use tracing::warn;
use uuid::Uuid;

pub(crate) use self::execution::AgentExecutionGuard;
use self::execution::AgentExecutionLimiter;
pub(crate) use self::legacy::LiveAgentMetadataDisposition;
pub(crate) use self::presentation::AgentTerminalPresentation;
pub(crate) use self::presentation::SessionPresentationId;
use self::presentation::SpawnedThreadRelease;
pub(crate) use self::presentation::TerminalPresentationDelivery;
use self::presentation::WaitAgentPresentations;
use self::presentation::WatcherTerminalPresentation;
use self::residency::V2Residency;

const ROOT_LAST_TASK_MESSAGE: &str = "Main thread";

mod execution;
mod legacy;
mod presentation;
mod residency;
mod service_tier;
mod spawn;
mod user_authorization;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SpawnAgentForkMode {
    FullHistory,
    LastNTurns(usize),
}

#[derive(Clone, Debug, Default)]
pub(crate) struct SpawnAgentOptions {
    pub(crate) fork_parent_spawn_call_id: Option<String>,
    pub(crate) fork_mode: Option<SpawnAgentForkMode>,
    pub(crate) parent_thread_id: Option<ThreadId>,
    pub(crate) parent_turn_id: Option<String>,
    pub(crate) root_turn_id: Option<String>,
    pub(crate) environments: Option<Vec<TurnEnvironmentSelection>>,
    pub(crate) multi_agent_v2_usage_hints: Option<ResolvedMultiAgentV2UsageHints>,
    pub(crate) cyber_access_program: Option<CyberAccessProgram>,
}

enum InterAgentSubmission<'a> {
    Ordinary {
        start_options: TurnStartOptions,
    },
    Completion {
        presentation: &'a AgentTerminalPresentation,
        admission: CompletionSubmissionAdmission,
    },
}

struct CompletionContextAuthorizationGuard {
    control: AgentControl,
    response_item_id: Option<ResponseItemId>,
    authorized: bool,
    committed: bool,
}

impl Drop for CompletionContextAuthorizationGuard {
    fn drop(&mut self) {
        if self.authorized
            && !self.committed
            && let Some(response_item_id) = self.response_item_id.as_ref()
        {
            self.control
                .discard_completion_context_response_item_id(response_item_id);
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LiveAgent {
    pub(crate) thread_id: ThreadId,
    pub(crate) metadata: AgentMetadata,
    pub(crate) status: AgentStatus,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub(crate) struct ListedAgent {
    pub(crate) agent_name: String,
    pub(crate) agent_status: AgentStatus,
    pub(crate) last_task_message: Option<String>,
}

/// Control-plane handle for multi-agent operations.
/// `AgentControl` is held by each session (via `SessionServices`). It provides capability to
/// spawn new agents and the inter-agent communication layer.
/// An `AgentControl` instance is intended to be created at most once per root thread/session
/// tree. That same `AgentControl` is then shared with every sub-agent spawned from that root,
/// which keeps the registry scoped to that root thread rather than the entire `ThreadManager`.
#[derive(Clone)]
pub(crate) struct AgentControl {
    /// session_id is equal to the root thread's ID.
    session_id: SessionId,
    /// Weak handle back to the global thread registry/state.
    /// This is `Weak` to avoid reference cycles and shadow persistence of the form
    /// `ThreadManagerState -> CodexThread -> Session -> SessionServices -> ThreadManagerState`.
    manager: Weak<ThreadManagerState>,
    /// Captured at construction so delegates retain their manager's allocation policy.
    thread_id_generator: ThreadIdGenerator,
    state: Arc<AgentRegistry>,
    v2_residency: Arc<V2Residency>,
    agent_execution_limiter: Arc<AgentExecutionLimiter>,
    wait_agent_presentations: Arc<WaitAgentPresentations>,
    /// Session-scoped state shared by the root thread and every cloned sub-agent control handle.
    rollout_budget: Arc<RolloutBudget>,
    /// The user-selected root routing tier, shared by the entire agent tree.
    root_service_tier: Arc<ArcSwapOption<String>>,
}

impl Default for AgentControl {
    fn default() -> Self {
        Self::new(
            Weak::default(),
            default_thread_id_generator(),
            /*rollout_budget*/ None,
        )
    }
}

impl AgentControl {
    /// Construct a new `AgentControl` that can spawn/message agents via the given manager state.
    pub(crate) fn new(
        manager: Weak<ThreadManagerState>,
        thread_id_generator: ThreadIdGenerator,
        rollout_budget: Option<RolloutBudgetConfig>,
    ) -> Self {
        let control = Self {
            session_id: SessionId::default(),
            manager,
            thread_id_generator,
            state: Arc::default(),
            v2_residency: Arc::default(),
            agent_execution_limiter: Arc::default(),
            wait_agent_presentations: Arc::default(),
            rollout_budget: Arc::default(),
            root_service_tier: Arc::new(ArcSwapOption::from(None)),
        };
        if let Some(rollout_budget) = rollout_budget {
            control.rollout_budget.configure(rollout_budget);
        }
        control
    }

    pub(crate) fn with_session_id(mut self, session_id: SessionId, max_threads: usize) -> Self {
        self.session_id = session_id;
        self.agent_execution_limiter.initialize(max_threads);
        self
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn generate_thread_id(&self) -> ThreadId {
        (self.thread_id_generator)()
    }

    pub(crate) fn rollout_budget(&self) -> &RolloutBudget {
        self.rollout_budget.as_ref()
    }

    /// Send rich user input items to an existing agent thread.
    pub(crate) async fn send_input(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let submission_semaphore = self.state.mailbox_submission_semaphore(agent_id);
        let _submission_permit = submission_semaphore.acquire_owned().await.map_err(|err| {
            CodexErr::Fatal(format!("mailbox submission semaphore closed: {err}"))
        })?;
        let last_task_message = non_empty_task_message(render_input_preview(&input));
        let thread = state.get_thread(agent_id).await?;
        let result = match thread
            .start_or_steer_turn(TurnInputRequest::user_input(input).on_start(start_options))
            .await
        {
            Ok(TurnInputSubmission::Started { turn_id }) => Ok(turn_id),
            Ok(TurnInputSubmission::Steered { .. }) => {
                // MAv1 exposes an opaque `submission_id` to the model. The legacy
                // `Op::UserInput` path returned a fresh ID for every steer, while the
                // turn-input API returns the active turn ID. Keep the tool-visible ID
                // unique without adding a submission receipt back to Core.
                Ok(Uuid::now_v7().to_string())
            }
            Ok(TurnInputSubmission::NotSubmitted { reason }) => Err(CodexErr::InvalidRequest(
                format!("turn input was not submitted: {reason:?}"),
            )),
            Err(err) => Err(err),
        };
        let result = self
            .handle_thread_request_result(&state, &thread, result)
            .await;
        if result.is_ok() {
            match last_task_message {
                Some(last_task_message) => self
                    .state
                    .update_last_task_message(agent_id, last_task_message),
                None => self.state.clear_last_task_message(agent_id),
            }
        }
        result
    }

    pub(crate) async fn send_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        communication: InterAgentCommunication,
        agent_communication_context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        if communication.trigger_turn {
            let thread = state.get_thread(agent_id).await?;
            self.ensure_execution_capacity_for_turn_start(&thread)
                .await?;
        }
        self.send_inter_agent_communication_after_capacity_check(
            agent_id,
            &state,
            communication,
            agent_communication_context,
            start_options,
        )
        .await
    }

    pub(crate) async fn emit_sub_agent_activity(
        &self,
        thread_id: ThreadId,
        turn_id: String,
        item: SubAgentActivityItem,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let thread = state.get_thread(thread_id).await?;
        let started_at_ms = now_unix_timestamp_ms();
        let item = TurnItem::SubAgentActivity(item);
        thread
            .session
            .send_event_raw(Event {
                id: turn_id.clone(),
                msg: EventMsg::ItemStarted(ItemStartedEvent {
                    thread_id,
                    turn_id: turn_id.clone(),
                    item: item.clone(),
                    started_at_ms,
                }),
            })
            .await;
        let completed_at_ms = now_unix_timestamp_ms();
        let completed = ItemCompletedEvent {
            thread_id,
            turn_id: turn_id.clone(),
            item,
            started_at_ms: Some(started_at_ms),
            completed_at_ms,
        };
        thread
            .session
            .send_event_raw(Event {
                id: turn_id.clone(),
                msg: EventMsg::ItemCompleted(completed.clone()),
            })
            .await;
        for legacy in completed.as_legacy_events(/*show_raw_agent_reasoning*/ false) {
            thread
                .session
                .send_event_raw(Event {
                    id: turn_id.clone(),
                    msg: legacy,
                })
                .await;
        }
        Ok(())
    }

    pub(crate) async fn send_inter_agent_completion_communication(
        &self,
        parent_thread_id: ThreadId,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        presentation: &AgentTerminalPresentation,
        admission: CompletionSubmissionAdmission,
    ) -> CodexResult<(String, Arc<CodexThread>)> {
        if presentation.parent().thread_id != parent_thread_id {
            return Err(CodexErr::InvalidRequest(
                "completion destination does not match its presentation parent".to_string(),
            ));
        }
        let state = self.upgrade()?;
        if communication.trigger_turn {
            let parent_thread = state.get_thread(parent_thread_id).await?;
            self.ensure_execution_capacity_for_turn_start(&parent_thread)
                .await?;
        }
        let Some(completion_context_response_item_id) = communication
            .id
            .as_ref()
            .filter(|id| is_sub_agent_completion_context_response_item_id(id.as_str()))
        else {
            return Err(CodexErr::InvalidRequest(
                "completion communication requires a reserved response item ID".to_string(),
            ));
        };
        if completion_context_response_item_id.as_str()
            != presentation.completion_context_response_item_id().as_str()
        {
            return Err(CodexErr::InvalidRequest(
                "completion communication response item ID does not match its presentation"
                    .to_string(),
            ));
        }
        loop {
            let parent_thread = state.get_thread(parent_thread_id).await?;
            if parent_thread.session.presentation_id() != presentation.parent() {
                return Err(CodexErr::ThreadNotFound(parent_thread_id));
            }
            let admitted = match admission {
                CompletionSubmissionAdmission::Ordinary => {
                    parent_thread
                        .session
                        .submission_admission
                        .wait_for_completion_submission()
                        .await
                }
                CompletionSubmissionAdmission::Accepted => {
                    parent_thread
                        .session
                        .submission_admission
                        .wait_for_accepted_completion_submission()
                        .await
                }
            };
            if !admitted {
                return Err(CodexErr::InvalidRequest(
                    "completion destination is no longer accepting work".to_string(),
                ));
            }
            let result = self
                .submit_inter_agent_communication_retaining_thread(
                    parent_thread_id,
                    &state,
                    communication.clone(),
                    context.clone(),
                    InterAgentSubmission::Completion {
                        presentation,
                        admission,
                    },
                )
                .await;
            let retry =
                if result.as_ref().err().is_some_and(|err| {
                    matches!(err.details(), CodexErrorDetails::InvalidRequest(_))
                }) {
                    match admission {
                        CompletionSubmissionAdmission::Ordinary => {
                            parent_thread
                                .session
                                .submission_admission
                                .wait_for_completion_submission()
                                .await
                        }
                        CompletionSubmissionAdmission::Accepted => {
                            parent_thread
                                .session
                                .submission_admission
                                .wait_for_accepted_completion_submission()
                                .await
                        }
                    }
                } else {
                    false
                };
            if retry {
                continue;
            }
            return result;
        }
    }

    async fn send_inter_agent_communication_after_capacity_check(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        self.submit_inter_agent_communication(
            agent_id,
            state,
            communication,
            context,
            start_options,
        )
        .await
    }

    async fn submit_inter_agent_communication(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        self.submit_inter_agent_communication_retaining_thread(
            agent_id,
            state,
            communication,
            context,
            InterAgentSubmission::Ordinary { start_options },
        )
        .await
        .map(|(submission_id, _thread)| submission_id)
    }

    async fn submit_inter_agent_communication_retaining_thread(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        submission: InterAgentSubmission<'_>,
    ) -> CodexResult<(String, Arc<CodexThread>)> {
        let submission_semaphore = self.state.mailbox_submission_semaphore(agent_id);
        let _submission_permit = submission_semaphore.acquire_owned().await.map_err(|err| {
            CodexErr::Fatal(format!("mailbox submission semaphore closed: {err}"))
        })?;
        let last_task_message = context
            .updates_last_task_message()
            .then(|| non_empty_task_message(communication.content.clone()));
        let communication_for_log =
            crate::agent_communication::logging_enabled().then(|| communication.clone());
        let thread = state.get_thread(agent_id).await?;
        if let InterAgentSubmission::Completion { presentation, .. } = &submission
            && thread.session.presentation_id() != presentation.parent()
        {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        let mut authorization_guard = CompletionContextAuthorizationGuard {
            control: self.clone(),
            response_item_id: match &submission {
                InterAgentSubmission::Ordinary { .. } => None,
                InterAgentSubmission::Completion { presentation, .. } => {
                    Some(presentation.completion_context_response_item_id())
                }
            },
            authorized: false,
            committed: false,
        };
        if let InterAgentSubmission::Completion { presentation, .. } = &submission {
            authorization_guard
                .control
                .authorize_pending_completion_context(
                    thread.session.presentation_id(),
                    presentation,
                );
            authorization_guard.authorized = true;
        }
        let send_result = match submission {
            InterAgentSubmission::Ordinary { mut start_options } => {
                if !communication.trigger_turn {
                    start_options.parent_turn_id = None;
                    start_options.root_turn_id = None;
                }
                let parent_turn_id = start_options.parent_turn_id.clone();
                let root_turn_id = start_options.root_turn_id.clone();
                state
                    .send_op_to_thread(
                        &thread,
                        Op::InterAgentCommunication {
                            communication,
                            start_options,
                        },
                        parent_turn_id,
                        root_turn_id,
                    )
                    .await
            }
            InterAgentSubmission::Completion {
                admission: CompletionSubmissionAdmission::Ordinary,
                ..
            } => {
                state
                    .send_op_to_thread(
                        &thread,
                        Op::InterAgentCommunication {
                            communication,
                            start_options: TurnStartOptions::default(),
                        },
                        /*parent_turn_id*/ None,
                        /*root_turn_id*/ None,
                    )
                    .await
            }
            InterAgentSubmission::Completion {
                admission: CompletionSubmissionAdmission::Accepted,
                ..
            } => {
                state
                    .send_accepted_completion_to_thread(&thread, communication)
                    .await
            }
        };
        if send_result.is_ok() {
            authorization_guard.committed = true;
        }
        let result = self
            .handle_thread_request_result(state, &thread, send_result)
            .await;
        if let (Some(communication), Ok(communication_id)) =
            (communication_for_log, result.as_ref())
        {
            crate::agent_communication::emit_agent_communication_send(
                communication_id,
                &context,
                &communication,
                agent_id,
            );
        }
        if result.is_ok()
            && let Some(last_task_message) = last_task_message
        {
            match last_task_message {
                Some(last_task_message) => self
                    .state
                    .update_last_task_message(agent_id, last_task_message),
                None => self.state.clear_last_task_message(agent_id),
            }
        }
        result.map(|submission_id| (submission_id, thread))
    }

    /// Interrupt the current task for an existing agent thread.
    pub(crate) async fn interrupt_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        let send_result = state
            .send_op_to_thread(
                &thread,
                Op::Interrupt,
                /*parent_turn_id*/ None,
                /*root_turn_id*/ None,
            )
            .await;
        self.handle_thread_request_result(&state, &thread, send_result)
            .await
    }

    async fn handle_thread_request_result<T>(
        &self,
        state: &Arc<ThreadManagerState>,
        thread: &Arc<CodexThread>,
        result: CodexResult<T>,
    ) -> CodexResult<T> {
        if result
            .as_ref()
            .is_err_and(|err| matches!(err.details(), CodexErrorDetails::InternalAgentDied))
        {
            let child = thread.session.presentation_id();
            let _ = state
                .remove_thread_if_current(thread, || {
                    self.forget_v2_residency(child.thread_id);
                    self.release_spawned_thread(SpawnedThreadRelease::Session(child));
                })
                .await;
        }
        result
    }

    /// Fetch the last known status for `agent_id`, returning `NotFound` when unavailable.
    pub(crate) async fn get_status(&self, agent_id: ThreadId) -> AgentStatus {
        let Ok(state) = self.upgrade() else {
            // No agent available if upgrade fails.
            return AgentStatus::NotFound;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return AgentStatus::NotFound;
        };
        thread.agent_status().await
    }

    pub(crate) fn register_session_root(
        &self,
        current_thread_id: ThreadId,
        current_parent_thread_id: Option<ThreadId>,
    ) {
        if current_parent_thread_id.is_none() {
            self.state.register_root_thread(current_thread_id);
        }
    }

    pub(crate) fn get_agent_metadata(&self, agent_id: ThreadId) -> Option<AgentMetadata> {
        self.state.agent_metadata_for_thread(agent_id)
    }

    pub(crate) fn ensure_agent_known(&self, agent_id: ThreadId) -> CodexResult<AgentMetadata> {
        self.state
            .agent_metadata_for_thread(agent_id)
            .ok_or_else(|| CodexErr::ThreadNotFound(agent_id))
    }

    pub(crate) async fn list_live_agent_subtree_thread_ids(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut thread_ids = vec![agent_id];
        thread_ids.extend(self.live_thread_spawn_descendants(agent_id).await?);
        Ok(thread_ids)
    }

    pub(crate) async fn get_agent_config_snapshot(
        &self,
        agent_id: ThreadId,
    ) -> Option<ThreadConfigSnapshot> {
        let Ok(state) = self.upgrade() else {
            return None;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return None;
        };
        Some(thread.config_snapshot().await)
    }

    pub(crate) async fn resolve_agent_reference(
        &self,
        _current_thread_id: ThreadId,
        current_session_source: &SessionSource,
        agent_reference: &str,
    ) -> CodexResult<ThreadId> {
        let current_agent_path = current_session_source
            .get_agent_path()
            .unwrap_or_else(AgentPath::root);
        let agent_path = current_agent_path
            .resolve(agent_reference)
            .map_err(CodexErr::UnsupportedOperation)?;
        if let Some(thread_id) = self.state.agent_id_for_path(&agent_path) {
            return Ok(thread_id);
        }
        Err(CodexErr::UnsupportedOperation(format!(
            "live agent path `{}` not found",
            agent_path.as_str()
        )))
    }

    /// Subscribe to status updates for `agent_id`, yielding the latest value and changes.
    pub(crate) async fn subscribe_status(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<watch::Receiver<AgentStatus>> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        Ok(thread.subscribe_status())
    }

    /// Subscribe to every future terminal transition with an atomic current-status snapshot.
    pub(crate) async fn subscribe_terminal_status(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<(AgentStatus, crate::session::TerminalStatusSubscription)> {
        let state = self.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        Ok(thread.session.subscribe_terminal_status())
    }

    pub(crate) async fn format_environment_context_subagents(
        &self,
        parent_thread_id: ThreadId,
    ) -> String {
        let Ok(agents) = self.open_thread_spawn_children(parent_thread_id).await else {
            return String::new();
        };

        agents
            .into_iter()
            .map(|(thread_id, metadata)| {
                let reference = metadata
                    .agent_path
                    .as_ref()
                    .map(|agent_path| agent_path.name().to_string())
                    .unwrap_or_else(|| thread_id.to_string());
                format_subagent_context_line(reference.as_str(), metadata.agent_nickname.as_deref())
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(crate) async fn list_agents(
        &self,
        current_session_source: &SessionSource,
        path_prefix: Option<&str>,
    ) -> CodexResult<Vec<ListedAgent>> {
        let state = self.upgrade()?;
        let resolved_prefix = path_prefix
            .map(|prefix| {
                current_session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(prefix)
                    .map_err(CodexErr::UnsupportedOperation)
            })
            .transpose()?;

        let mut live_agents = self.state.live_agents();
        live_agents.sort_by(|left, right| {
            left.agent_path
                .as_deref()
                .unwrap_or_default()
                .cmp(right.agent_path.as_deref().unwrap_or_default())
                .then_with(|| {
                    left.agent_id
                        .map(|id| id.to_string())
                        .unwrap_or_default()
                        .cmp(&right.agent_id.map(|id| id.to_string()).unwrap_or_default())
                })
        });

        let root_path = AgentPath::root();
        let mut agents = Vec::with_capacity(live_agents.len().saturating_add(1));
        if resolved_prefix
            .as_ref()
            .is_none_or(|prefix| agent_matches_prefix(Some(&root_path), prefix))
            && let Some(root_thread_id) = self.state.agent_id_for_path(&root_path)
            && let Ok(root_thread) = state.get_thread(root_thread_id).await
        {
            agents.push(ListedAgent {
                agent_name: root_path.to_string(),
                agent_status: root_thread.agent_status().await,
                last_task_message: Some(ROOT_LAST_TASK_MESSAGE.to_string()),
            });
        }

        for metadata in live_agents {
            let Some(thread_id) = metadata.agent_id else {
                continue;
            };
            if resolved_prefix
                .as_ref()
                .is_some_and(|prefix| !agent_matches_prefix(metadata.agent_path.as_ref(), prefix))
            {
                continue;
            }

            let Ok(thread) = state.get_thread(thread_id).await else {
                continue;
            };
            let agent_name = metadata
                .agent_path
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| thread_id.to_string());
            let last_task_message = metadata.last_task_message.clone();
            agents.push(ListedAgent {
                agent_name,
                agent_status: thread.agent_status().await,
                last_task_message,
            });
        }

        Ok(agents)
    }

    /// Starts a detached watcher for sub-agents spawned from another thread.
    ///
    /// This is only enabled for `SubAgentSource::ThreadSpawn`, where a parent thread exists and
    /// can receive completion notifications.
    async fn maybe_start_completion_watcher(
        &self,
        child_thread: &Arc<CodexThread>,
        session_source: Option<SessionSource>,
        child_reference: String,
        child_agent_path: Option<AgentPath>,
        child_multi_agent_version: MultiAgentVersion,
    ) {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return;
        };
        let Ok(state) = self.upgrade() else {
            return;
        };
        let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
            return;
        };
        let parent = parent_thread.session.presentation_id();
        let child = child_thread.session.presentation_id();
        let child_thread_id = child.thread_id;
        let Some(watcher_registration) = self.register_completion_watcher_with_admission(
            child,
            parent,
            &parent_thread.session.submission_admission,
        ) else {
            return;
        };
        let mut status_rx = child_thread.subscribe_status();
        let child_rollout_thread_trace = child_thread.session.services.rollout_thread_trace.clone();
        let control = self.clone();
        tokio::spawn(async move {
            let _watcher_registration = watcher_registration;
            loop {
                let terminal = match control.take_watcher_terminal_presentation(child) {
                    Some(terminal) => terminal,
                    None => {
                        if status_rx.changed().await.is_ok() {
                            continue;
                        }
                        if let Some(terminal) = control.take_watcher_terminal_presentation(child) {
                            terminal
                        } else {
                            return;
                        }
                    }
                };
                let status = terminal.status.clone();
                if child_multi_agent_version == MultiAgentVersion::V2
                    && let Some(child_agent_path) = child_agent_path.clone()
                {
                    let accepted_completion_delivery =
                        terminal.presentation.take_accepted_completion_delivery();
                    let admission = if accepted_completion_delivery.is_some() {
                        CompletionSubmissionAdmission::Accepted
                    } else {
                        CompletionSubmissionAdmission::Ordinary
                    };
                    let Some(parent_agent_path) = child_agent_path
                        .as_str()
                        .rsplit_once('/')
                        .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
                    else {
                        return;
                    };
                    let Some(message) = format_inter_agent_completion_message(
                        parent_agent_path.clone(),
                        child_agent_path.clone(),
                        &status,
                    ) else {
                        return;
                    };
                    let trace_message = child_rollout_thread_trace
                        .is_enabled()
                        .then(|| message.clone());
                    let mut communication = InterAgentCommunication::new(
                        child_agent_path,
                        parent_agent_path,
                        Vec::new(),
                        message,
                        /*trigger_turn*/ false,
                    );
                    communication.id =
                        Some(terminal.presentation.completion_context_response_item_id());
                    let context = AgentCommunicationContext::new(
                        AgentCommunicationKind::Result,
                        child_thread_id,
                    );
                    let completion_communication = communication.clone();
                    let Ok((_submission_id, parent_thread)) = control
                        .send_inter_agent_completion_communication(
                            parent_thread_id,
                            communication,
                            context,
                            &terminal.presentation,
                            admission,
                        )
                        .await
                    else {
                        return;
                    };
                    if !parent_thread
                        .persist_inter_agent_completion_context_without_turn(
                            completion_communication,
                        )
                        .await
                    {
                        return;
                    }
                    if !terminal.presentation.wait_owns_presentation().await {
                        match accepted_completion_delivery {
                            Some(completion_delivery) => {
                                parent_thread
                                    .emit_accepted_sub_agent_completion_without_turn(
                                        &child_reference,
                                        &status,
                                        completion_delivery,
                                    )
                                    .await;
                            }
                            None => {
                                parent_thread
                                    .emit_sub_agent_completion_without_turn(
                                        &child_reference,
                                        &status,
                                    )
                                    .await;
                            }
                        }
                    }
                    if let Some(message) = trace_message {
                        child_rollout_thread_trace.record_agent_result_interaction(
                            &terminal.turn_id,
                            parent_thread_id,
                            &AgentResultTracePayload {
                                child_agent_path: child_reference.as_str(),
                                message: &message,
                                status: &status,
                            },
                        );
                    }
                } else if !control
                    .deliver_v1_watcher_terminal(
                        parent_thread_id,
                        child_reference.as_str(),
                        &terminal,
                    )
                    .await
                {
                    return;
                }
                if matches!(status, AgentStatus::Shutdown | AgentStatus::NotFound) {
                    control.finish_watcher_terminal_presentation(child, &terminal.turn_id);
                    return;
                }
            }
        });
    }

    async fn deliver_v1_watcher_terminal(
        &self,
        parent_thread_id: ThreadId,
        child_reference: &str,
        terminal: &WatcherTerminalPresentation,
    ) -> bool {
        let accepted_completion_delivery =
            terminal.presentation.take_accepted_completion_delivery();
        let admission = if accepted_completion_delivery.is_some() {
            CompletionSubmissionAdmission::Accepted
        } else {
            CompletionSubmissionAdmission::Ordinary
        };
        let Ok(state) = self.upgrade() else {
            return false;
        };
        let message = format_subagent_notification_message(child_reference, &terminal.status);
        let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
            return false;
        };
        if parent_thread.session.presentation_id() != terminal.presentation.parent() {
            return false;
        }
        if !parent_thread
            .persist_sub_agent_notification_without_turn(message, admission)
            .await
        {
            return false;
        }
        if !terminal.presentation.wait_owns_presentation().await {
            match accepted_completion_delivery {
                Some(completion_delivery) => {
                    parent_thread
                        .emit_accepted_sub_agent_completion_without_turn(
                            child_reference,
                            &terminal.status,
                            completion_delivery,
                        )
                        .await;
                }
                None => {
                    parent_thread
                        .emit_sub_agent_completion_without_turn(child_reference, &terminal.status)
                        .await;
                }
            }
        }
        true
    }

    /// Ensures an explicitly adopted live v1 thread reports terminal status to its caller.
    ///
    /// A thread can already be live because another client resumed its rollout directly through
    /// the app-server. V1 tools address live threads through the global thread manager, so direct
    /// control already works in that case, but the caller's session-scoped presentation state
    /// still needs a completion watcher. Registering is idempotent for the child presentation.
    pub(crate) async fn ensure_v1_completion_watcher(
        &self,
        child_thread_id: ThreadId,
        session_source: SessionSource,
    ) -> CodexResult<()> {
        let state = self.upgrade()?;
        let child_thread = state.get_thread(child_thread_id).await?;
        if child_thread.multi_agent_version() == Some(MultiAgentVersion::V2) {
            return Ok(());
        }
        let (initial_status, mut terminal_status_rx) =
            child_thread.session.subscribe_terminal_status();
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        }) = &session_source
        else {
            return Ok(());
        };
        let parent_thread_id = *parent_thread_id;
        if child_thread_id == parent_thread_id {
            return Ok(());
        }
        let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
            return Ok(());
        };
        let child_reference = self
            .get_agent_metadata(child_thread_id)
            .and_then(|metadata| metadata.agent_path)
            .map_or_else(|| child_thread_id.to_string(), |path| path.to_string());
        let child_uses_this_parent_presentation = matches!(
            &child_thread.session_source,
            SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
                parent_thread_id: source_parent_thread_id,
                ..
            }) if *source_parent_thread_id == parent_thread_id
        );
        if child_uses_this_parent_presentation
            && Arc::ptr_eq(
                &self.wait_agent_presentations,
                &child_thread
                    .session
                    .services
                    .agent_control
                    .wait_agent_presentations,
            )
        {
            self.maybe_start_completion_watcher(
                &child_thread,
                Some(session_source),
                child_reference,
                /*child_agent_path*/ None,
                MultiAgentVersion::V1,
            )
            .await;
            return Ok(());
        }
        let parent = parent_thread.session.presentation_id();
        let child = child_thread.session.presentation_id();
        let Some(watcher_registration) = self.register_completion_watcher_with_admission(
            child,
            parent,
            &parent_thread.session.submission_admission,
        ) else {
            return Ok(());
        };
        let control = self.clone();
        tokio::spawn(async move {
            let _watcher_registration = watcher_registration;
            let mut initial_status = Some(initial_status);
            loop {
                let status = match terminal_status_rx.try_recv() {
                    Ok(status) => {
                        initial_status = None;
                        status
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {
                        match initial_status.take().filter(is_final) {
                            Some(status) => status,
                            None => match terminal_status_rx.recv().await {
                                Some(status) => status,
                                None => return,
                            },
                        }
                    }
                    Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                        let Some(status) = initial_status.take().filter(is_final) else {
                            return;
                        };
                        status
                    }
                };
                let turn_id = uuid::Uuid::now_v7().to_string();
                let _ = control.record_agent_terminal_presentation(
                    parent,
                    child,
                    &turn_id,
                    status.clone(),
                    TerminalPresentationDelivery::Watcher,
                    || {},
                );
                let Some(terminal) = control.take_watcher_terminal_presentation(child) else {
                    continue;
                };
                if !control
                    .deliver_v1_watcher_terminal(
                        parent_thread_id,
                        child_reference.as_str(),
                        &terminal,
                    )
                    .await
                {
                    return;
                }
                if matches!(status, AgentStatus::Shutdown | AgentStatus::NotFound) {
                    control.finish_watcher_terminal_presentation(child, &terminal.turn_id);
                    return;
                }
            }
        });
        Ok(())
    }

    fn prepare_agent_metadata(
        &self,
        reservation: &mut crate::agent::registry::SpawnReservation,
        config: &Config,
        agent_path: Option<AgentPath>,
        agent_role: Option<String>,
        preferred_agent_nickname: Option<String>,
    ) -> CodexResult<AgentMetadata> {
        if let Some(agent_path) = agent_path.as_ref() {
            reservation.reserve_agent_path(agent_path)?;
        }
        let candidate_names = spawn::agent_nickname_candidates(config, agent_role.as_deref());
        let candidate_name_refs: Vec<&str> = candidate_names.iter().map(String::as_str).collect();
        let agent_nickname = Some(reservation.reserve_agent_nickname_with_preference(
            &candidate_name_refs,
            preferred_agent_nickname.as_deref(),
        )?);
        Ok(AgentMetadata {
            agent_id: None,
            agent_path,
            agent_nickname,
            agent_role,
            last_task_message: None,
        })
    }

    fn prepare_restored_agent_metadata_exact(
        &self,
        reservation: &mut crate::agent::registry::SpawnReservation,
        agent_path: Option<AgentPath>,
        agent_role: Option<String>,
        agent_nickname: Option<String>,
    ) -> CodexResult<AgentMetadata> {
        if let Some(agent_path) = agent_path.as_ref() {
            reservation.reserve_agent_path(agent_path)?;
        }
        if let Some(agent_nickname) = agent_nickname.as_deref() {
            reservation.reserve_agent_nickname_with_preference(&[], Some(agent_nickname))?;
        }
        Ok(AgentMetadata {
            agent_path,
            agent_nickname,
            agent_role,
            ..Default::default()
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_thread_spawn(
        &self,
        reservation: &mut crate::agent::registry::SpawnReservation,
        config: &Config,
        parent_thread_id: ThreadId,
        depth: i32,
        agent_path: Option<AgentPath>,
        agent_role: Option<String>,
        preferred_agent_nickname: Option<String>,
    ) -> CodexResult<(SessionSource, AgentMetadata)> {
        if depth == 1 {
            self.state.register_root_thread(parent_thread_id);
        }
        let agent_metadata = self.prepare_agent_metadata(
            reservation,
            config,
            agent_path,
            agent_role,
            preferred_agent_nickname,
        )?;
        let session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            depth,
            agent_path: agent_metadata.agent_path.clone(),
            agent_nickname: agent_metadata.agent_nickname.clone(),
            agent_role: agent_metadata.agent_role.clone(),
        });
        Ok((session_source, agent_metadata))
    }

    fn upgrade(&self) -> CodexResult<Arc<ThreadManagerState>> {
        self.manager
            .upgrade()
            .ok_or_else(|| CodexErr::UnsupportedOperation("thread manager dropped".to_string()))
    }

    async fn inherited_environments_for_source(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: Option<&SessionSource>,
    ) -> Option<TurnEnvironmentSnapshot> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };

        let parent_thread = state.get_thread(*parent_thread_id).await.ok()?;
        Some(
            parent_thread
                .session
                .services
                .turn_environments
                .snapshot()
                .await,
        )
    }

    async fn inherited_exec_policy_for_source(
        &self,
        state: &Arc<ThreadManagerState>,
        session_source: Option<&SessionSource>,
        child_config: &Config,
    ) -> Option<Arc<crate::exec_policy::ExecPolicyManager>> {
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return None;
        };

        let parent_thread = state.get_thread(*parent_thread_id).await.ok()?;
        let parent_config = parent_thread.session.get_config().await;
        if !crate::exec_policy::child_uses_parent_exec_policy(&parent_config, child_config) {
            return None;
        }

        Some(Arc::clone(&parent_thread.session.services.exec_policy))
    }

    async fn open_thread_spawn_children(
        &self,
        parent_thread_id: ThreadId,
    ) -> CodexResult<Vec<(ThreadId, AgentMetadata)>> {
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        Ok(children_by_parent
            .remove(&parent_thread_id)
            .unwrap_or_default())
    }

    async fn live_thread_spawn_children(
        &self,
    ) -> CodexResult<HashMap<ThreadId, Vec<(ThreadId, AgentMetadata)>>> {
        let state = self.upgrade()?;
        let mut children_by_parent = HashMap::<ThreadId, Vec<(ThreadId, AgentMetadata)>>::new();

        for (parent_thread_id, child_thread_id) in state.list_live_thread_spawn_edges().await {
            children_by_parent
                .entry(parent_thread_id)
                .or_default()
                .push((
                    child_thread_id,
                    self.state
                        .agent_metadata_for_thread(child_thread_id)
                        .unwrap_or(AgentMetadata {
                            agent_id: Some(child_thread_id),
                            ..Default::default()
                        }),
                ));
        }

        for children in children_by_parent.values_mut() {
            children.sort_by(|left, right| {
                left.1
                    .agent_path
                    .as_deref()
                    .unwrap_or_default()
                    .cmp(right.1.agent_path.as_deref().unwrap_or_default())
                    .then_with(|| left.0.to_string().cmp(&right.0.to_string()))
            });
        }

        Ok(children_by_parent)
    }

    async fn persist_thread_spawn_edge_for_source(
        &self,
        child_thread: &crate::CodexThread,
        child_thread_id: ThreadId,
        session_source: Option<&SessionSource>,
    ) {
        let Some(parent_thread_id) = session_source.and_then(SessionSource::parent_thread_id)
        else {
            return;
        };
        if child_thread.config_snapshot().await.ephemeral {
            return;
        }
        let Ok(state) = self.upgrade() else {
            return;
        };
        let Some(agent_graph_store) = state.agent_graph_store() else {
            return;
        };
        if let Err(err) = agent_graph_store
            .upsert_thread_spawn_edge(
                parent_thread_id,
                child_thread_id,
                codex_agent_graph_store::ThreadSpawnEdgeStatus::Open,
            )
            .await
        {
            warn!("failed to persist thread-spawn edge: {err}");
        }
    }

    async fn live_thread_spawn_descendants(
        &self,
        root_thread_id: ThreadId,
    ) -> CodexResult<Vec<ThreadId>> {
        let mut children_by_parent = self.live_thread_spawn_children().await?;
        let mut descendants = Vec::new();
        let mut stack = children_by_parent
            .remove(&root_thread_id)
            .unwrap_or_default()
            .into_iter()
            .map(|(child_thread_id, _)| child_thread_id)
            .rev()
            .collect::<Vec<_>>();

        while let Some(thread_id) = stack.pop() {
            descendants.push(thread_id);
            if let Some(children) = children_by_parent.remove(&thread_id) {
                for (child_thread_id, _) in children.into_iter().rev() {
                    stack.push(child_thread_id);
                }
            }
        }

        Ok(descendants)
    }
}

fn agent_matches_prefix(agent_path: Option<&AgentPath>, prefix: &AgentPath) -> bool {
    if prefix.is_root() {
        return true;
    }

    agent_path.is_some_and(|agent_path| {
        agent_path == prefix
            || agent_path
                .as_str()
                .strip_prefix(prefix.as_str())
                .is_some_and(|suffix| suffix.starts_with('/'))
    })
}

pub(crate) fn render_input_preview(input: &[UserInput]) -> String {
    input
        .iter()
        .map(|item| match item {
            UserInput::Text { text, .. } => text.clone(),
            UserInput::Image { .. } => "[image]".to_string(),
            UserInput::LocalImage { path, .. } => {
                format!("[local_image:{}]", path.display())
            }
            UserInput::Audio { .. } => "[audio]".to_string(),
            UserInput::LocalAudio { path } => {
                format!("[local_audio:{}]", path.display())
            }
            UserInput::Skill { name, path, .. } => {
                format!("[skill:${name}]({})", path.display())
            }
            UserInput::Mention { name, path, .. } => format!("[mention:${name}]({path})"),
            _ => "[input]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn non_empty_task_message(message: String) -> Option<String> {
    (!message.is_empty()).then_some(message)
}

fn thread_spawn_depth(session_source: &SessionSource) -> Option<i32> {
    match session_source {
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn { depth, .. }) => Some(*depth),
        _ => None,
    }
}
#[cfg(test)]
#[path = "control_tests.rs"]
mod tests;
