use crate::TurnInputRequest;
use crate::TurnInputSubmission;
use crate::TurnStartOptions;
use crate::agent::AgentStatus;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::resolve_role_config;
use crate::agent::types::AgentMetadata;
use crate::agent::types::LiveAgent;
use crate::agent_communication::AgentCommunicationContext;
use crate::config::Config;
use crate::config::RolloutBudgetConfig;
use crate::environment_selection::TurnEnvironmentSnapshot;
use crate::session::emit_subagent_session_started;
use crate::thread_manager::ResumeThreadWithHistoryOptions;
use crate::thread_manager::ThreadIdGenerator;
use crate::thread_manager::ThreadManagerState;
use crate::thread_manager::default_thread_id_generator;
use crate::thread_rollout_truncation::truncate_rollout_to_last_n_fork_turns;
use crate::turn_timing::now_unix_timestamp_ms;
use codex_history::InitialHistory;
use codex_history::ResumedHistory;
use codex_history::RolloutItem;
use codex_protocol::AgentPath;
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
use codex_protocol::user_input::UserInput;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ReadThreadParams;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Weak;
use tracing::warn;
use uuid::Uuid;

pub(crate) use self::runtime::AgentControlInit;
pub(crate) use self::runtime::LocalAgentRuntime;

mod api;
mod aliases;
mod budget;
pub(crate) use aliases::AgentResumeOwnership;
mod close_response;
mod completion;
mod completion_watcher;
mod input;
mod presentation;
mod response_delivery;
mod response_observer;
mod scoped_messages;
mod turn_queue;
mod wait_commentary;
pub(crate) use close_response::CloseAgentResponseDisposition;
pub(crate) use close_response::ClosedAgent;
pub(crate) use input::AgentControlInput;
pub(crate) use turn_queue::QueuedInputObservationParams;
mod user_dispatch;
mod user_observation;
mod user_resume;
mod user_spawn;
pub(in crate::agent) use presentation::ReplacedFinalResponseObservationBinding;
pub(crate) use user_dispatch::ResponseObservationSubmission;
pub(crate) use user_dispatch::ResumeUserInputAdmission;
mod restore_environments;
mod restore_metadata;
mod restore_publication;
mod restore_v2;
mod resume_registration;
pub(crate) use presentation::AgentTerminalPresentation;
pub(crate) use presentation::CompletionParentAdoption;
pub(crate) use presentation::CompletionParentBinding;
pub(crate) use presentation::CompletionParentState;
pub(crate) use presentation::CompletionPresentation;
pub(crate) use presentation::CompletionWatcherRegistration;
use presentation::ResponseObservationBinding;
use presentation::ResponseObservationBindingPublication;
pub(crate) use presentation::ResponseObservationDeliveryCommit;
pub(crate) use presentation::ResponseObservationDeliveryKind;
use presentation::ResponseObservationPersistence;
pub(crate) use presentation::SessionPresentationId;
pub(crate) use presentation::TargetMessageAdmission;
pub(crate) use presentation::TargetMessageAdmissionMode;
pub(crate) use presentation::TerminalPresentationDelivery;
pub(crate) use presentation::WaitAgentPresentationCommit;
mod delivery;
mod execution;
mod fork_goal_context;
mod fork_notification_context;
mod inspection;
mod interrupt;
mod legacy;
mod residency;
mod resume;
mod resume_role;
mod root_handoff;
mod runtime;
mod runtime_context;
mod sender_context;
mod service_tier;
mod spawn;
mod spawn_guard;
mod spawn_telemetry;
mod target;
mod user_authorization;
mod watch;

/// Per-session controller handle for a local agent tree.
/// Handles retain a session identity and share their tree's `LocalAgentRuntime`.
/// Local startup preserves that state when creating or resuming children.
#[derive(Clone)]
pub(crate) struct LocalAgentControl {
    /// session_id is equal to the root thread's ID.
    session_id: SessionId,
    pub(crate) runtime: LocalAgentRuntime,
    session_id_is_bound: bool,
}

impl Default for LocalAgentControl {
    fn default() -> Self {
        Self::new(
            Weak::default(),
            default_thread_id_generator(),
            /*rollout_budget*/ None,
        )
    }
}

impl LocalAgentControl {
    pub(crate) fn response_observation_changed(&self) -> &tokio::sync::Notify {
        &self
            .runtime
            .wait_agent_presentations
            .response_observation_changed
    }

    /// Construct a new `LocalAgentControl` that can spawn/message agents via the given manager state.
    pub(crate) fn new(
        manager: Weak<ThreadManagerState>,
        thread_id_generator: ThreadIdGenerator,
        rollout_budget: Option<RolloutBudgetConfig>,
    ) -> Self {
        Self {
            session_id: SessionId::default(),
            runtime: LocalAgentRuntime::new(manager, thread_id_generator, rollout_budget),
            session_id_is_bound: false,
        }
    }

    pub(crate) fn with_session_id(mut self, session_id: SessionId, max_threads: usize) -> Self {
        self.session_id = session_id;
        self.runtime.agent_execution_limiter.initialize(max_threads);
        self.session_id_is_bound = true;
        self
    }

    pub(crate) fn session_id(&self) -> SessionId {
        self.session_id
    }

    pub(crate) fn bound_session_id(&self) -> Option<SessionId> {
        self.session_id_is_bound.then_some(self.session_id)
    }

    /// Send rich user input items to an existing agent thread.
    pub(crate) async fn send_input(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let state = self.runtime.upgrade()?;
        let submission = self.runtime.registry.mailbox_submission(agent_id);
        let thread = state.get_thread(agent_id).await?;
        let _submission_permit = Arc::clone(&submission.semaphore)
            .acquire_owned()
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!("mailbox submission semaphore closed: {err}"))
            })?;
        let current_thread = state.get_thread(agent_id).await?;
        if !Arc::ptr_eq(&thread, &current_thread)
            || !self
                .runtime
                .registry
                .submission_is_current(agent_id, &submission)
        {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        let last_task_message = non_empty_task_message(render_input_preview(&input));
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
            .handle_thread_request_result(agent_id, &state, &thread, result)
            .await;
        if result.is_ok() {
            self.runtime.registry.update_last_task_message(
                agent_id,
                &submission,
                last_task_message,
            );
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
        let state = self.runtime.upgrade()?;
        let thread = state.get_thread(agent_id).await?;
        if communication.trigger_turn {
            thread
                .ensure_execution_capacity_for_turn_start(self)
                .await?;
        }
        self.send_inter_agent_communication_after_capacity_check(
            agent_id,
            &state,
            &thread,
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
        let state = self.runtime.upgrade()?;
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

    async fn send_inter_agent_communication_after_capacity_check(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        thread: &Arc<crate::codex_thread::CodexThread>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        self.submit_inter_agent_communication(
            agent_id,
            state,
            thread,
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
        thread: &Arc<crate::codex_thread::CodexThread>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let submission = self.runtime.registry.mailbox_submission(agent_id);
        let _submission_permit = Arc::clone(&submission.semaphore)
            .acquire_owned()
            .await
            .map_err(|err| {
                CodexErr::Fatal(format!("mailbox submission semaphore closed: {err}"))
            })?;
        let current_thread = state.get_thread(agent_id).await?;
        if !Arc::ptr_eq(thread, &current_thread)
            || !self
                .runtime
                .registry
                .submission_is_current(agent_id, &submission)
        {
            return Err(CodexErr::ThreadNotFound(agent_id));
        }
        let last_task_message = context
            .updates_last_task_message()
            .then(|| non_empty_task_message(communication.content.clone()));
        let communication_for_log =
            crate::agent_communication::logging_enabled().then(|| communication.clone());
        let (parent_turn_id, root_turn_id) = if communication.trigger_turn {
            (
                start_options.parent_turn_id.clone(),
                start_options.root_turn_id.clone(),
            )
        } else {
            (None, None)
        };
        let result = self
            .handle_thread_request_result(
                agent_id,
                state,
                thread,
                state
                    .send_op_to_thread(
                        thread,
                        Op::InterAgentCommunication {
                            communication,
                            start_options,
                        },
                        parent_turn_id,
                        root_turn_id,
                    )
                    .await,
            )
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
            self.runtime.registry.update_last_task_message(
                agent_id,
                &submission,
                last_task_message,
            );
        }
        result
    }

    /// Interrupt the current task for an existing agent thread.
    pub(crate) async fn interrupt_agent(&self, agent_id: ThreadId) -> CodexResult<String> {
        let state = self.runtime.upgrade()?;
        // A stopped runtime still needs exact-instance cleanup. The live-only
        // admission helper would return NotFound before that cleanup can run.
        let _lifecycle = state.agent_lifecycle_lock(agent_id).lock_owned().await;
        let thread = state.get_thread(agent_id).await?;
        thread.ensure_not_unloading()?;
        state.check_restoration_fence(agent_id)?;
        self.require_current_agent_ownership(agent_id).await?;
        let result = if thread.is_running() {
            thread.session.submission_admission.check_ready()?;
            state
                .send_op_to_thread(
                    &thread,
                    Op::Interrupt,
                    /*parent_turn_id*/ None,
                    /*root_turn_id*/ None,
                )
                .await
        } else {
            Err(CodexErr::InternalAgentDied)
        };
        self.handle_thread_request_result(agent_id, &state, &thread, result)
            .await
    }

    async fn handle_thread_request_result(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        thread: &Arc<crate::codex_thread::CodexThread>,
        result: CodexResult<String>,
    ) -> CodexResult<String> {
        if result
            .as_ref()
            .is_err_and(|err| matches!(err.details(), CodexErrorDetails::InternalAgentDied))
            && state
                .remove_thread_if_matches(&agent_id, thread)
                .await
                .is_some()
        {
            self.forget_v2_residency(agent_id);
            self.runtime.registry.release_spawned_thread(agent_id);
        }
        result
    }

    /// Fetch the last known status for `agent_id`, returning `NotFound` when unavailable.
    pub(crate) async fn get_status(&self, agent_id: ThreadId) -> AgentStatus {
        let Ok(state) = self.runtime.upgrade() else {
            // No agent available if upgrade fails.
            return AgentStatus::NotFound;
        };
        let Ok(thread) = state.get_thread(agent_id).await else {
            return AgentStatus::NotFound;
        };
        thread.agent_status().await
    }

    pub(crate) fn get_agent_metadata(&self, agent_id: ThreadId) -> Option<AgentMetadata> {
        self.runtime.registry.agent_metadata_for_thread(agent_id)
    }

    pub(crate) async fn list_agents(
        &self,
        current_session_source: &SessionSource,
        path_prefix: Option<&str>,
    ) -> CodexResult<Vec<LiveAgent>> {
        let state = self.runtime.upgrade()?;
        let resolved_prefix = path_prefix
            .map(|prefix| {
                current_session_source
                    .get_agent_path()
                    .unwrap_or_else(AgentPath::root)
                    .resolve(prefix)
                    .map_err(CodexErr::UnsupportedOperation)
            })
            .transpose()?;

        let mut live_agents = self.runtime.registry.live_agents();
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
            && let Some(root_thread_id) = self.runtime.registry.agent_id_for_path(&root_path)
            && let Ok(root_thread) = state.get_thread(root_thread_id).await
        {
            agents.push(LiveAgent {
                thread_id: root_thread_id,
                metadata: AgentMetadata {
                    agent_id: Some(root_thread_id),
                    agent_path: Some(root_path),
                    ..Default::default()
                },
                status: root_thread.agent_status().await,
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
            agents.push(LiveAgent {
                thread_id,
                metadata,
                status: thread.agent_status().await,
            });
        }

        Ok(agents)
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
        if let Some(path) = &agent_path {
            reservation.reserve_agent_path(path)?;
        }
        if let Some(nickname) = agent_nickname.as_deref() {
            reservation.reserve_agent_nickname_with_preference(&[], Some(nickname))?;
        }
        Ok(AgentMetadata {
            agent_path,
            agent_role,
            agent_nickname,
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
            self.runtime.registry.register_root_thread(parent_thread_id);
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
