//! Handles reply-bearing turn-input operations.
//!
//! This is the one place Core decides whether submitted input starts a turn,
//! steers an active turn, or is rejected. It replies after that decision; it
//! does not wait for user-prompt hooks, updating the in-memory model context,
//! rollout persistence, or sampling.
//!
//! Persistent thread settings apply on Started and Steered. Turn start
//! options only apply on Started.
//! Host shutdown admission is checked before reserving or starting a new turn.
//! Parent-delegated subagent input bypasses drain; automatic starts remain gated.
//! Realtime drain refusals are returned to the fanout for ordered session teardown.

use super::TurnInput;
use super::session::Session;
use super::session::SessionConfiguration;
use super::session::SessionSettingsUpdate;
use super::thread_settings;
use super::turn_context::NewTurnContextOptions;
use super::turn_context::TurnContext;
use crate::context::GuardianContextMode;
use crate::state::ActiveTurn;
use crate::state::TurnState;
use crate::tasks::RegularTask;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::config_types::ModeKind;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::AdditionalContextEntry;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::Event;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::NonSteerableTurnKind;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::turn_input::NotSubmittedReason;
use codex_protocol::turn_input::TurnInput as SubmittedTurnInput;
use codex_protocol::turn_input::TurnInputMode;
use codex_protocol::turn_input::TurnInputRequest;
use codex_protocol::turn_input::TurnInputSubmission;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use serde_json::Value;
use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::Arc;
use uuid::Uuid;

#[cfg(test)]
#[path = "turn_input_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "turn_input_admission_tests.rs"]
mod admission_tests;

/// Why input is starting a turn; shared by admission and input delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TurnStartKind {
    User,
    Automatic,
    Recovery,
}

enum IdleContext {
    Ordinary,
    Mailbox(codex_thread_store::MailboxInventoryNotification),
}

/// Identity and eligibility of an idle start, separate from its admission lease.
struct IdleStart {
    submission_id: String,
    kind: TurnStartKind,
    expected_previous_turn_id: Option<String>,
    context: IdleContext,
}

impl TurnStartKind {
    fn permits_mode(self, mode: ModeKind) -> bool {
        match self {
            Self::User | Self::Recovery => true,
            Self::Automatic => mode != ModeKind::Plan,
        }
    }

    /// Automatic work may neither leave an existing Plan mode nor enter it.
    fn permits_settings(
        self,
        current: &SessionConfiguration,
        proposed: &SessionConfiguration,
    ) -> bool {
        self.permits_mode(current.step_settings.collaboration_mode.mode)
            && self.permits_mode(proposed.step_settings.collaboration_mode.mode)
    }
}

/// Thread settings and start-only options prepared before Core knows whether
/// turn input starts or steers.
///
/// Thread settings are validated up front but only applied after Core accepts
/// the input. Start-only options are only consumed by `apply_started`.
struct PreparedTurnInputSettings {
    thread_settings_update: Option<SessionSettingsUpdate>,
    start_options: TurnStartOptions,
}

impl PreparedTurnInputSettings {
    /// Validates turn-input settings without applying them so rejected input
    /// leaves the thread unchanged.
    async fn prepare(
        session: &Session,
        thread_settings: ThreadSettingsOverrides,
        start_options: TurnStartOptions,
    ) -> CodexResult<Self> {
        let thread_settings_update = if thread_settings == ThreadSettingsOverrides::default() {
            None
        } else {
            let updates = thread_settings::prepare_update(thread_settings);
            session
                .preview_settings(&updates)
                .await
                .map_err(|error| CodexErr::InvalidRequest(error.to_string()))?;
            Some(updates)
        };
        Ok(Self {
            thread_settings_update,
            start_options,
        })
    }

    fn required_active_final_output_json_schema(&self) -> Option<&Value> {
        self.start_options.final_output_json_schema.as_ref()
    }

    /// Applies persistent settings and start-only options before creating a
    /// new turn context. Returns `None` if admission rejects the candidate,
    /// without committing its settings.
    async fn apply_started(
        self,
        session: &Arc<Session>,
        submission_id: String,
        kind: TurnStartKind,
    ) -> CodexResult<Option<Arc<TurnContext>>> {
        let TurnStartOptions {
            turn_trigger,
            final_output_json_schema,
            service_tier,
            parent_turn_id,
            root_turn_id,
            cyber_access_program,
        } = self.start_options;
        let emit_thread_settings_applied = self.thread_settings_update.is_some();
        let settings_guard = thread_settings::acquire_persistence_lock(session).await;
        session.check_history_publication()?;
        let mut updates = self.thread_settings_update.unwrap_or_default();
        updates.service_tier_for_turn = service_tier;

        let options = NewTurnContextOptions {
            final_output_json_schema,
            cyber_access_program,
        };
        let turn_context = session
            .new_turn_with_sub_id_if_with_permit(
                submission_id.clone(),
                updates,
                options,
                |current, proposed| {
                    kind != TurnStartKind::Automatic || kind.permits_settings(current, proposed)
                },
                &settings_guard,
            )
            .await?;
        let Some((turn_context, settings_snapshot)) = turn_context else {
            return Ok(None);
        };
        if let Some(turn_trigger) = turn_trigger {
            turn_context
                .turn_metadata_state
                .set_turn_trigger(turn_trigger);
        }
        if emit_thread_settings_applied {
            thread_settings::emit_applied(
                session,
                settings_guard,
                submission_id,
                settings_snapshot,
            )
            .await?;
        } else {
            drop(settings_guard);
        }
        if let Some(parent_turn_id) = parent_turn_id {
            turn_context
                .turn_metadata_state
                .set_parent_turn_id(parent_turn_id);
        }
        if let Some(root_turn_id) = root_turn_id {
            turn_context
                .turn_metadata_state
                .set_root_turn_id(root_turn_id);
        }
        Ok(Some(turn_context))
    }

    /// Applies only persistent settings after steering succeeds. The active
    /// turn keeps its existing context; subsequent turns see the update.
    async fn apply_steered(self, session: &Session, submission_id: String) -> CodexResult<()> {
        let Some(thread_settings_update) = self.thread_settings_update else {
            return Ok(());
        };
        thread_settings::apply_update(session, submission_id, thread_settings_update).await
    }
}

pub(super) async fn handle(
    session: &Arc<Session>,
    request: TurnInputRequest,
    mode: TurnInputMode,
    submission_id: String,
) -> CodexResult<TurnInputSubmission> {
    let result = match mode {
        TurnInputMode::StartOrSteer => {
            start_or_steer(session, request, submission_id.clone()).await
        }
        TurnInputMode::StartIfIdle => {
            let kind = match &request.input {
                SubmittedTurnInput::UserInput { content, .. }
                | SubmittedTurnInput::AgentInput { content, .. }
                    if !content.is_empty() =>
                {
                    TurnStartKind::User
                }
                SubmittedTurnInput::UserInput { .. }
                | SubmittedTurnInput::AgentInput { .. }
                | SubmittedTurnInput::ResponseItem(_)
                | SubmittedTurnInput::InterAgentCommunication(_) => TurnStartKind::Automatic,
            };
            start_if_idle(
                session,
                request,
                submission_id.clone(),
                kind,
                /*expected_previous_turn_id*/ None,
            )
            .await
        }
        TurnInputMode::ContinueIfIdle {
            expected_previous_turn_id,
        } => {
            if !matches!(&request.input, SubmittedTurnInput::ResponseItem(_)) {
                let error = CodexErr::InvalidRequest(
                    "continuation requires internal response input".to_string(),
                );
                session.reject_input_turn_admission(
                    &submission_id,
                    CodexErr::InvalidRequest(error.to_string()),
                );
                return Err(error);
            }
            start_if_idle(
                session,
                request,
                submission_id.clone(),
                TurnStartKind::Recovery,
                Some(expected_previous_turn_id),
            )
            .await
        }
        TurnInputMode::Steer { expected_turn_id } => {
            steer(session, request, expected_turn_id, submission_id.clone()).await
        }
    };
    match &result {
        Ok(TurnInputSubmission::NotSubmitted { reason }) => session.reject_input_turn_admission(
            &submission_id,
            CodexErr::InvalidRequest(format!("turn input was not submitted: {reason:?}")),
        ),
        Err(error) => session.reject_input_turn_admission(
            &submission_id,
            CodexErr::InvalidRequest(error.to_string()),
        ),
        Ok(TurnInputSubmission::Started { .. } | TurnInputSubmission::Steered { .. }) => {}
    }
    result
}

pub(super) async fn handle_recovery(
    session: &Arc<Session>,
    thread_settings: ThreadSettingsOverrides,
    start_options: TurnStartOptions,
    submission_id: String,
) -> CodexResult<TurnInputSubmission> {
    let request = TurnInputRequest::user_input(Vec::new())
        .with_thread_settings(thread_settings)
        .on_start(TurnStartOptions {
            turn_trigger: Some("retry".to_string()),
            ..start_options
        });
    start_if_idle(
        session,
        request,
        submission_id,
        TurnStartKind::Recovery,
        /*expected_previous_turn_id*/ None,
    )
    .await
}

async fn start_or_steer(
    session: &Arc<Session>,
    request: TurnInputRequest,
    submission_id: String,
) -> CodexResult<TurnInputSubmission> {
    let TurnInputRequest {
        mut input,
        thread_settings,
        start,
        additional_context,
        responsesapi_client_metadata,
        ..
    } = request;
    let has_explicit_input = match &input {
        SubmittedTurnInput::UserInput { content, .. }
        | SubmittedTurnInput::AgentInput { content, .. } => !content.is_empty(),
        SubmittedTurnInput::ResponseItem(ResponseItem::FunctionCallOutput {
            call_id: None,
            ..
        }) => true,
        _ => {
            return Err(CodexErr::InvalidRequest(
                "only user or agent input, or standalone function-call outputs, can start or \
                 steer a turn"
                    .to_string(),
            ));
        }
    };
    let settings = PreparedTurnInputSettings::prepare(session, thread_settings, start).await?;
    match session
        .steer_input(
            &mut input,
            additional_context.clone(),
            /*expected_turn_id*/ None,
            settings.required_active_final_output_json_schema(),
            responsesapi_client_metadata.clone(),
        )
        .await
    {
        Ok(resolution) => {
            settings
                .apply_steered(session, submission_id.clone())
                .await?;
            let turn_id = resolution.target_turn_id.clone();
            session.resolve_input_turn_admission(&submission_id, resolution);
            Ok(TurnInputSubmission::Steered { turn_id })
        }
        Err(NotSubmittedReason::NoActiveTurn) => {
            // MAv1 sends explicit input to spawned agents as part of an existing
            // parent's work. Client RPCs are gated separately by the host.
            let is_delegated_input = settings.start_options.parent_turn_id.is_some()
                && matches!(
                    session
                        .state
                        .lock()
                        .await
                        .session_configuration
                        .session_source,
                    SessionSource::SubAgent(SubAgentSource::ThreadSpawn { .. })
                );
            let _admission = if is_delegated_input {
                None
            } else {
                let Some(admission) = session.services.extensions.admit_turn_start() else {
                    return Ok(TurnInputSubmission::NotSubmitted {
                        reason: NotSubmittedReason::ServerDraining,
                    });
                };
                Some(admission)
            };
            let Some(turn_context) = settings
                .apply_started(session, submission_id.clone(), TurnStartKind::User)
                .await?
            else {
                unreachable!("explicit user input can enter Plan mode");
            };
            if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
                turn_context
                    .turn_metadata_state
                    .set_responsesapi_client_metadata(responsesapi_client_metadata);
            }
            session
                .maybe_emit_model_warnings_for_turn(turn_context.as_ref())
                .await;
            if let SubmittedTurnInput::UserInput { content, .. } = &input {
                turn_context.session_telemetry.user_prompt(content);
            }
            let resolution =
                session.capture_input_turn_admission_resolution(turn_context.sub_id.clone());
            session.resolve_input_turn_admission(&submission_id, resolution);
            let mut task_input = merge_additional_context_input(session, additional_context).await;
            if has_explicit_input {
                task_input.push(pending_turn_input(session, input, &turn_context.sub_id).await);
            }
            session
                .spawn_task(turn_context, task_input, RegularTask::new())
                .await;
            Ok(TurnInputSubmission::Started {
                turn_id: submission_id,
            })
        }
        Err(reason) => Ok(TurnInputSubmission::NotSubmitted { reason }),
    }
}

async fn start_if_idle(
    session: &Arc<Session>,
    request: TurnInputRequest,
    submission_id: String,
    kind: TurnStartKind,
    expected_previous_turn_id: Option<String>,
) -> CodexResult<TurnInputSubmission> {
    start_if_idle_with_lease(
        session,
        request,
        IdleStart {
            submission_id,
            kind,
            expected_previous_turn_id,
            context: IdleContext::Ordinary,
        },
        (),
        |_| {},
    )
    .await
}

impl Session {
    pub(crate) async fn start_turn_if_idle_with_lease(
        self: &Arc<Self>,
        request: TurnInputRequest,
        lease: impl Send,
        on_admitted: impl FnOnce(&str) + Send,
    ) -> CodexResult<TurnInputSubmission> {
        let kind = match &request.input {
            SubmittedTurnInput::UserInput { content, .. }
            | SubmittedTurnInput::AgentInput { content, .. } => {
                if content.is_empty() {
                    TurnStartKind::Automatic
                } else {
                    TurnStartKind::User
                }
            }
            SubmittedTurnInput::ResponseItem(_)
            | SubmittedTurnInput::InterAgentCommunication(_) => TurnStartKind::Automatic,
        };
        start_if_idle_with_lease(
            self,
            request,
            IdleStart {
                submission_id: self.next_internal_sub_id(),
                kind,
                expected_previous_turn_id: None,
                context: IdleContext::Ordinary,
            },
            lease,
            on_admitted,
        )
        .await
    }

    pub(super) async fn start_mailbox_inventory_with_lease(
        self: &Arc<Self>,
        notification: codex_thread_store::MailboxInventoryNotification,
        lease: impl Send,
    ) -> CodexResult<super::mailbox_inventory::MailboxInventoryAdmission> {
        use super::mailbox_inventory::MailboxInventoryAdmission;
        let result = start_if_idle_with_lease(
            self,
            TurnInputRequest::user_input(Vec::new()),
            IdleStart {
                submission_id: notification.id.clone(),
                kind: TurnStartKind::Automatic,
                expected_previous_turn_id: None,
                context: IdleContext::Mailbox(notification),
            },
            lease,
            |_| {},
        )
        .await?;
        Ok(match result {
            TurnInputSubmission::Started { .. } => MailboxInventoryAdmission::Started,
            TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::Superseded,
            } => MailboxInventoryAdmission::Retry,
            TurnInputSubmission::NotSubmitted { .. } | TurnInputSubmission::Steered { .. } => {
                MailboxInventoryAdmission::Deferred
            }
        })
    }
}

#[expect(
    clippy::await_holding_invalid_type,
    reason = "the previous turn check and idle reservation must be atomic"
)]
async fn start_if_idle_with_lease(
    session: &Arc<Session>,
    request: TurnInputRequest,
    start: IdleStart,
    lease: impl Send,
    on_admitted: impl FnOnce(&str) + Send,
) -> CodexResult<TurnInputSubmission> {
    let IdleStart {
        submission_id,
        kind,
        expected_previous_turn_id,
        context: idle_context,
    } = start;
    let TurnInputRequest {
        input,
        thread_settings,
        start,
        additional_context,
        responsesapi_client_metadata,
        ..
    } = request;
    if session.input_queue.has_trigger_turn_mailbox_items().await {
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PendingTriggerTurn,
        });
    }
    // Preserve current-Plan rejection before reservation and settings errors.
    // The commit-time decision also checks the proposed mode.
    if kind == TurnStartKind::Automatic
        && !kind.permits_mode(session.collaboration_mode().await.mode)
    {
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PlanMode,
        });
    }

    let _admission = session.services.extensions.admit_turn_start();
    // A one-shot review delegate completes its already-running parent's work.
    // Its explicit input carries parent lineage; automatic starts do not qualify.
    if _admission.is_none()
        && !(kind == TurnStartKind::User
            && start.parent_turn_id.is_some()
            && matches!(
                session
                    .state
                    .lock()
                    .await
                    .session_configuration
                    .session_source,
                SessionSource::SubAgent(SubAgentSource::Review)
            ))
    {
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::ServerDraining,
        });
    }

    let observation = if kind == TurnStartKind::Automatic {
        Some(
            session
                .services
                .agent_control
                .acquire_response_observation_transaction(session.presentation_id())
                .await,
        )
    } else {
        None
    };
    let shell_wake_reservation = if kind == TurnStartKind::Automatic {
        Some(
            session
                .services
                .unified_exec_manager
                .acquire_user_shell_wake_reservation_permit()
                .await,
        )
    } else {
        None
    };
    if kind == TurnStartKind::Automatic
        && (session
            .services
            .agent_control
            .has_bound_final_response_wake(session.presentation_id())
            || session
                .services
                .unified_exec_manager
                .has_pending_user_shell_completion_wake()
                .await
            || session.input_queue.has_queued_turn_trigger().await)
    {
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PendingTriggerTurn,
        });
    }
    let turn_state = {
        let mut active_turn = session.active_turn.lock().await;
        if active_turn.is_some() {
            return Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::NotIdle,
            });
        }
        if let Some(expected) = expected_previous_turn_id
            && session.state.lock().await.last_started_turn_id.as_ref() != Some(&expected)
        {
            return Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::Superseded,
            });
        }
        let active_turn = active_turn.get_or_insert_with(ActiveTurn::default);
        Arc::clone(&active_turn.turn_state)
    };
    drop(shell_wake_reservation);
    drop(observation);
    // The reserved turn now owns admission. Lifecycle callbacks may reacquire the goal lease.
    drop(lease);

    if session.input_queue.has_trigger_turn_mailbox_items().await {
        session.clear_reserved_idle_turn(&turn_state).await;
        session.maybe_start_turn_for_pending_work().await;
        return Ok(TurnInputSubmission::NotSubmitted {
            reason: NotSubmittedReason::PendingTriggerTurn,
        });
    }

    let settings = match PreparedTurnInputSettings::prepare(session, thread_settings, start).await {
        Ok(settings) => settings,
        Err(error) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Err(error);
        }
    };
    let turn_context = match settings
        .apply_started(session, submission_id.clone(), kind)
        .await
    {
        Ok(Some(turn_context)) => turn_context,
        Ok(None) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::PlanMode,
            });
        }
        Err(error) => {
            session.clear_reserved_idle_turn(&turn_state).await;
            return Err(error);
        }
    };
    if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
        turn_context
            .turn_metadata_state
            .set_responsesapi_client_metadata(responsesapi_client_metadata);
    }
    session
        .maybe_emit_model_warnings_for_turn(turn_context.as_ref())
        .await;

    // Inventory shares this reservation and all ordinary/goal priority checks. Its canonical
    // publication stays in the mailbox owner, before any automatic sampling can start.
    if let IdleContext::Mailbox(notification) = idle_context
        && let Some(reason) = session
            .record_reserved_mailbox_inventory(&turn_context, &turn_state, notification)
            .await?
    {
        return Ok(TurnInputSubmission::NotSubmitted { reason });
    }

    let mut task_input = merge_additional_context_input(session, additional_context).await;
    match kind {
        TurnStartKind::User => {
            session.clear_connector_selection().await;
            if let SubmittedTurnInput::UserInput { content, .. } = &input {
                turn_context.session_telemetry.user_prompt(content);
            }
            task_input.push(pending_turn_input(session, input, &turn_context.sub_id).await);
        }
        TurnStartKind::Automatic | TurnStartKind::Recovery => {
            // Empty automatic user input resumes sampling without a new message.
            if !matches!(&input, SubmittedTurnInput::UserInput { .. }) {
                session
                    .input_queue
                    .extend_pending_input_for_turn_state(
                        turn_state.as_ref(),
                        vec![pending_turn_input(session, input, &turn_context.sub_id).await],
                    )
                    .await;
            }
        }
    }
    on_admitted(&submission_id);
    let resolution = session.capture_input_turn_admission_resolution(turn_context.sub_id.clone());
    session.resolve_input_turn_admission(&submission_id, resolution);
    session
        .start_task(turn_context, task_input, RegularTask::new())
        .await;
    Ok(TurnInputSubmission::Started {
        turn_id: submission_id,
    })
}

async fn steer(
    session: &Arc<Session>,
    request: TurnInputRequest,
    expected_turn_id: String,
    submission_id: String,
) -> CodexResult<TurnInputSubmission> {
    let TurnInputRequest {
        mut input,
        thread_settings,
        start,
        additional_context,
        responsesapi_client_metadata,
        ..
    } = request;
    if !matches!(
        &input,
        SubmittedTurnInput::UserInput { .. } | SubmittedTurnInput::AgentInput { .. }
    ) {
        return Err(CodexErr::InvalidRequest(
            "only user or agent input can steer a turn".to_string(),
        ));
    }
    let settings = PreparedTurnInputSettings::prepare(session, thread_settings, start).await?;
    match session
        .steer_input(
            &mut input,
            additional_context,
            Some(expected_turn_id.as_str()),
            settings.required_active_final_output_json_schema(),
            responsesapi_client_metadata,
        )
        .await
    {
        Ok(resolution) => {
            settings
                .apply_steered(session, submission_id.clone())
                .await?;
            let turn_id = resolution.target_turn_id.clone();
            session.resolve_input_turn_admission(&submission_id, resolution);
            Ok(TurnInputSubmission::Steered { turn_id })
        }
        Err(reason) => Ok(TurnInputSubmission::NotSubmitted { reason }),
    }
}

impl Session {
    /// Called under the active-turn lock before running any task or lifecycle callback.
    pub(crate) async fn record_started_turn(&self, turn_id: &str) {
        self.state.lock().await.last_started_turn_id = Some(turn_id.to_string());
    }

    pub(crate) async fn route_realtime_text_input(
        self: &Arc<Self>,
        text: String,
    ) -> Result<(), &'static str> {
        let submission_id = Uuid::now_v7().to_string();
        let submission = handle(
            self,
            TurnInputRequest::user_input(vec![UserInput::Text {
                text,
                text_elements: Vec::new(),
            }])
            .on_start(TurnStartOptions {
                turn_trigger: Some("realtime".to_string()),
                ..Default::default()
            }),
            TurnInputMode::StartOrSteer,
            submission_id.clone(),
        )
        .await;
        match submission {
            Ok(TurnInputSubmission::Started { .. } | TurnInputSubmission::Steered { .. }) => {}
            Ok(TurnInputSubmission::NotSubmitted {
                reason: NotSubmittedReason::ServerDraining,
            }) => {
                return Err("Server is draining; retry the turn after reconnecting");
            }
            Ok(TurnInputSubmission::NotSubmitted { reason }) => {
                self.send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(ErrorEvent {
                        misalignment: None,
                        message: format!("failed to submit turn input: {reason:?}"),
                        codex_error_info: Some(CodexErrorInfo::BadRequest),
                    }),
                })
                .await;
            }
            Err(error) => {
                self.send_event_raw(Event {
                    id: submission_id,
                    msg: EventMsg::Error(error.to_error_event(/*message_prefix*/ None)),
                })
                .await;
            }
        }
        Ok(())
    }

    pub(super) async fn clear_reserved_idle_turn(
        &self,
        turn_state: &Arc<tokio::sync::Mutex<TurnState>>,
    ) {
        let mut active_turn_guard = self.active_turn.lock().await;
        if let Some(active_turn) = active_turn_guard.as_ref()
            && active_turn.task.is_none()
            && Arc::ptr_eq(&active_turn.turn_state, turn_state)
        {
            *active_turn_guard = None;
            self.active_turn_transition.notify_waiters();
        }
    }

    /// Inject additional user input or a standalone tool output into the active turn.
    ///
    /// Returns the active turn id when accepted.
    #[expect(
        clippy::await_holding_invalid_type,
        reason = "active turn checks and turn state updates must remain atomic"
    )]
    async fn steer_input(
        &self,
        input: &mut SubmittedTurnInput,
        additional_context: BTreeMap<String, AdditionalContextEntry>,
        expected_turn_id: Option<&str>,
        required_final_output_json_schema: Option<&Value>,
        responsesapi_client_metadata: Option<HashMap<String, String>>,
    ) -> Result<super::InputTurnAdmissionResolution, NotSubmittedReason> {
        let mut active = self.active_turn.lock().await;
        let Some(active_turn) = active.as_mut() else {
            return Err(NotSubmittedReason::NoActiveTurn);
        };

        let Some(active_task) = active_turn.task.as_ref() else {
            return Err(NotSubmittedReason::NoActiveTurn);
        };
        let active_turn_id = &active_task.turn_context.sub_id;

        if let Some(expected_turn_id) = expected_turn_id
            && expected_turn_id != active_turn_id
        {
            return Err(NotSubmittedReason::ExpectedTurnMismatch {
                expected: expected_turn_id.to_string(),
                actual: active_turn_id.clone(),
            });
        }

        match active_task.kind {
            crate::state::TaskKind::Regular => {}
            crate::state::TaskKind::Review => {
                return Err(NotSubmittedReason::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Review,
                });
            }
            crate::state::TaskKind::Compact => {
                return Err(NotSubmittedReason::ActiveTurnNotSteerable {
                    turn_kind: NonSteerableTurnKind::Compact,
                });
            }
        }

        if matches!(
            input,
            SubmittedTurnInput::UserInput { content, .. }
                | SubmittedTurnInput::AgentInput { content, .. }
                if content.is_empty()
        ) {
            return Err(NotSubmittedReason::EmptyInput);
        }
        // Compare JSON values directly instead of serialized schema text.
        // Value equality ignores object key order while preserving array and
        // scalar distinctions; broader JSON Schema equivalence is out of scope.
        if let Some(required_schema) = required_final_output_json_schema
            && active_task.turn_context.final_output_json_schema.as_ref() != Some(required_schema)
        {
            return Err(NotSubmittedReason::ActiveTurnOutputSchemaMismatch);
        }
        let mut pending_input = merge_additional_context_input(self, additional_context).await;

        if let Some(responsesapi_client_metadata) = responsesapi_client_metadata {
            active_task
                .turn_context
                .turn_metadata_state
                .set_responsesapi_client_metadata(responsesapi_client_metadata);
        }

        let input = match input {
            SubmittedTurnInput::UserInput { content, client_id } => {
                active_task
                    .turn_context
                    .session_telemetry
                    .user_prompt(content);
                TurnInput::UserInput {
                    content: std::mem::take(content),
                    client_id: client_id.clone(),
                    acceptance_order: self.reserve_user_input_order().await,
                }
            }
            input => pending_turn_input(self, input.clone(), active_turn_id).await,
        };
        pending_input.push(input);
        self.input_queue
            .extend_pending_input_and_accept_mailbox_delivery_for_turn_state(
                active_turn.turn_state.as_ref(),
                pending_input,
            )
            .await;
        Ok(self.capture_input_turn_admission_resolution(active_turn_id.clone()))
    }
}

async fn merge_additional_context_input(
    session: &Session,
    additional_context: BTreeMap<String, AdditionalContextEntry>,
) -> Vec<TurnInput> {
    let additional_context_input = {
        let mut state = session.state.lock().await;
        state.additional_context.merge(additional_context)
    };
    additional_context_input
        .into_iter()
        .map(|item| session.annotate_client_response_item(item))
        .map(TurnInput::ResponseItem)
        .collect()
}

async fn pending_turn_input(
    session: &Session,
    input: SubmittedTurnInput,
    turn_id: &str,
) -> TurnInput {
    match input {
        SubmittedTurnInput::UserInput { content, client_id } => TurnInput::UserInput {
            content,
            client_id,
            acceptance_order: session.reserve_user_input_order().await,
        },
        SubmittedTurnInput::AgentInput {
            content,
            presentation,
        } => TurnInput::AgentInput {
            content,
            presentation,
        },
        SubmittedTurnInput::ResponseItem(mut item)
            if matches!(
                &item,
                ResponseItem::FunctionCallOutput { call_id: None, .. }
            ) =>
        {
            Session::assign_missing_response_item_id(&mut item);
            let metadata = if session.guardian_context_mode == GuardianContextMode::ThreadOwned
                && let Some(messages) = session
                    .services
                    .agent_control
                    .capture_sender_user_messages(&item, session.thread_id, turn_id)
                    .await
            {
                Some(CodexHarnessMetadata {
                    user_input_order: session.reserve_user_input_order().await,
                    sender_user_messages: Some(Box::new(messages)),
                    ..Default::default()
                })
            } else {
                None
            };
            TurnInput::FunctionCallOutput(ResponseItemEnvelope { item, metadata })
        }
        SubmittedTurnInput::ResponseItem(item) => TurnInput::ResponseItem(item.into()),
        SubmittedTurnInput::InterAgentCommunication(communication) => {
            TurnInput::InterAgentCommunication(communication)
        }
    }
}
