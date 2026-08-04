use crate::TurnInputRequest;
use crate::TurnInputSubmission;
use crate::TurnStartOptions;
use crate::agent::AgentStatus;
use crate::agent::registry::AgentMetadata;
use crate::agent::registry::AgentRegistry;
use crate::agent::response_observation::FinalResponseObservation;
use crate::agent::response_observation::ResponseObservationPolicy;
use crate::agent::role::DEFAULT_ROLE_NAME;
use crate::agent::role::resolve_role_config;
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
use codex_protocol::protocol::SubAgentCompletionModelVisibility;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadSource;
use codex_protocol::protocol::TurnEnvironmentSelection;
use codex_protocol::protocol::is_sub_agent_completion_context_response_item_id;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use codex_protocol::turn_input::CyberAccessProgram;
use codex_protocol::turn_input::TurnInputMode;
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
use self::presentation::ResponseObservationBinding;
use self::presentation::ResponseObservationBindingPublication;
pub(crate) use self::presentation::ResponseObservationDeliveryCommit;
use self::presentation::ResponseObservationDeliveryKind;
use self::presentation::ResponseObservationPersistence;
pub(crate) use self::presentation::SessionPresentationId;
use self::presentation::SpawnedThreadRelease;
pub(crate) use self::presentation::TerminalPresentationDelivery;
use self::presentation::WaitAgentPresentations;
use self::residency::V2Residency;
use self::response_delivery::WatcherTerminalPoll;

const ROOT_LAST_TASK_MESSAGE: &str = "Main thread";

enum InitialTerminalObservation {
    FutureTurnsOnly,
    ReconcileIfAdvancedFrom(AgentStatus),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThreadCreatedPublication {
    Immediate,
    Deferred,
}

struct InitialTerminalReconciliation {
    terminal: Option<(String, AgentStatus)>,
    status: AgentStatus,
}

struct DurableResponseDelivery {
    commit: ResponseObservationDeliveryCommit,
    submission_permit: tokio::sync::OwnedSemaphorePermit,
    target_lifecycle_guard: tokio::sync::OwnedMutexGuard<()>,
}

impl InitialTerminalObservation {
    fn observes_future_turns(&self) -> bool {
        matches!(self, Self::FutureTurnsOnly)
    }

    fn target_turn_id(&self, active_turn_id: Option<String>) -> Option<String> {
        match self {
            Self::FutureTurnsOnly => None,
            Self::ReconcileIfAdvancedFrom(_) => active_turn_id,
        }
    }

    fn reconcile(
        self,
        active_turn_id: Option<String>,
        last_terminal: Option<(String, AgentStatus)>,
        snapshot_status: AgentStatus,
    ) -> InitialTerminalReconciliation {
        match self {
            Self::FutureTurnsOnly => InitialTerminalReconciliation {
                terminal: None,
                status: snapshot_status,
            },
            Self::ReconcileIfAdvancedFrom(previous_status)
                if !crate::agent::status::is_final(&previous_status) =>
            {
                // A final outcome from an older turn can arrive after a newer turn has started.
                // In that state the active turn and current Running status are authoritative;
                // reconciling the historical outcome would make live adoption report the wrong
                // status and enqueue an unsolicited completion for the old turn.
                if active_turn_id.is_some() && !crate::agent::status::is_final(&snapshot_status) {
                    InitialTerminalReconciliation {
                        terminal: None,
                        status: snapshot_status,
                    }
                } else if let Some((turn_id, status)) = last_terminal {
                    InitialTerminalReconciliation {
                        terminal: Some((turn_id, status.clone())),
                        status,
                    }
                } else if crate::agent::status::is_final(&snapshot_status) {
                    // Raw lifecycle events such as ShutdownComplete can make the session final
                    // without publishing a response-stream terminal. Preserve the active turn
                    // identity when one exists so an already-bound one-shot observation can
                    // reconcile that outcome instead of ignoring a synthetic unrelated turn.
                    let turn_id =
                        active_turn_id.unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
                    InitialTerminalReconciliation {
                        terminal: Some((turn_id, snapshot_status.clone())),
                        status: snapshot_status,
                    }
                } else {
                    InitialTerminalReconciliation {
                        terminal: None,
                        status: snapshot_status,
                    }
                }
            }
            Self::ReconcileIfAdvancedFrom(_) => InitialTerminalReconciliation {
                terminal: None,
                status: snapshot_status,
            },
        }
    }
}

mod execution;
mod legacy;
mod presentation;
mod residency;
mod response_delivery;
mod response_observer;
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
    pub(crate) response_observation: ResponseObservationPolicy,
}

enum InterAgentSubmission<'a> {
    Ordinary {
        start_options: TurnStartOptions,
    },
    ObservedResponse {
        parent_turn_id: Option<String>,
        receiver: SessionPresentationId,
    },
    Completion {
        presentation: &'a AgentTerminalPresentation,
        admission: CompletionSubmissionAdmission,
    },
}

fn response_observations_have_work(
    observations: &[codex_protocol::protocol::AgentResponseObservation],
) -> bool {
    observations.iter().any(|observation| {
        observation.pending_commentary
            || !observation.commentary_after_sequences.is_empty()
            || !observation.commentary_admissions.is_empty()
            || observation.commentary_delivery.is_some()
            || observation.final_delivery
                != codex_protocol::protocol::AgentResponseFinalDelivery::None
            || observation.baseline_final_delivery
                != codex_protocol::protocol::AgentResponseFinalDelivery::None
    })
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

    pub(crate) async fn acquire_mailbox_submission_permit(
        &self,
        agent_id: ThreadId,
    ) -> CodexResult<tokio::sync::OwnedSemaphorePermit> {
        self.state
            .mailbox_submission_semaphore(agent_id)
            .acquire_owned()
            .await
            .map_err(|err| CodexErr::Fatal(format!("mailbox submission semaphore closed: {err}")))
    }

    /// Send rich user input items to an existing agent thread.
    #[cfg(test)]
    pub(crate) async fn send_input(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        self.send_input_after_capacity_check(agent_id, &state, input, start_options)
            .await
            .map(|(submission_id, _resolution)| submission_id)
    }

    pub(crate) async fn send_input_observing_response(
        &self,
        agent_id: ThreadId,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
        observer: SessionPresentationId,
        response_observation: ResponseObservationPolicy,
    ) -> CodexResult<String> {
        let state = self.upgrade()?;
        let lifecycle_lock = state.agent_lifecycle_lock(agent_id);
        let _lifecycle_guard = lifecycle_lock.lock_owned().await;
        let _submission_permit = self.acquire_mailbox_submission_permit(agent_id).await?;
        let thread = state.get_thread(agent_id).await?;
        let child_lifecycle_generation = state.agent_lifecycle_generation(agent_id);
        // Observation semantics belong to this V1 caller. A V2 target still publishes the common
        // response stream and must not silently discard the caller's `w` policy.
        let observes_v1_response = agent_id != observer.thread_id;
        let _response_observation_transaction = self
            .acquire_response_observation_transaction(observer)
            .await;
        let admission_id = uuid::Uuid::now_v7();
        let binding = ResponseObservationBinding::ExplicitAdmission(admission_id);
        let child = thread.session.presentation_id();
        let previous_relationship =
            self.response_observation_relationship_snapshot(observer, child);
        self.ensure_v1_response_observer_for_thread(
            &state,
            &thread,
            observer,
            child_lifecycle_generation,
            response_observation,
            /*retain_passive_completion_relationship*/ false,
            binding,
            InitialTerminalObservation::FutureTurnsOnly,
        )
        .await?;
        let send_result = self
            .send_input_to_retained_thread(
                agent_id,
                &state,
                &thread,
                input,
                start_options,
            )
            .await;
        let (submission_id, resolution) = match send_result {
            Ok(result) => result,
            Err(err) => {
                self.restore_response_observation_relationship_snapshot(
                    observer,
                    child,
                    previous_relationship,
                );
                return Err(err);
            }
        };
        if observes_v1_response
            && (response_observation.commentary()
                || response_observation.final_response() != FinalResponseObservation::None)
        {
            self.bind_response_observation_turn_at_sequence(
                observer,
                child,
                &resolution.target_turn_id,
                binding,
                Some((
                    resolution.minimum_event_sequence,
                    resolution.after_item_id.clone(),
                )),
                ResponseObservationBindingPublication::Deferred,
            );
            if !self
                .persist_response_observation_snapshot(observer, child)
                .await
            {
                let message = "failed to persist response observation state";
                self.rollback_response_observation_relationship_locked(
                    observer,
                    child,
                    previous_relationship,
                    Some(resolution.target_turn_id.clone()),
                    message,
                )
                .await?;
                return Err(CodexErr::Fatal(message.to_string()));
            }
            self.publish_response_observation_binding();
        } else if observes_v1_response
            && !self
                .persist_response_observation_updates(
                    observer,
                    self.response_observation_audit_snapshots(
                        observer,
                        child,
                        Some(resolution.target_turn_id.clone()),
                    ),
                )
                .await
        {
            let message = "failed to persist response observation audit state";
            self.rollback_response_observation_relationship_locked(
                observer,
                child,
                previous_relationship,
                Some(resolution.target_turn_id),
                message,
            )
            .await?;
            return Err(CodexErr::Fatal(message.to_string()));
        }
        Ok(submission_id)
    }

    #[cfg(test)]
    async fn send_input_after_capacity_check(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
    ) -> CodexResult<(String, crate::session::InputTurnAdmissionResolution)> {
        let submission_semaphore = self.state.mailbox_submission_semaphore(agent_id);
        let _submission_permit = submission_semaphore.acquire_owned().await.map_err(|err| {
            CodexErr::Fatal(format!("mailbox submission semaphore closed: {err}"))
        })?;
        let thread = state.get_thread(agent_id).await?;
        self.send_input_to_retained_thread(
            agent_id,
            state,
            &thread,
            input,
            start_options,
        )
        .await
    }

    async fn send_input_to_retained_thread(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        thread: &Arc<CodexThread>,
        input: Vec<UserInput>,
        start_options: TurnStartOptions,
    ) -> CodexResult<(String, crate::session::InputTurnAdmissionResolution)> {
        // V1 deliberately forwards task input unchanged. Direct agent-to-agent replies remain
        // explicit, while commentary and final-response observation cover routine return traffic.
        let last_task_message = non_empty_task_message(render_input_preview(&input));
        let send_result = thread
            .io
            .submit_turn_input_with_admission(
                thread.session.as_ref(),
                TurnInputRequest::user_input(input).on_start(start_options),
                TurnInputMode::StartOrSteer,
            )
            .await;
        let result = self
            .handle_thread_request_result(state, thread, send_result)
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
        let lifecycle_lock = state.agent_lifecycle_lock(agent_id);
        let _lifecycle_guard = lifecycle_lock.lock_owned().await;
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

    async fn send_inter_agent_communication_durably(
        &self,
        receiver: SessionPresentationId,
        communication: InterAgentCommunication,
        agent_communication_context: AgentCommunicationContext,
        parent_turn_id: Option<String>,
        delivery: DurableResponseDelivery,
    ) -> CodexResult<(String, Arc<CodexThread>)> {
        let DurableResponseDelivery {
            commit: delivery_commit,
            submission_permit,
            target_lifecycle_guard,
        } = delivery;
        let response_item_id = communication.id.clone().ok_or_else(|| {
            CodexErr::InvalidRequest(
                "durable inter-agent response delivery requires a response item ID".to_string(),
            )
        })?;
        if response_item_id != delivery_commit.response_item_id
            || receiver != delivery_commit.parent
            || delivery_commit.kind != ResponseObservationDeliveryKind::Commentary
        {
            return Err(CodexErr::InvalidRequest(
                "durable inter-agent response delivery does not match its observation claim"
                    .to_string(),
            ));
        }
        let state = self.upgrade()?;
        let thread = state.get_thread(receiver.thread_id).await?;
        if thread.session.presentation_id() != receiver {
            return Err(CodexErr::ThreadNotFound(receiver.thread_id));
        }
        if communication.trigger_turn {
            self.ensure_execution_capacity_for_turn_start(&thread)
                .await?;
        }
        let receipt = thread
            .session
            .register_communication_delivery(delivery_commit);
        let (submission_id, thread) = self
            .submit_inter_agent_communication_with_permit(
                receiver.thread_id,
                &state,
                communication,
                agent_communication_context,
                InterAgentSubmission::ObservedResponse {
                    parent_turn_id,
                    receiver,
                },
                submission_permit,
            )
            .await?;
        // The accepted claim and mailbox enqueue are ordered under the target lifecycle boundary,
        // but parent consumption must not retain that boundary.
        drop(target_lifecycle_guard);
        let delivered = tokio::select! {
            delivered = receipt.recv() => delivered,
            () = thread.io.session_loop_termination.clone() => false,
        };
        if !delivered {
            return Err(CodexErr::InternalAgentDied);
        }
        Ok((submission_id, thread))
    }

    async fn send_inter_agent_completion_communication_durably(
        &self,
        parent_thread_id: ThreadId,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        presentation: &AgentTerminalPresentation,
        admission: CompletionSubmissionAdmission,
        delivery: DurableResponseDelivery,
    ) -> CodexResult<(String, Arc<CodexThread>)> {
        let DurableResponseDelivery {
            commit: delivery_commit,
            submission_permit,
            target_lifecycle_guard,
        } = delivery;
        if presentation.parent().thread_id != parent_thread_id {
            return Err(CodexErr::InvalidRequest(
                "completion destination does not match its presentation parent".to_string(),
            ));
        }
        let response_item_id = communication.id.clone().ok_or_else(|| {
            CodexErr::InvalidRequest(
                "durable completion response delivery requires a response item ID".to_string(),
            )
        })?;
        if response_item_id != delivery_commit.response_item_id
            || presentation.parent() != delivery_commit.parent
            || presentation.child() != delivery_commit.child
            || delivery_commit.kind != ResponseObservationDeliveryKind::Final
        {
            return Err(CodexErr::InvalidRequest(
                "durable completion response delivery does not match its observation claim"
                    .to_string(),
            ));
        }
        let state = self.upgrade()?;
        if !is_sub_agent_completion_context_response_item_id(response_item_id.as_str())
            || response_item_id.as_str()
                != presentation.completion_context_response_item_id().as_str()
        {
            return Err(CodexErr::InvalidRequest(
                "completion communication response item ID does not match its presentation"
                    .to_string(),
            ));
        }
        let parent_thread = state.get_thread(parent_thread_id).await?;
        if parent_thread.session.presentation_id() != presentation.parent() {
            return Err(CodexErr::ThreadNotFound(parent_thread_id));
        }
        if communication.trigger_turn {
            self.ensure_execution_capacity_for_turn_start(&parent_thread)
                .await?;
        }
        let receipt = parent_thread
            .session
            .register_communication_delivery(delivery_commit);
        let (submission_id, parent_thread) = self
            .submit_inter_agent_communication_with_permit(
                parent_thread_id,
                &state,
                communication,
                context,
                InterAgentSubmission::Completion {
                    presentation,
                    admission,
                },
                submission_permit,
            )
            .await?;
        // A close that starts after this enqueue revokes future observation, not the response
        // item that already won admission. Do not retain the child lifecycle boundary while the
        // parent waits to consume that item.
        drop(target_lifecycle_guard);
        let delivered = tokio::select! {
            delivered = receipt.recv() => delivered,
            () = parent_thread.io.session_loop_termination.clone() => false,
        };
        if !delivered {
            return Err(CodexErr::InternalAgentDied);
        }
        Ok((submission_id, parent_thread))
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
        let submission_permit = self.acquire_mailbox_submission_permit(agent_id).await?;
        self.submit_inter_agent_communication_with_permit(
            agent_id,
            state,
            communication,
            context,
            submission,
            submission_permit,
        )
        .await
    }

    async fn submit_inter_agent_communication_with_permit(
        &self,
        agent_id: ThreadId,
        state: &Arc<ThreadManagerState>,
        communication: InterAgentCommunication,
        context: AgentCommunicationContext,
        submission: InterAgentSubmission<'_>,
        _submission_permit: tokio::sync::OwnedSemaphorePermit,
    ) -> CodexResult<(String, Arc<CodexThread>)> {
        let last_task_message = context
            .updates_last_task_message()
            .then(|| non_empty_task_message(communication.content.clone()));
        let communication_for_log =
            crate::agent_communication::logging_enabled().then(|| communication.clone());
        let thread = state.get_thread(agent_id).await?;
        match &submission {
            InterAgentSubmission::Ordinary { .. } => {}
            InterAgentSubmission::ObservedResponse { receiver, .. }
                if thread.session.presentation_id() != *receiver =>
            {
                return Err(CodexErr::ThreadNotFound(agent_id));
            }
            InterAgentSubmission::ObservedResponse { .. } => {}
            InterAgentSubmission::Completion { presentation, .. }
                if thread.session.presentation_id() != presentation.parent() =>
            {
                return Err(CodexErr::ThreadNotFound(agent_id));
            }
            InterAgentSubmission::Completion { .. } => {}
        }
        let mut authorization_guard = CompletionContextAuthorizationGuard {
            control: self.clone(),
            response_item_id: match &submission {
                InterAgentSubmission::Ordinary { .. }
                | InterAgentSubmission::ObservedResponse { .. } => None,
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
            InterAgentSubmission::ObservedResponse { parent_turn_id, .. } => {
                let parent_turn_id = parent_turn_id.filter(|_| communication.trigger_turn);
                state
                    .send_op_to_thread(
                        &thread,
                        Op::InterAgentCommunication { communication },
                        parent_turn_id,
                        /*root_turn_id*/ None,
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
        if result.is_ok() {
            let thread_id = thread.session.thread_id();
            let still_current = state
                .get_thread(thread_id)
                .await
                .is_ok_and(|current| Arc::ptr_eq(&current, thread));
            if !still_current {
                return Err(CodexErr::ThreadNotFound(thread_id));
            }
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

    pub(crate) fn restore_agent_metadata(
        &self,
        agent_id: ThreadId,
        metadata: AgentMetadata,
    ) -> CodexResult<()> {
        self.state
            .reserve_agent_metadata_replacement(agent_id, metadata)?
            .commit()
    }

    pub(crate) fn restore_agent_metadata_if_current(
        &self,
        agent_id: ThreadId,
        expected: &AgentMetadata,
        metadata: AgentMetadata,
    ) -> CodexResult<bool> {
        self.state
            .reserve_agent_metadata_replacement(agent_id, metadata)?
            .commit_if_current(expected)
    }

    pub(crate) fn clear_agent_metadata_if_current(
        &self,
        agent_id: ThreadId,
        expected: &AgentMetadata,
    ) -> bool {
        self.state
            .release_spawned_thread_if_current(agent_id, expected)
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
    ) -> CodexResult<(
        crate::session::TerminalStatusEvent,
        crate::session::TerminalStatusSubscription,
    )> {
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
    #[allow(clippy::too_many_arguments)]
    async fn maybe_start_completion_watcher(
        &self,
        child_thread: &Arc<CodexThread>,
        session_source: Option<SessionSource>,
        child_reference: String,
        child_agent_path: Option<AgentPath>,
        response_observation: ResponseObservationPolicy,
        initial_terminal_observation: InitialTerminalObservation,
    ) -> CodexResult<AgentStatus> {
        if !response_observation.commentary()
            && response_observation.final_response() == FinalResponseObservation::None
        {
            return Ok(child_thread.agent_status().await);
        }
        let Some(SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id, ..
        })) = session_source
        else {
            return Ok(child_thread.agent_status().await);
        };
        let Ok(state) = self.upgrade() else {
            return Ok(child_thread.agent_status().await);
        };
        let Ok(parent_thread) = state.get_thread(parent_thread_id).await else {
            return Ok(child_thread.agent_status().await);
        };
        let observer_multi_agent_version = parent_thread
            .multi_agent_version()
            .unwrap_or(MultiAgentVersion::V1);
        let parent = parent_thread.session.presentation_id();
        let child = child_thread.session.presentation_id();
        let child_thread_id = child.thread_id;
        let (response_snapshot, mut response_rx) = match observer_multi_agent_version {
            MultiAgentVersion::V1 | MultiAgentVersion::Disabled => {
                let terminal_control = self.clone();
                let terminal_lifecycle_generation =
                    self.agent_lifecycle_generation(child_thread_id);
                child_thread
                    .session
                    .subscribe_agent_responses_observing_terminal(move |turn_id, status| {
                        if !terminal_control.agent_lifecycle_generation_is_current(
                            child.thread_id,
                            terminal_lifecycle_generation,
                        ) {
                            return;
                        }
                        let _ = terminal_control.record_agent_terminal_presentation(
                            parent,
                            child,
                            turn_id,
                            status,
                            TerminalPresentationDelivery::Watcher,
                            || {},
                        );
                    })
            }
            // Native V2 completion remains direct. A V1 observer of a V2 target takes the
            // watcher branch above so the caller receives V1 delivery and durability semantics.
            MultiAgentVersion::V2 => child_thread.session.subscribe_agent_responses(),
        };
        let target_turn_id =
            initial_terminal_observation.target_turn_id(response_snapshot.active_turn_id.clone());
        let initial_reconciliation = initial_terminal_observation.reconcile(
            response_snapshot.active_turn_id.clone(),
            response_snapshot.last_terminal.clone(),
            response_snapshot.status.clone(),
        );
        let previous_relationship = self.response_observation_relationship_snapshot(parent, child);
        let retain_passive_completion_relationship =
            observer_multi_agent_version != MultiAgentVersion::V1;
        let watcher_registration = self.register_response_watcher_with_admission_at_sequence(
            child,
            parent,
            &parent_thread.session.submission_admission,
            response_observation,
            retain_passive_completion_relationship,
            target_turn_id.clone(),
            ResponseObservationBinding::NextTurn,
            match observer_multi_agent_version {
                MultiAgentVersion::V1 => ResponseObservationPersistence::Durable,
                MultiAgentVersion::V2 | MultiAgentVersion::Disabled => {
                    ResponseObservationPersistence::RuntimeOnly
                }
            },
            response_snapshot.next_event_sequence,
            response_snapshot.last_commentary_item_id,
        );
        if observer_multi_agent_version == MultiAgentVersion::V1
            && !self
                .persist_response_observation_snapshot(parent, child)
                .await
        {
            drop(watcher_registration);
            let message = "failed to persist initial response observation state";
            self.rollback_response_observation_relationship_locked(
                parent,
                child,
                previous_relationship,
                target_turn_id,
                message,
            )
            .await?;
            return Err(CodexErr::Fatal(message.to_string()));
        }
        let Some(watcher_registration) = watcher_registration else {
            return Ok(initial_reconciliation.status);
        };
        if let Some((turn_id, status)) = initial_reconciliation.terminal {
            let _ = self.record_agent_terminal_presentation(
                parent,
                child,
                &turn_id,
                status,
                TerminalPresentationDelivery::Watcher,
                || {},
            );
        }
        let child_rollout_thread_trace = child_thread.session.services.rollout_thread_trace.clone();
        let control = self.clone();
        tokio::spawn(async move {
            let child_lifecycle_generation = watcher_registration.child_lifecycle_generation();
            let mut watcher_guard = self::response_observer::CompletionWatcherLifecycleGuard::new(
                control.clone(),
                watcher_registration,
                parent,
                child,
            );
            loop {
                let terminal = match control
                    .next_watcher_terminal(
                        parent,
                        child,
                        child_reference.as_str(),
                        &mut response_rx,
                        observer_multi_agent_version,
                        child_lifecycle_generation,
                    )
                    .await
                {
                    WatcherTerminalPoll::Terminal(terminal) => terminal,
                    WatcherTerminalPoll::Retry => {
                        if !control.agent_lifecycle_generation_is_current(
                            child.thread_id,
                            child_lifecycle_generation,
                        ) || !control.response_observer_can_retry(parent).await
                        {
                            return;
                        }
                        while !control
                            .persist_response_observation_snapshot_transactionally(parent, child)
                            .await
                        {
                            if !control.agent_lifecycle_generation_is_current(
                                child.thread_id,
                                child_lifecycle_generation,
                            ) || !control.response_observer_can_retry(parent).await
                            {
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        continue;
                    }
                    WatcherTerminalPoll::Closed => {
                        if observer_multi_agent_version == MultiAgentVersion::V1
                            && control.response_observer_can_retry(parent).await
                        {
                            control.restart_v1_response_observer_after_runtime_end(
                                watcher_guard.take_registration(),
                                parent,
                                child,
                            );
                        }
                        return;
                    }
                };
                let status = terminal.status.clone();
                if observer_multi_agent_version == MultiAgentVersion::V2 {
                    let Some(_lifecycle_guard) = control
                        .acquire_current_agent_lifecycle(
                            child.thread_id,
                            child_lifecycle_generation,
                        )
                        .await
                    else {
                        return;
                    };
                    let Some(child_agent_path) = child_agent_path.clone() else {
                        return;
                    };
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
                                        SubAgentCompletionModelVisibility::Visible,
                                        completion_delivery,
                                    )
                                    .await;
                            }
                            None => {
                                parent_thread
                                    .emit_sub_agent_completion_without_turn(
                                        &child_reference,
                                        &status,
                                        SubAgentCompletionModelVisibility::Visible,
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
                } else {
                    loop {
                        let Some(lifecycle_guard) = control
                            .acquire_current_agent_lifecycle(
                                child.thread_id,
                                child_lifecycle_generation,
                            )
                            .await
                        else {
                            return;
                        };
                        let delivered = control
                            .deliver_v1_watcher_terminal(
                                parent_thread_id,
                                child_reference.as_str(),
                                &terminal,
                                lifecycle_guard,
                            )
                            .await;
                        if delivered {
                            break;
                        }
                        if !control.agent_lifecycle_generation_is_current(
                            child.thread_id,
                            child_lifecycle_generation,
                        ) || !control
                            .terminal_response_observer_can_retry(parent, &terminal.presentation)
                            .await
                        {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
                if observer_multi_agent_version == MultiAgentVersion::V1 {
                    while !control
                        .finish_and_persist_response_observation_turn(
                            parent,
                            child,
                            &terminal.turn_id,
                        )
                        .await
                    {
                        if !control.agent_lifecycle_generation_is_current(
                            child.thread_id,
                            child_lifecycle_generation,
                        ) || !control.response_observer_can_retry(parent).await
                        {
                            return;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    if watcher_guard.retire_if_observation_idle() {
                        return;
                    }
                } else {
                    let _transaction_permit = control
                        .acquire_response_observation_transaction(parent)
                        .await;
                    let removed_bound_wake = control
                        .response_observation_turn_has_bound_final_wake(
                            parent,
                            child,
                            &terminal.turn_id,
                        );
                    control.finish_response_observation_turn(parent, child, &terminal.turn_id);
                    drop(_transaction_permit);
                    if removed_bound_wake {
                        control.recheck_thread_idle_lifecycle(parent).await;
                    }
                }
                if matches!(status, AgentStatus::Shutdown | AgentStatus::NotFound) {
                    control.finish_watcher_terminal_presentation(parent, child, &terminal.turn_id);
                    if observer_multi_agent_version == MultiAgentVersion::V1 {
                        control.restart_v1_response_observer_after_runtime_end(
                            watcher_guard.take_registration(),
                            parent,
                            child,
                        );
                    }
                    return;
                }
            }
        });
        Ok(initial_reconciliation.status)
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

    /// Persist a child edge after its runtime and observation relationship are ready.
    ///
    /// The caller must hold the direct parent's lifecycle guard through this call so a
    /// concurrent subtree close cannot take its final membership snapshot first.
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
